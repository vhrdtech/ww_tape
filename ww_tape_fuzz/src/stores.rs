//! A chain on a store of tapes, driven by operations and checked against a model.
//!
//! The operations: chunks written, cuts, crashes and resumes, closes, removed pieces and dropped fronts.
//! The model knows the chain's pieces and the chunks each holds. After an operation that changes pieces, and at the
//! end, every tape of the store is read back by a [TapeReader] and its pieces, their chunks and which are closed
//! must be the model's. [MemPieces] and a store of files (`ww_tape_fs::FilePieces`, in the integration tests) run the
//! same operations, so both keep the same contract.
//!
//! Input: a mode byte (bit 0 a chain in one tape, else a tape per piece; bit 1 aligned), then one byte per
//! operation: 0 to 3 write a chunk of track `byte >> 7`, 4 cuts, 5 crashes and resumes, 6 closes (the next write
//! resumes), 7 takes the next byte as a piece index and removes that piece's tape (a tape per piece) or drops the
//! tape's front to that piece (one tape). A crash or a close after a removal is skipped: a chain with removed pieces
//! is not resumed.

use std::collections::BTreeSet;

use ww_tape::chunk::ChunkHead;
use ww_tape::{
    ByteSource, ChainOptions, Codec, ColumnKind, ColumnSpec, CutPolicy, Definitions, FileBlocks,
    MemError, MemPiece, MemPieces, OwnerId, PieceChain, PieceStore, PieceTapes, TapeReader,
    TrackNum,
};

use crate::targets::{Defined, chunk_of, options, template_definitions};

/// Operations of one run at most.
pub const OPS_MAX: usize = 256;

/// A store the model drives: the [PieceStore] a chain writes on, and its tapes listed, read and removed.
pub trait ModelStore {
    type Store: PieceStore<Key = u64>;

    /// The store, for a chain to create or resume on: every one shares the same bytes.
    fn piece_store(&self) -> Self::Store;

    /// Every key with a tape, in order.
    fn keys(&self) -> Vec<u64>;

    /// The bytes of the tape under `key`, from the first byte kept, and where that byte is in the tape.
    fn bytes(&self, key: u64) -> Option<(u64, Vec<u8>)>;

    /// Remove the closed tape under `key`: whether it was there.
    fn remove(&mut self, key: u64) -> bool;

    /// Free the pieces of the tape under `key` before `to` ([MemPieces::drop_front]): the first byte kept, None when
    /// the store can't.
    fn drop_front(&mut self, key: u64, to: u64) -> Option<u64>;
}

impl ModelStore for MemPieces {
    type Store = Self;

    fn piece_store(&self) -> Self {
        self.clone()
    }

    fn keys(&self) -> Vec<u64> {
        Self::keys(self).expect("memory")
    }

    fn bytes(&self, key: u64) -> Option<(u64, Vec<u8>)> {
        let tape: MemPiece = self.get(&key).expect("memory")?;
        Some((
            tape.start().expect("memory"),
            tape.to_vec().expect("memory"),
        ))
    }

    fn remove(&mut self, key: u64) -> bool {
        Self::remove(self, &key).expect("a closed tape")
    }

    fn drop_front(&mut self, key: u64, to: u64) -> Option<u64> {
        Self::drop_front(self, &key, to).expect("memory")
    }
}

/// A piece as the model or a reader sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Piece {
    pub closed: bool,
    /// (track, chunk number) of each chunk, in the order written
    pub chunks: Vec<(u64, u64)>,
}

/// What a run left: the pieces a reader finds at the end, and how many pieces it removed or dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub pieces: Vec<Piece>,
    pub gone: usize,
}

/// Run the operations of `data` on `store`, checking the model after each one that changes pieces.
pub fn run<T: ModelStore>(store: T, data: &[u8]) -> Outcome {
    let Some((&mode, ops)) = data.split_first() else {
        return Outcome::default();
    };
    let one = mode & 1 != 0;
    let options = ChainOptions {
        tapes: if one {
            PieceTapes::One
        } else {
            PieceTapes::Each
        },
        writer: options(mode & 2 != 0),
        cut: CutPolicy {
            chunks: exact_units::Count::new(u64::MAX),
            ..CutPolicy::default()
        },
    };
    let (defs, on) = template_definitions();
    let mut m = Model {
        store,
        options,
        chain: None,
        pieces: vec![Piece {
            closed: false,
            chunks: Vec::new(),
        }],
        gone: BTreeSet::new(),
        next: [0; 2],
        one,
    };
    m.chain = Some(
        PieceChain::create_on(m.store.piece_store(), OWNER, 0, &[], m.options.clone())
            .expect("a chain starts"),
    );
    let mut ops = ops.iter().take(OPS_MAX);
    while let Some(&op) = ops.next() {
        match op & 7 {
            0..=3 => m.write(&defs, on, u64::from(op >> 7)),
            4 => {
                m.cut(&defs);
                m.check();
            }
            5 if m.gone.is_empty() => {
                m.crash();
                m.check();
            }
            6 if m.gone.is_empty() => {
                m.close();
                m.check();
            }
            7 => {
                let at = ops.next().copied().unwrap_or(0);
                m.remove(at);
                m.check();
            }
            _ => {}
        }
    }
    if m.gone.is_empty() {
        m.close();
    }
    Outcome {
        pieces: m.check(),
        gone: m.gone.len(),
    }
}

const OWNER: OwnerId = OwnerId([7; 32]);

struct Model<T: ModelStore> {
    store: T,
    options: ChainOptions,
    /// None after a close, until the next write resumes
    chain: Option<PieceChain<T::Store>>,
    /// Every piece the chain started, removed ones included
    pieces: Vec<Piece>,
    /// Pieces removed or dropped
    gone: BTreeSet<usize>,
    next: [u64; 2],
    one: bool,
}

impl<T: ModelStore> Model<T> {
    /// The chain, resumed after a close; None after a close and a removal.
    fn chain(&mut self) -> Option<&mut PieceChain<T::Store>> {
        if self.chain.is_none() && self.gone.is_empty() {
            let (c, _) =
                PieceChain::resume_on(self.store.piece_store(), OWNER, 0, self.options.clone())
                    .expect("a closed chain resumes");
            // the last piece is written on after its close record
            self.pieces.last_mut().expect("a piece").closed = false;
            self.chain = Some(c);
        }
        self.chain.as_mut()
    }

    fn write(&mut self, defs: &Definitions, on: Defined, t: u64) {
        let k = self.next[t as usize];
        let (h, b) = chunk_of(t, on.chunks[t as usize], k);
        let spec = ColumnSpec {
            kind: ColumnKind::Starts,
            codec: Codec::RAW,
            param: 0,
            len: b.len(),
        };
        let Some(c) = self.chain() else {
            return;
        };
        c.write_chunk(defs, &h, &[(spec, &b)])
            .expect("a chunk writes");
        self.next[t as usize] += 1;
        self.pieces.last_mut().expect("a piece").chunks.push((t, k));
    }

    fn cut(&mut self, defs: &Definitions) {
        let Some(c) = self.chain() else {
            return;
        };
        c.cut_now(defs).expect("a cut");
        self.pieces.last_mut().expect("a piece").closed = true;
        self.pieces.push(Piece {
            closed: false,
            chunks: Vec::new(),
        });
    }

    fn crash(&mut self) {
        let Some(mut c) = self.chain.take() else {
            return;
        };
        c.writer()
            .expect("a chain writing")
            .flush_deferred()
            .expect("deferred blocks write");
        // the open piece is left as it is
        drop(c);
        let (c, _) =
            PieceChain::resume_on(self.store.piece_store(), OWNER, 0, self.options.clone())
                .expect("a crashed chain resumes");
        self.chain = Some(c);
    }

    fn close(&mut self) {
        if let Some(c) = self.chain.take() {
            c.close().expect("a chain closes");
            self.pieces.last_mut().expect("a piece").closed = true;
        }
    }

    /// Remove piece `at` (modulo the closed pieces kept), or drop the tape's front to it.
    fn remove(&mut self, at: u8) {
        let kept: Vec<usize> = (0..self.pieces.len())
            .filter(|i| !self.gone.contains(i))
            .collect();
        if kept.is_empty() {
            return;
        }
        let i = kept[usize::from(at) % kept.len()];
        if self.one {
            let found = self.found();
            let Some(start) = found.iter().position(|(n, _)| *n == i).map(|p| found[p].1) else {
                return;
            };
            if self.store.drop_front(0, start).is_some() {
                self.gone.extend(0..i);
            }
        } else if self.pieces[i].closed {
            assert!(self.store.remove(i as u64), "piece {i}'s tape is there");
            self.gone.insert(i);
        }
    }

    /// The model's piece number and the start of each piece kept, as a reader of the one tape finds them.
    fn found(&self) -> Vec<(usize, u64)> {
        let first = (0..self.pieces.len())
            .find(|i| !self.gone.contains(i))
            .unwrap_or(0);
        read(&self.store, 0)
            .into_iter()
            .enumerate()
            .map(|(n, (start, _))| (first + n, start))
            .collect()
    }

    /// The pieces a reader finds in every tape against the model's.
    fn check(&self) -> Vec<Piece> {
        let found: Vec<Piece> = self
            .store
            .keys()
            .into_iter()
            .flat_map(|k| read(&self.store, k).into_iter().map(|(_, p)| p))
            .collect();
        let want: Vec<Piece> = self
            .pieces
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.gone.contains(i))
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(found, want, "the pieces in the store");
        found
    }
}

/// The pieces of the tape under `key` with their starts.
fn read<T: ModelStore>(store: &T, key: u64) -> Vec<(u64, Piece)> {
    let Some((front, bytes)) = store.bytes(key) else {
        return Vec::new();
    };
    let src = Kept { front, bytes };
    let r = TapeReader::open(&src).expect("a tape opens");
    r.pieces()
        .iter()
        .map(|p| {
            let from = (p.start - front) as usize;
            let to = (p.end - front) as usize;
            let mut chunks = Vec::new();
            let mut blocks = FileBlocks::new(&src.bytes[from..to]);
            while let Some(b) = blocks.next_block() {
                let b = b.expect("good bytes");
                if let Some(c) = b.chunk() {
                    chunks.push(chunk_number(&c.head()));
                }
            }
            (
                p.start,
                Piece {
                    closed: p.is_closed(),
                    chunks,
                },
            )
        })
        .collect()
}

/// (track, chunk number) of a chunk [chunk_of] wrote.
fn chunk_number(head: &ChunkHead) -> (u64, u64) {
    let t = (0..2u64)
        .find(|&t| chunk_of(t, TrackNum::new(1), 0).0.track_id == head.track_id)
        .expect("a chunk of the model's tracks");
    (t, head.first_item.get() / 16)
}

/// Bytes kept from `front` on, read at their offsets in the tape.
struct Kept {
    front: u64,
    bytes: Vec<u8>,
}

impl ByteSource for Kept {
    type Error = MemError;

    fn size(&self) -> Result<u64, MemError> {
        Ok(self.front + self.bytes.len() as u64)
    }

    fn start(&self) -> Result<u64, MemError> {
        Ok(self.front)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemError> {
        let from = offset
            .checked_sub(self.front)
            .ok_or(MemError::Dropped { front: self.front })? as usize;
        let src = self
            .bytes
            .get(from..from + buf.len())
            .ok_or(MemError::OutOfRange)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}
