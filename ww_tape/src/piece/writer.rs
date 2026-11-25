//! [PieceChain]: a recording written as a chain of pieces, cut at chunk boundaries, on files or any store of
//! [PieceStore]: each piece a tape of its own, or all of them back to back in one tape ([PieceTapes]).

use crate::TrackNum;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::time::Duration;
use exact_units::units::Chunk;
use exact_units::{Bytes, Count};

use super::{
    CloseReason, OwnerId, PieceHead, PieceId, PieceIdent, Removed, RemovedPiece, TAIL_LEN,
    TrackStart, UnitHash,
};
use crate::index::{IndexEntry, IndexError, TrackIdent};
use crate::reader::{ByteSource, TapeReader};
use crate::sink::{PieceStore, TapeSink, sink_err};
use crate::writer::kept::Kept;
use crate::writer::resume::{SinkSource, refuse_headless};
use crate::writer::{
    Closed, Newest, NextPiece, Placed, ResumeError, Resumed, TapeWriter, WriteError, WriterOptions,
    block_id,
};
use crate::{ChunkHead, ColumnSpec, Definitions, HEADER_LEN, SessionTag};

/// When a [PieceChain] cuts to a new piece; checked before each chunk. Zero or None turns a limit off.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct CutPolicy {
    /// Once a piece holds this many bytes.
    pub bytes: Bytes<u64>,
    /// Once a piece holds this many chunk blocks.
    pub chunks: Count<Chunk, u64>,
    /// Once a piece is this old (since it was started or resumed).
    pub interval: Option<Duration>,
}

/// What a piece may hold past [CutPolicy::bytes]: the chunk that takes it over the limit, the index, close and tail.
pub const CUT_SLACK: Bytes<u64> = Bytes::new(1 << 20);

/// Refuse a resumed piece of a chain that kept blocks but no piece head: its head is damaged, and a piece without
/// one has no link to the chain and can't be cut. A piece cut off inside its head keeps nothing and starts again.
fn headed<S: TapeSink>(writer: &TapeWriter<S>) -> Result<(), WriteError<S::Error>> {
    if writer.bytes() != 0 && !writer.has_head() {
        return Err(WriteError::Index(IndexError::Invalid(
            "the last piece of the chain has no readable piece head",
        )));
    }
    Ok(())
}

/// Ask a new piece's sink to make room for a piece cut by `cut`, see [TapeSink::reserve]: a memory sink then never
/// grows by doubling under its write lock while readers read.
fn reserve_piece<S: TapeSink>(writer: &mut TapeWriter<S>, cut: CutPolicy) {
    if cut.bytes.get() > 0 {
        writer.reserve(cut.bytes.saturating_add(CUT_SLACK).get());
    }
}

/// Where [PieceChain::write_chunk] placed the chunk: the seq of the piece it landed in ([PieceChain::seq] by the
/// time the call returns, past a cut when one happened) and where in that piece.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChunkPlaced {
    pub seq: u64,
    pub offset: u64,
    pub len: u32,
    /// The chunk's unit hash as the writer states it ([Placed::unit]).
    pub unit: UnitHash,
}

/// How a [PieceChain] keeps its pieces in its [PieceStore].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PieceTapes {
    /// Each piece a tape of its own, piece `seq` under [PieceStore::key_of] (`seq`): a file per piece in a directory.
    #[default]
    Each,
    /// All pieces back to back in one tape, under [PieceStore::key_of] (0): one file. A cut starts the next piece right
    /// behind the last ([TapeWriter::cut_in_place]); a resume goes on in the last piece when it is unclosed, else
    /// starts a new one behind it.
    One,
}

/// A closed piece of a [PieceChain]: the key of the tape it is in, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainPiece<K> {
    /// The tape: the piece's own, or the one holding every piece ([Closed::start] says where it starts there).
    pub tape: K,
    pub piece: Closed,
}

/// Writes one recording as pieces on the tapes of a [PieceStore] (files by default), each piece a tape of its
/// own or all in one ([PieceTapes]), cutting by a [CutPolicy] before a chunk.
pub struct PieceChain<T: PieceStore> {
    store: T,
    pieces: PieceTapes,
    /// None after a failed cut: the chain takes no more blocks
    writer: Option<TapeWriter<T::Sink>>,
    cut: CutPolicy,
    /// When the current piece was started or resumed, by its writer's clock
    started: Option<u64>,
    seq: u64,
    closed: Vec<ChainPiece<T::Key>>,
}

/// Options of a new [PieceChain].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ChainOptions {
    /// Each piece's writer.
    pub writer: WriterOptions,
    pub cut: CutPolicy,
    /// A tape per piece, or all pieces in one.
    pub tapes: PieceTapes,
}

impl<T: PieceStore> PieceChain<T> {
    /// Start chain `chain` of `owner` in `store` with its first piece, id [PieceId::first_of] `(owner, chain)`;
    /// `tracks` say where items of tracks carried over from elsewhere continue (empty for a new recording). Each
    /// chain of a owner takes its own number, so its pieces and session tag are its own. With [PieceTapes::One] the
    /// tape that holds every piece is made with [PieceStore::create_whole].
    #[expect(
        clippy::needless_pass_by_value,
        reason = "a chain's constructors take their options by value, as TapeWriter::resume_on does"
    )]
    pub fn create_on(
        mut store: T,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
    ) -> Result<Self, WriteError<T::Error>> {
        let first = store.key_of(0);
        let sink = match options.tapes {
            PieceTapes::Each => store
                .create(&first)
                .map_err(sink_err("creating the first piece"))?,
            PieceTapes::One => store
                .create_whole(&first)
                .map_err(sink_err("creating the tape of the chain"))?,
        };
        let mut writer = TapeWriter::create_on(sink, options.writer.sync);
        writer.apply(&options.writer)?;
        reserve_piece(&mut writer, options.cut);
        writer.begin_piece(&first_head(owner, chain, options.writer.integrity), tracks)?;
        Ok(Self {
            store,
            pieces: options.tapes,
            writer: Some(writer),
            cut: options.cut,
            started: options.writer.clock.now_ms(),
            seq: 0,
            closed: Vec::new(),
        })
    }

    /// Reopen chain `chain` of `owner` in `store` to write on, after a crash or after its close, from the lowest piece
    /// there ([PieceStore::first_from]): the pieces before it were deleted (a head cut). A hole between pieces is walked
    /// over when the closes at hand name its pieces as removed on purpose and link both ends, as
    /// [ChainLink::Removed](super::ChainLink::Removed); any other is refused ([ResumeError::Gap]). The pieces closed by
    /// a cut are read from their tails ([Self::closed]), the last piece is resumed ([TapeWriter::resume_on]): an
    /// unclosed one goes on after its last good block, a closed one is written on after its last close record. After
    /// a crash between a cut's close and the next piece's start, the next piece is started as the cut would have.
    /// `options` are the ones the chain was written with. A piece cut whole that a crash left unmarked
    /// ([ResumeError::Continued]) is marked and read as closed.
    ///
    /// With [PieceTapes::One] the chain is in one tape, and its last piece is resumed with the same rules as a piece
    /// of its own: only a torn end is cut, and a resume that would drop more is refused. An application that starts
    /// every write session in a piece of its own gives the calls a resume asks for again, then cuts
    /// ([Self::cut_now]).
    #[expect(
        clippy::needless_pass_by_value,
        reason = "a chain's constructors take their options by value, as TapeWriter::resume_on does"
    )]
    pub fn resume_on(
        store: T,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError<T::Error>> {
        match options.tapes {
            PieceTapes::Each => Self::resume_each(store, owner, chain, &options),
            PieceTapes::One => Self::resume_one(store, owner, chain, &options),
        }
    }

    /// [Self::resume_on] of a chain whose pieces are tapes of their own. The pieces are read first and the holes
    /// between them checked; only then is a piece written: a piece cut whole that a crash left unmarked, or torn after
    /// its tail, is made so, and the last piece is resumed. A resume refused for the chain (a hole, a lost head, a last
    /// piece whose head doesn't read and whose bytes its own resume refuses) writes nothing. A repair that refuses (a
    /// good block after a torn cut piece's tail, or damage there) comes after the repairs before it, and a refusal by
    /// the last piece's own resume ([TapeWriter::resume_on]) after all of them: the bytes after a cut piece's tail are
    /// dropped and its closed mark set, as any later resume would do.
    fn resume_each(
        mut store: T,
        owner: OwnerId,
        chain: u64,
        options: &ChainOptions,
    ) -> Result<(Self, Resumed), WriteError<T::Error>> {
        let (walk, key, reopened) = {
            let walk = walk_each(&mut store)?;
            // the last piece's head unread, no piece just before it to start it again (deleted, or a hole): refused,
            // with its own bytes' refusal when they are more than a head a crash cut off
            let unread = walk.last_seen.as_ref().is_some_and(|s| s.head.is_none());
            let after_gap = walk.gaps.last().is_some_and(|g| g.1 == walk.seq);
            let headless = unread && (after_gap || (walk.closed.is_empty() && walk.seq != 0));
            if headless {
                let key = store.key_of(walk.seq);
                if let Some(sink) = store
                    .reopen(&key)
                    .map_err(sink_err("reopening the last piece"))?
                {
                    refuse_headless(&sink)?;
                }
            }
            check_gaps(&walk.closed, walk.last_seen.as_ref(), &walk.gaps)?;
            if headless {
                return Err(WriteError::Resume(ResumeError::HeadLost {
                    seq: Some(walk.seq),
                }));
            }
            // the holes hold: the pieces cut whole that a crash left unmarked or torn after their tail are made so
            for key in &walk.repair {
                let Some(sink) = store
                    .reopen(key)
                    .map_err(sink_err("reopening a piece cut whole"))?
                else {
                    continue;
                };
                match TapeWriter::resume_on(sink, options.writer.clone()) {
                    Err(WriteError::Resume(ResumeError::Continued { .. })) => {}
                    Err(e) => return Err(e),
                    Ok(_) => {
                        return Err(WriteError::Index(IndexError::Invalid(
                            "a piece read as cut resumes as unclosed",
                        )));
                    }
                }
            }
            let key = store.key_of(walk.seq);
            if walk.last_seen.is_none() {
                (walk, key, None)
            } else {
                let sink = store
                    .reopen(&key)
                    .map_err(sink_err("reopening the last piece"))?
                    .ok_or(WriteError::Index(IndexError::Invalid(
                        "the last piece of the chain is gone",
                    )))?;
                // the walk read every piece closed by a cut as closed: the last piece isn't one
                let r = TapeWriter::resume_on(sink, options.writer.clone())?;
                (walk, key, Some(r))
            }
        };
        let Walk {
            closed,
            gaps,
            seq,
            numbers,
            ..
        } = walk;
        let newest = newest_in(&closed, |done, e, buf| {
            let src = store
                .closed(&done.tape)
                .map_err(sink_err("opening a closed piece"))?
                .ok_or(WriteError::Index(IndexError::Invalid(
                    "a closed piece of the chain is gone",
                )))?;
            read_listed(&src, done.piece.start, e, buf)
        })?;
        let (mut writer, resumed) = if let Some((mut w, resumed)) = reopened {
            headed(&w)?;
            reserve_piece(&mut w, options.cut);
            if w.bytes() == 0 {
                // cut off inside its head: the piece starts again (a first piece without carried-over tracks),
                // unless the piece before it is gone
                let after_gap = gaps.last().is_some_and(|g| g.1 == seq);
                match closed.last() {
                    _ if after_gap => {
                        return Err(WriteError::Resume(ResumeError::HeadLost { seq: Some(seq) }));
                    }
                    None if seq != 0 => {
                        return Err(WriteError::Resume(ResumeError::HeadLost { seq: Some(seq) }));
                    }
                    None => {
                        w.begin_piece(&first_head(owner, chain, options.writer.integrity), &[])?;
                    }
                    Some((done, last)) => w.begin_next(next_piece(&done.piece, last)?)?,
                }
            } else if let Some((_, last)) = closed.last() {
                w.keep_after(last.kept.clone());
            }
            (w, resumed)
        } else {
            let Some((done, last)) = closed.last() else {
                return Err(WriteError::Resume(ResumeError::NoPiece));
            };
            let next = next_piece(&done.piece, last)?;
            let sink = store
                .create(&key)
                .map_err(sink_err("creating the next piece"))?;
            let mut w = TapeWriter::start_next(sink, &options.writer, next)?;
            reserve_piece(&mut w, options.cut);
            let resumed = Resumed::started(w.bytes());
            (w, resumed)
        };
        writer.take_newest(&newest);
        let resumed = numbers.under(resumed);
        Ok((
            Self {
                store,
                pieces: options.tapes,
                writer: Some(writer),
                cut: options.cut,
                started: options.writer.clock.now_ms(),
                seq,
                closed: closed.into_iter().map(|(f, _)| f).collect(),
            },
            resumed,
        ))
    }

    /// [Self::resume_on] of a chain whose pieces are all in one tape.
    fn resume_one(
        mut store: T,
        owner: OwnerId,
        chain: u64,
        options: &ChainOptions,
    ) -> Result<(Self, Resumed), WriteError<T::Error>> {
        let key = store.key_of(0);
        let Some(mut sink) = store
            .open_whole(&key)
            .map_err(sink_err("opening the tape of the chain"))?
        else {
            return Err(WriteError::Resume(ResumeError::NoPiece));
        };
        let size = sink
            .size()
            .map_err(crate::sink::sink_err("reading the length to resume"))?;
        let (mut closed, numbers) = closed_in(&sink, &key)?;
        // a last piece the chain doesn't go on from is written on, after its last close record
        if closed.last().is_some_and(|(_, last)| !last.continued) {
            closed.pop();
        }
        let newest = newest_in(&closed, |done, e, buf| {
            read_listed(&SinkSource(&sink), done.piece.start, e, buf)
        })?;
        // the bytes after the last piece cut: the last piece, or every byte the tape keeps
        let open_at = match closed.last() {
            Some((f, _)) => f
                .piece
                .start
                .checked_add(f.piece.len)
                .ok_or_else(too_many)?,
            None => sink
                .front()
                .map_err(crate::sink::sink_err("finding the tape's first byte"))?,
        };
        sink.rebase(open_at)
            .map_err(crate::sink::sink_err("finding the last piece"))?;
        let (mut writer, resumed) = if open_at < size || closed.is_empty() {
            let (mut w, resumed) = TapeWriter::resume_on(sink, options.writer.clone())?;
            headed(&w)?;
            reserve_piece(&mut w, options.cut);
            if w.bytes() == 0 {
                // cut off inside its head: the piece starts again, unless the pieces before it were dropped
                match closed.last() {
                    None if open_at != 0 => {
                        return Err(WriteError::Resume(ResumeError::HeadLost { seq: None }));
                    }
                    None => {
                        w.begin_piece(&first_head(owner, chain, options.writer.integrity), &[])?;
                    }
                    Some((done, last)) => w.begin_next(next_piece(&done.piece, last)?)?,
                }
            } else if let Some((_, last)) = closed.last() {
                w.keep_after(last.kept.clone());
            }
            (w, resumed)
        } else {
            let Some((done, last)) = closed.last() else {
                return Err(WriteError::Index(IndexError::Invalid(
                    "a file of closed pieces holds one",
                )));
            };
            let next = next_piece(&done.piece, last)?;
            let mut w = TapeWriter::start_next(sink, &options.writer, next)?;
            reserve_piece(&mut w, options.cut);
            let resumed = Resumed::started(w.bytes());
            (w, resumed)
        };
        writer.take_newest(&newest);
        let resumed = numbers.under(resumed);
        // the pieces before the first at hand may be dropped: the seq is the head's
        let seq = writer
            .piece_head()
            .map(|h| h.ident.seq)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "the last piece of the chain has no readable piece head",
            )))?;
        Ok((
            Self {
                store,
                pieces: options.tapes,
                writer: Some(writer),
                cut: options.cut,
                started: options.writer.clock.now_ms(),
                seq,
                closed: closed.into_iter().map(|(f, _)| f).collect(),
            },
            resumed,
        ))
    }

    /// The piece being written; [WriteError::Failed] after a failed cut.
    pub fn writer(&mut self) -> Result<&mut TapeWriter<T::Sink>, WriteError<T::Error>> {
        self.writer.as_mut().ok_or(WriteError::Failed)
    }

    /// The pieces closed so far, those before a resume included.
    pub fn closed(&self) -> &[ChainPiece<T::Key>] {
        &self.closed
    }

    /// When the chain cuts.
    pub fn cut_policy(&self) -> CutPolicy {
        self.cut
    }

    /// Sequence number of the piece being written.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The store of the pieces' tapes, for reading them ([crate::MemPieces::get]).
    pub fn store(&self) -> &T {
        &self.store
    }

    /// The key of the tape piece `seq` is in.
    fn tape_of(&self, seq: u64) -> T::Key {
        match self.pieces {
            PieceTapes::Each => self.store.key_of(seq),
            PieceTapes::One => self.store.key_of(0),
        }
    }

    /// `call` on the writer; when the piece lists all the state entries it may ([IndexError::StateEntries]), again
    /// on a new piece. A call refused so has written nothing: it checks its room first. The new piece carries the
    /// definitions of the call's tracks first ([TapeWriter::want]), so the call fits it. A call the writer refuses
    /// as input ([crate::InputError]) is checked before its room, so it never cuts.
    fn with_room<R>(
        &mut self,
        defs: &Definitions,
        mut call: impl FnMut(&mut TapeWriter<T::Sink>) -> Result<R, WriteError<T::Error>>,
    ) -> Result<R, WriteError<T::Error>> {
        match call(self.writer()?) {
            // a cut inside a unit would split it: the call fails instead
            Err(WriteError::Index(IndexError::StateEntries { .. }))
                if !self.writer.as_ref().is_some_and(TapeWriter::unit_open) =>
            {
                self.cut_now(defs)?;
                call(self.writer()?)
            }
            r => r,
        }
    }

    /// A whole block ([TapeWriter::write_block]); a chunk block in a new piece when [Self::cut_due].
    pub fn write_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<T::Error>> {
        self.cut_for_block(defs, block)?;
        self.with_room(defs, |w| w.write_block(defs, block))
            .map(|_| ())
    }

    /// Pass on a live producer's block ([TapeWriter::relay_block]), its definitions held in `defs`; a chunk block in
    /// a new piece when [Self::cut_due]. When the piece lists all the state entries it may, again on a new piece.
    pub fn relay_block(
        &mut self,
        defs: &mut Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<T::Error>> {
        self.cut_for_block(defs, block)?;
        match self.writer()?.relay_block(defs, block) {
            Err(WriteError::Index(IndexError::StateEntries { .. })) => {
                self.cut_now(defs)?;
                self.writer()?.relay_block(defs, block)
            }
            r => r,
        }
    }

    /// A producer's block as the writer's own ([TapeWriter::adopt_block]); a chunk block in a new piece when
    /// [Self::cut_due].
    pub fn adopt_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<T::Error>> {
        self.cut_for_block(defs, block)?;
        self.with_room(defs, |w| w.adopt_block(defs, block))
            .map(|_| ())
    }

    /// Cut before the chunk block `block` when [Self::cut_due]; any other block, or one that doesn't read, writes on
    /// in the piece (the writer refuses a bad block).
    fn cut_for_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<T::Error>> {
        if !self.cut_due() {
            return Ok(());
        }
        let chunk = crate::Block::read(block)
            .ok()
            .and_then(|b| b.chunk().map(|c| c.head().track_number));
        match chunk {
            Some(track) => self.cut_for(defs, track),
            None => Ok(()),
        }
    }

    /// Cut before a chunk of `track`, due by the policy: refused before the cut when the chunk's definitions fit no
    /// piece; the next piece carries `track` first.
    fn cut_for(&mut self, defs: &Definitions, track: TrackNum) -> Result<(), WriteError<T::Error>> {
        let w = self.writer()?;
        w.admit_data(defs, &[track])?;
        w.want(&[track]);
        self.cut_now(defs)
    }

    /// Put a state track's item ([TapeWriter::put]).
    pub fn put(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        item: &[u8],
    ) -> Result<bool, WriteError<T::Error>> {
        self.with_room(defs, |w| w.put(defs, track, ticks, item))
    }

    /// Put a key's value on a keyed state track ([TapeWriter::put_key]).
    pub fn put_key(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        key: &[u8],
        value: &[u8],
    ) -> Result<bool, WriteError<T::Error>> {
        self.with_room(defs, |w| w.put_key(defs, track, ticks, key, value))
    }

    /// Put a key's deletion on a keyed state track ([TapeWriter::delete_key]).
    pub fn delete_key(
        &mut self,
        defs: &Definitions,
        track: TrackNum,
        ticks: i64,
        key: &[u8],
    ) -> Result<bool, WriteError<T::Error>> {
        self.with_room(defs, |w| w.delete_key(defs, track, ticks, key))
    }

    /// Put a group of state tracks' items, taken whole or not at all ([TapeWriter::put_group]). When the piece has no
    /// room for the group's state entries, the chain cuts before it and puts it whole in the new piece: never a cut
    /// inside it.
    pub fn put_group(
        &mut self,
        defs: &Definitions,
        puts: &[crate::GroupPut<'_>],
    ) -> Result<Vec<bool>, WriteError<T::Error>> {
        self.with_room(defs, |w| w.put_group(defs, puts))
    }

    /// Put a typed item ([TapeWriter::put_item]).
    pub fn put_item(
        &mut self,
        defs: &Definitions,
        item: &crate::TypedItem<'_>,
    ) -> Result<bool, WriteError<T::Error>> {
        self.with_room(defs, |w| w.put_item(defs, item))
    }

    /// A small block placed in the next gap ([TapeWriter::defer_block]).
    /// When the piece has no room for the state entries of the definitions it needs, on a new piece.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the signature of TapeWriter::defer_block, which keeps the block; the chain copies it for a retry"
    )]
    pub fn defer_block(
        &mut self,
        defs: &Definitions,
        block: Vec<u8>,
    ) -> Result<(), WriteError<T::Error>> {
        // a small block: a copy for the retry costs little
        self.with_room(defs, |w| w.defer_block(defs, block.clone()))
    }

    /// Whether the policy says the current piece is full: checked before a chunk, by its bytes, its chunk blocks
    /// ([TapeWriter::chunks], whoever wrote them) and its age. False after a failed cut.
    pub fn cut_due(&self) -> bool {
        let Some(w) = self.writer.as_ref() else {
            return false;
        };
        // never inside a unit ([TapeWriter::write_chunk_unit])
        if w.unit_open() {
            return false;
        }
        let c = self.cut;
        (c.bytes.get() > 0 && w.bytes() >= c.bytes.get())
            || (c.chunks.get() > 0 && w.chunks() >= c.chunks.get())
            || c.interval
                .is_some_and(|t| w.clock().passed(self.started, t))
    }

    /// A chunk ([TapeWriter::write_chunk]), in a new piece when [Self::cut_due]: a cut happens before a chunk, never
    /// after the last one, so no piece is left empty. A chunk whose definitions fit no piece
    /// ([crate::InputError::CarriedEntries]) is refused before that cut, so it cuts no piece either. Where it landed.
    pub fn write_chunk(
        &mut self,
        defs: &Definitions,
        head: &ChunkHead,
        columns: &[(ColumnSpec, &[u8])],
    ) -> Result<ChunkPlaced, WriteError<T::Error>> {
        self.write_chunk_as(defs, head, columns, None)
    }

    /// [Self::write_chunk] with the hash of its payload given ([TapeWriter::write_chunk_hashed]).
    /// The level of the pieces the chain starts ([WriterOptions::integrity]): below [Integrity::Full](crate::Integrity::Full)
    /// a producer need not hash payloads for [Self::write_chunk_hashed]. None after a failed cut.
    pub fn integrity(&self) -> Option<crate::Integrity> {
        self.writer.as_ref().map(|w| w.options().integrity)
    }

    pub fn write_chunk_hashed(
        &mut self,
        defs: &Definitions,
        head: &ChunkHead,
        columns: &[(ColumnSpec, &[u8])],
        payload_hash: [u8; 32],
    ) -> Result<ChunkPlaced, WriteError<T::Error>> {
        self.write_chunk_as(defs, head, columns, Some(payload_hash))
    }

    /// Chunks of one track that belong together, as a grid tile chunk and its overview chunks do
    /// ([grid](crate::grid#overviews)), written as one unit ([TapeWriter::write_chunk_unit]) in one piece: a cut by
    /// the policy comes before the unit when due, never inside it, so the policy counts the unit's bytes and chunks
    /// as one. Where each landed.
    pub fn write_unit(
        &mut self,
        defs: &Definitions,
        chunks: &[crate::UnitChunk<'_>],
    ) -> Result<Vec<ChunkPlaced>, WriteError<T::Error>> {
        if self.cut_due() {
            let track = chunks.first().map(|c| c.head.track_number);
            if let Some(track) = track {
                self.cut_for(defs, track)?;
            }
        }
        let placed = self.with_room(defs, |w| w.write_chunk_unit(defs, chunks))?;
        Ok(placed
            .into_iter()
            .map(|Placed { offset, len, unit }| ChunkPlaced {
                seq: self.seq,
                offset,
                len,
                unit,
            })
            .collect())
    }

    fn write_chunk_as(
        &mut self,
        defs: &Definitions,
        head: &ChunkHead,
        columns: &[(ColumnSpec, &[u8])],
        payload_hash: Option<[u8; 32]>,
    ) -> Result<ChunkPlaced, WriteError<T::Error>> {
        if self.cut_due() {
            self.cut_for(defs, head.track_number)?;
        }
        self.place_chunk(defs, head, columns, payload_hash)
    }

    /// Write a chunk into the current piece, or the next when its definitions fit no room left.
    fn place_chunk(
        &mut self,
        defs: &Definitions,
        head: &ChunkHead,
        columns: &[(ColumnSpec, &[u8])],
        payload_hash: Option<[u8; 32]>,
    ) -> Result<ChunkPlaced, WriteError<T::Error>> {
        let Placed { offset, len, unit } = self.with_room(defs, |w| match payload_hash {
            Some(h) => w.write_chunk_hashed(defs, head, columns, h),
            None => w.write_chunk(defs, head, columns),
        })?;
        Ok(ChunkPlaced {
            seq: self.seq,
            offset,
            len,
            unit,
        })
    }

    /// Cut to a new piece now, its head followed by the definitions `defs` holds of the tracks it carries
    /// ([TapeWriter::cut_on]). A cut refused before the piece ends keeps the piece being written. The new piece
    /// carries the last call's tracks first and keeps room for their records; a caller writing other tracks next
    /// names them with [TapeWriter::want] first.
    pub fn cut_now(&mut self, defs: &Definitions) -> Result<(), WriteError<T::Error>> {
        let seq = self
            .seq
            .checked_add(1)
            .ok_or(WriteError::Index(IndexError::Invalid(
                "piece seq past u64::MAX",
            )))?;
        // what may fail before the piece ends fails here, the writer kept
        if let Some(w) = self.writer.as_mut() {
            w.flush_deferred()?;
        }
        let current = self.writer.take().ok_or(WriteError::Failed)?;
        let tape = self.tape_of(self.seq);
        let cut = match self.pieces {
            PieceTapes::Each => {
                let key = self.store.key_of(seq);
                current.cut_with(defs, &mut self.store, &key)
            }
            PieceTapes::One => current.cut_in_place(defs),
        };
        let (piece, mut next) = match cut {
            Ok(cut) => cut,
            Err(e) => {
                self.writer = e.writer.map(|w| *w);
                return Err(e.error);
            }
        };
        reserve_piece(&mut next, self.cut);
        self.writer = Some(next);
        self.closed.push(ChainPiece { tape, piece });
        self.seq = seq;
        self.started = self.writer.as_ref().and_then(|w| w.clock().now_ms());
        Ok(())
    }

    /// Remove the closed pieces of sequence numbers `seqs` from the store, on purpose, so that the chain still reads
    /// and resumes, also after a crash part way.
    ///
    /// A piece after one that stays is named first ([TapeWriter::record_removed]) in the piece being written, with
    /// the rows the removed pieces held of pieces gone before; that piece is then closed and synced, and goes on after
    /// its close ([TapeWriter::close_and_go_on]). Only then are the pieces taken out of the store, oldest first
    /// ([PieceStore::remove]); when the piece being written becomes the lowest, it is synced first. The pieces before
    /// every piece that stays need no row: the chain reads as cut at its
    /// head ([ChainReport::head_cut](super::ChainReport::head_cut)), as after retention. In a chain in one tape
    /// ([PieceTapes::One]) only those go, as the front of the tape ([PieceStore::drop_front]).
    ///
    /// Refused ([crate::InputError::Removed]) before anything is written: a piece that is not a closed piece of the
    /// chain at hand ([RemovedRefusal::NotAtHand](crate::writer::RemovedRefusal::NotAtHand)), the piece being written
    /// among them; a piece inside a tape of several after one that stays
    /// ([RemovedRefusal::InTape](crate::writer::RemovedRefusal::InTape)); rows [TapeWriter::record_removed] refuses.
    ///
    /// A close that fails leaves the chain failed ([WriteError::Failed] from then on), as a failed cut does, with no
    /// piece removed: the piece reads as a crash left it, and a resume goes on from there.
    pub fn remove(&mut self, seqs: &[u64]) -> Result<(), WriteError<T::Error>> {
        use crate::writer::{InputError, RemovedRefusal};
        let refuse = |why| Err(WriteError::Input(InputError::Removed(why)));
        let seq_of = |c: &ChainPiece<T::Key>| c.piece.ident.map(|i| i.seq);
        let mut gone: BTreeSet<u64> = BTreeSet::new();
        for &seq in seqs {
            if !self.closed.iter().any(|c| seq_of(c) == Some(seq)) {
                return refuse(RemovedRefusal::NotAtHand { seq });
            }
            gone.insert(seq);
        }
        if gone.is_empty() {
            return Ok(());
        }
        let kept: BTreeSet<u64> = self
            .closed
            .iter()
            .filter_map(seq_of)
            .filter(|s| !gone.contains(s))
            .chain([self.seq])
            .collect();
        // the lowest piece that stays: the pieces before it go as a head cut
        let lowest = self
            .closed
            .iter()
            .filter_map(seq_of)
            .find(|s| !gone.contains(s))
            .unwrap_or(self.seq);
        if self.pieces == PieceTapes::One
            && let Some(&seq) = gone.iter().find(|&&s| s > lowest)
        {
            return refuse(RemovedRefusal::InTape { seq });
        }
        let removed: Vec<ChainPiece<T::Key>> = self
            .closed
            .iter()
            .filter(|c| seq_of(c).is_some_and(|s| gone.contains(&s)))
            .cloned()
            .collect();
        let mut rows: Vec<RemovedPiece> = Vec::new();
        for c in &removed {
            let Some(ident) = c.piece.ident.filter(|i| i.seq > lowest) else {
                continue;
            };
            rows.push(RemovedPiece {
                seq: ident.seq,
                id: ident.id,
                hash: c.piece.hash,
            });
        }
        // the rows a removed piece holds of pieces gone before: named again, or the hole they cover reads as a gap
        if self.pieces == PieceTapes::Each {
            for c in &removed {
                let Some(src) = self
                    .store
                    .closed(&c.tape)
                    .map_err(sink_err("opening a piece to remove"))?
                else {
                    continue;
                };
                let reader = TapeReader::open(&src).map_err(WriteError::Read)?;
                let held = reader.pieces().first().map(|p| p.removed.clone());
                rows.extend(
                    held.unwrap_or_default()
                        .into_iter()
                        .filter(|r| r.seq > lowest && !kept.contains(&r.seq)),
                );
            }
        }
        if !rows.is_empty() {
            self.writer()?.record_removed(&rows)?;
            let w = self.writer.take().ok_or(WriteError::Failed)?;
            let (mut w, _) = w.close_and_go_on()?;
            reserve_piece(&mut w, self.cut);
            self.writer = Some(w);
        } else if lowest == self.seq {
            // the piece written becomes the lowest: its head, which a cut wrote unsynced, must outlive the pieces
            // before it, or a crash leaves no piece to resume from
            self.writer()?.sync()?;
        }
        match self.pieces {
            PieceTapes::Each => {
                for c in &removed {
                    self.store
                        .remove(&c.tape)
                        .map_err(sink_err("removing a piece"))?;
                    // a failure part way lists the pieces still there
                    self.closed.retain(|k| k.tape != c.tape);
                }
            }
            PieceTapes::One => {
                let to = removed
                    .iter()
                    .map(|c| c.piece.start.saturating_add(c.piece.len))
                    .fold(0, u64::max);
                let tape = self.store.key_of(0);
                self.store
                    .drop_front(&tape, to)
                    .map_err(sink_err("dropping the front of the tape"))?;
            }
        }
        self.closed
            .retain(|c| !seq_of(c).is_some_and(|s| gone.contains(&s)));
        Ok(())
    }

    /// Close the last piece ([CloseReason::Closed]); every piece of the chain.
    pub fn close(self) -> Result<Vec<ChainPiece<T::Key>>, WriteError<T::Error>> {
        self.close_as(CloseReason::Closed)
    }

    /// Close the last piece for `reason`; every piece of the chain.
    pub fn close_as(
        mut self,
        reason: CloseReason,
    ) -> Result<Vec<ChainPiece<T::Key>>, WriteError<T::Error>> {
        if let Some(w) = self.writer.as_mut() {
            w.flush_deferred()?;
        }
        let tape = self.tape_of(self.seq);
        let piece = self
            .writer
            .take()
            .ok_or(WriteError::Failed)?
            .close_as(reason)?;
        self.closed.push(ChainPiece { tape, piece });
        Ok(self.closed)
    }
}

/// The head of the first piece of chain `chain` of `owner`, of level `integrity`.
fn first_head(owner: OwnerId, chain: u64, integrity: crate::Integrity) -> PieceHead {
    PieceHead {
        ident: PieceIdent {
            id: PieceId::first_of(&owner, chain),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity,
        unlisted_keys_unknown: false,
    }
}

/// The piece after the closed piece `done`, continued, as its cut would have started it.
fn next_piece<E>(done: &Closed, last: &LastPiece) -> Result<NextPiece, WriteError<E>> {
    // a keyed track whose keys the piece before holds whole is carried whole, as the cut would have
    let tracks = last
        .next_tracks
        .iter()
        .map(|t| TrackStart {
            keys_carried: last
                .kept
                .keys(t.track_number, (t.track_id, t.owner))
                .is_some_and(|k| k.complete),
            ..*t
        })
        .collect();
    let mut next = NextPiece::after(
        &last.head,
        done,
        (tracks, last.unlisted),
        last.session,
        last.next_seq,
    )?;
    next.kept = last.kept.clone();
    Ok(next)
}

/// What a resume of a chain of a tape per piece finds before it writes: the closed pieces from the lowest there, the
/// holes between them, and the last piece.
struct Walk<K> {
    closed: Vec<Done<K>>,
    /// Pieces missing between those at hand: (from, to, how many closed pieces come before them)
    gaps: Vec<(u64, u64, usize)>,
    /// Pieces cut whole that a crash left unmarked ([TapeSink::close]) or with torn bytes after their tail: read as
    /// closed, made so ([TapeWriter::resume_on] gives [ResumeError::Continued]) once the holes hold
    repair: Vec<K>,
    /// The last piece's seq: the one to resume, or the one to start
    seq: u64,
    /// The last piece, as a reader finds it; None when there is none and it is to be started
    last_seen: Option<Seen>,
    /// The numbers of the pieces read, the last one's up to its last close
    numbers: ChainNumbers,
}

/// A piece the resume goes on in, read before it is touched: its head and the removed pieces its close in force names.
#[derive(Default)]
struct Seen {
    /// None when the head doesn't read: cut off inside it, or damaged
    head: Option<PieceHead>,
    removed: Vec<RemovedPiece>,
}

/// Read the pieces of a chain of a tape per piece from the lowest there, writing nothing ([PieceChain::resume_on]).
fn walk_each<T: PieceStore>(store: &mut T) -> Result<Walk<T::Key>, WriteError<T::Error>> {
    let mut walk = Walk {
        closed: Vec::new(),
        gaps: Vec::new(),
        repair: Vec::new(),
        seq: 0,
        last_seen: None,
        numbers: ChainNumbers::default(),
    };
    // a head cut starts the chain at hand: the lowest piece there
    walk.seq = store
        .first_from(0)
        .map_err(sink_err("finding the lowest piece"))?
        .ok_or(WriteError::Resume(ResumeError::NoPiece))?;
    loop {
        let key = store.key_of(walk.seq);
        let named = store
            .closed(&key)
            .map_err(sink_err("opening a closed piece"))?;
        // a piece the chain goes on from: cut, closed by its last close record
        let (cut, seen) = match &named {
            Some(src) => {
                let cut = cut_piece(src, &mut walk.numbers)?;
                if cut.as_ref().is_some_and(|c| c.torn) {
                    walk.repair.push(key.clone());
                }
                let seen = seen_in(src, cut.is_none().then_some(&mut walk.numbers))?;
                (cut, Some(seen))
            }
            // a store that marks pieces closed may hold one a crash left unmarked: read through its sink
            None => match store
                .reopen(&key)
                .map_err(sink_err("reopening the last piece"))?
            {
                Some(sink) => {
                    let src = SinkSource(&sink);
                    let cut = cut_piece(&src, &mut walk.numbers)?;
                    if cut.is_some() {
                        walk.repair.push(key.clone());
                    }
                    let seen = seen_in(&src, cut.is_none().then_some(&mut walk.numbers))?;
                    (cut, Some(seen))
                }
                None => (None, None),
            },
        };
        match (cut, seen) {
            (Some(CutPiece { piece, last, .. }), _) => {
                walk.closed.push((ChainPiece { tape: key, piece }, last));
                walk.seq = walk.seq.checked_add(1).ok_or_else(too_many)?;
            }
            (None, Some(seen)) => {
                walk.last_seen = Some(seen);
                return Ok(walk);
            }
            // the end of the chain, or pieces missing before later ones
            (None, None) => {
                let after = walk.seq.checked_add(1).ok_or_else(too_many)?;
                match store
                    .first_from(after)
                    .map_err(sink_err("finding the next piece"))?
                {
                    None => return Ok(walk),
                    Some(next) => {
                        walk.gaps.push((walk.seq, next, walk.closed.len()));
                        walk.seq = next;
                    }
                }
            }
        }
    }
}

/// The head and the removed pieces of the piece in `src`, as a reader finds them; a head the reader doesn't take (cut
/// off or damaged) is None. A reader's refusal of the bytes is kept. Into `numbers`, when given, the piece's numbers
/// up to its last close: its resume reads only what follows it.
fn seen_in<B: ByteSource>(
    src: &B,
    numbers: Option<&mut ChainNumbers>,
) -> Result<Seen, WriteError<B::Error>> {
    let reader = TapeReader::open(src).map_err(WriteError::Read)?;
    if let Some(numbers) = numbers {
        numbers.take(&reader, 0)?;
    }
    Ok(reader
        .pieces()
        .first()
        .map_or_else(Seen::default, |p| Seen {
            head: p.head.as_ref().map(|(h, _)| *h),
            removed: p.removed.clone(),
        }))
}

/// Refuse each gap of `gaps` (pieces `from..to` missing after the first `before` pieces of `closed`) that the closes at
/// hand don't name as removed on purpose, as [check_chain](super::check_chain) links over them
/// ([ChainLink::Removed](super::ChainLink::Removed)): every missing piece named, agreed; the cut before the gap names
/// the first one's id; the head after it names the last one's hash and id. The piece after the last gap is `last`, the
/// piece to resume, when no closed piece comes after it.
fn check_gaps<K, E>(
    closed: &[Done<K>],
    last: Option<&Seen>,
    gaps: &[(u64, u64, usize)],
) -> Result<(), WriteError<E>> {
    if gaps.is_empty() {
        return Ok(());
    }
    let resumed: &[RemovedPiece] = last.map_or(&[], |s| &s.removed);
    let removed = Removed::of_rows(closed.iter().flat_map(|(_, l)| &l.removed).chain(resumed));
    for &(from, to, before) in gaps {
        let gap = WriteError::Resume(ResumeError::Gap { from, to });
        let Some((first, end)) = removed.span(from, to) else {
            return Err(gap);
        };
        let cut = before
            .checked_sub(1)
            .and_then(|i| closed.get(i))
            .and_then(|(done, _)| done.piece.next);
        let head = match (closed.get(before), last) {
            (Some((_, l)), _) => Some(l.head.prev),
            (None, Some(s)) => match s.head {
                Some(h) => Some(h.prev),
                // cut off inside its head: the piece before it is gone, so it can't start again
                None => return Err(WriteError::Resume(ResumeError::HeadLost { seq: Some(to) })),
            },
            (None, None) => None,
        };
        if cut != Some(first.id) || head != Some(Some((end.hash, end.id))) {
            return Err(gap);
        }
    }
    Ok(())
}

fn too_many<E>() -> WriteError<E> {
    WriteError::Index(IndexError::Invalid("piece seq past u64::MAX"))
}

/// The closed pieces at the start of the tape `key` in `sink`, in order, read from their tails: every piece but
/// an unclosed last one. A piece without its end before another is refused. With them the numbers of every piece,
/// an unclosed last one's up to its last close.
fn closed_in<S: TapeSink, K: Clone>(
    sink: &S,
    key: &K,
) -> Result<ClosedIn<K>, WriteError<S::Error>> {
    let src = SinkSource(sink);
    let reader = TapeReader::open(&src).map_err(WriteError::Read)?;
    let pieces = reader.pieces();
    let done = pieces.iter().take_while(|p| p.is_closed()).count();
    if done.saturating_add(1) < pieces.len() {
        return Err(WriteError::Index(IndexError::Invalid(
            "an unclosed piece before the last one in a file of pieces",
        )));
    }
    let mut numbers = ChainNumbers::default();
    let mut closed = Vec::new();
    for i in 0..done {
        let (piece, last) = read_closed_at(&reader, i)?;
        numbers.take(&reader, i)?;
        let tape = key.clone();
        closed.push((ChainPiece { tape, piece }, last));
    }
    // an unclosed last piece written on after a close: its resume reads only what follows that close
    if done < pieces.len() {
        numbers.take(&reader, done)?;
    }
    Ok((closed, numbers))
}

/// A closed piece of a chain, and what the piece after it starts from.
type Done<K> = (ChainPiece<K>, LastPiece);

/// The closed pieces of a tape of pieces, and the numbers of every piece in it ([closed_in]).
type ClosedIn<K> = (Vec<Done<K>>, ChainNumbers);

/// Each track's last chunk in a piece, by its entry.
type LastChunks = BTreeMap<TrackIdent, IndexEntry>;

/// What the piece after a closed one starts from.
struct LastPiece {
    /// It goes on in a next piece, which its close record names
    continued: bool,
    head: PieceHead,
    next_tracks: Vec<TrackStart>,
    session: SessionTag,
    next_seq: u64,
    /// The tops of its chunks' ranges, for the monotone relations' newest values
    ranges: Newest,
    /// Each track's last chunk in the piece, by its entry, read only when no later piece holds a chunk of the track
    last_chunks: BTreeMap<TrackIdent, IndexEntry>,
    /// The newest item of each state track of its next head that reads in it, for the next cut to carry
    kept: Kept,
    /// The pieces of the chain its close in force names as removed on purpose
    removed: Vec<RemovedPiece>,
    /// Its close in force says keyed tracks without a row in the next head may have keys before it
    /// ([crate::piece::Close::unlisted_keys_unknown])
    unlisted: bool,
}

/// A piece the chain goes on from, read by [cut_piece].
struct CutPiece {
    piece: Closed,
    last: LastPiece,
    /// Bytes follow its tail, torn by a crash during the cut: a resume of the piece cuts them
    /// ([ResumeError::Continued]), or refuses a good block among them.
    torn: bool,
}

/// The piece in `src` when the chain goes on from it: closed by a cut (its last close record names the next piece);
/// None for any other. The numbers of a piece it returns go into `numbers`.
fn cut_piece<B: ByteSource>(
    src: &B,
    numbers: &mut ChainNumbers,
) -> Result<Option<CutPiece>, WriteError<B::Error>> {
    let reader = TapeReader::open(src).map_err(WriteError::Read)?;
    let end = match reader.pieces().first() {
        Some(p)
            if p.start == 0
                && p.is_closed()
                && p.close.as_ref().is_some_and(|(c, _)| c.next.is_some()) =>
        {
            p.end
        }
        _ => return Ok(None),
    };
    let size = src
        .size()
        .map_err(crate::sink::sink_err("reading a piece's length"))?;
    let torn = reader.pieces().len() > 1 || size > end;
    let (piece, last) = read_closed_at(&reader, 0)?;
    numbers.take(&reader, 0)?;
    Ok(Some(CutPiece { piece, last, torn }))
}

/// Closed piece `i` of `reader`, from its tail, close and head.
fn read_closed_at<B: ByteSource>(
    reader: &TapeReader<B>,
    i: usize,
) -> Result<(Closed, LastPiece), WriteError<B::Error>> {
    let bad = |what| WriteError::Index(IndexError::Invalid(what));
    let src = reader.source();
    let p = reader.pieces().get(i).ok_or(bad("no such piece"))?;
    if let Some(e @ IndexError::UnknownIntegrity { .. }) = p.head_error {
        return Err(WriteError::Index(e));
    }
    let (Some(tail), Some((close, ranges)), Some((head, starts))) =
        (p.tail, p.close.as_ref(), p.head.as_ref())
    else {
        return Err(bad(
            "a closed piece of a chain has a head, a close and a tail",
        ));
    };
    // the tail carries the session and seq of the last counted block of the piece
    let tail_at = p
        .end
        .checked_sub(crate::reader::wide(TAIL_LEN))
        .ok_or(bad("a piece shorter than its tail"))?;
    let mut header = [0u8; HEADER_LEN];
    src.read_at(tail_at, &mut header)
        .map_err(crate::sink::sink_err("reading a tail"))?;
    let id = block_id(&header).ok_or(bad("a tail shorter than a header"))?;
    // the tail's seq is the last counted block's, or 0 (a first piece) or the piece before's when it holds none
    let next_seq = if head.prev.is_none() && !holds_counted(reader, i)? {
        0
    } else {
        id.seq.wrapping_add(1)
    };
    let next_tracks = crate::index::next_starts(starts, ranges);
    let (newest, last_chunks) = last_chunks_of(reader, i, ranges)?;
    let kept = kept_of(reader, i, &next_tracks)?;
    Ok((
        Closed {
            hash: close.hash,
            len: tail.piece_len,
            ident: close.ident,
            next: close.next,
            start: p.start,
            tracks: next_tracks.clone(),
            states_unknown: 0,
            keys_unknown: 0,
        },
        LastPiece {
            continued: close.next.is_some(),
            head: *head,
            next_tracks,
            session: id.session,
            next_seq,
            ranges: newest,
            last_chunks,
            kept,
            removed: p.removed.clone(),
            unlisted: close.unlisted_keys_unknown || head.unlisted_keys_unknown,
        },
    ))
}

/// The numbers a chain's resume reads from its pieces, oldest first, and the definitions it could not read.
#[derive(Default)]
struct ChainNumbers {
    numbers: crate::Numbers,
    unread: usize,
}

impl ChainNumbers {
    /// Take the numbers of piece `i` of `reader` up to its last close, over those taken: its head's rows, the track
    /// and clock definitions in force at that close, then the close's rows. A piece without a close gives none: its
    /// resume reads it whole. A definition that doesn't read, or that doesn't decode, is counted unread; a lookup
    /// whose budget ran out counts once.
    fn take<B: ByteSource>(
        &mut self,
        reader: &TapeReader<B>,
        i: usize,
    ) -> Result<(), WriteError<B::Error>> {
        let Some(p) = reader.pieces().get(i) else {
            return Ok(());
        };
        let Some(close_at) = p.close_offset else {
            return Ok(());
        };
        for r in p.head.iter().flat_map(|(_, rows)| rows) {
            self.numbers
                .take_track(r.track_number, r.track_id, r.owner.0);
        }
        let defs = reader
            .definitions_of(i, close_at, |s| {
                matches!(
                    s,
                    crate::state::Slot::Track { .. } | crate::state::Slot::Clock { .. }
                )
            })
            .map_err(WriteError::Read)?;
        self.unread = self
            .unread
            .saturating_add(defs.lost.len())
            .saturating_add(usize::from(defs.spent));
        for s in defs.items.values() {
            match <crate::Describe<'_> as shrink_wrap::DeserializeShrinkWrap>::from_ww_bytes(
                &s.item,
            ) {
                Ok(crate::Describe::Track(t)) => self.numbers.take_track(t.number, t.id(), t.owner),
                Ok(crate::Describe::Clock(c)) => self.numbers.take_clock(c.number, c.key),
                _ => self.unread = self.unread.saturating_add(1),
            }
        }
        for r in p.close.iter().flat_map(|(_, rows)| rows) {
            self.numbers
                .take_track(r.track_number, r.track_id, r.owner.0);
        }
        Ok(())
    }

    /// `resumed` with the numbers of the resumed piece over these: what an application numbers the tracks and clocks
    /// it defines again by ([Resumed::numbers]), and the definitions not read ([Resumed::numbers_unread]).
    fn under(mut self, mut resumed: Resumed) -> Resumed {
        self.numbers.take_all(&resumed.numbers);
        resumed.numbers = self.numbers;
        resumed.numbers_unread = resumed.numbers_unread.saturating_add(self.unread);
        resumed
    }
}

/// The newest item of each state track of rows `rows` in piece `i` of `reader` that reads, oldest first: what the
/// writer of the piece kept for its cut ([crate::writer]), and every live key of each keyed state track. A state
/// that only older pieces hold isn't read; the keys are complete when the piece carried those in force at its start.
fn kept_of<B: ByteSource>(
    reader: &TapeReader<B>,
    i: usize,
    rows: &[TrackStart],
) -> Result<Kept, WriteError<B::Error>> {
    let mut items = reader.states_in(i).map_err(WriteError::Read)?;
    items.sort_by_key(|s| s.offset);
    let keys: BTreeMap<TrackNum, TrackIdent> = rows
        .iter()
        .map(|t| (t.track_number, (t.track_id, t.owner)))
        .collect();
    let mut kept = Kept::default();
    for s in items {
        if let crate::state::Slot::State { track } = s.slot
            && let Some(key) = keys.get(&track)
        {
            kept.placed(track, *key, s.at, &s.item, usize::MAX);
        }
    }
    // every live key of each keyed track as of the close: the piece's own and those its head carried
    let p = reader.pieces().get(i);
    let head_rows = p.and_then(|p| p.head.as_ref()).map_or(&[][..], |(_, r)| r);
    let before = p.and_then(|p| p.close_offset).unwrap_or(u64::MAX);
    // uncapped like the plain items: the writer's keep_after counts both under its kept_bytes_max
    crate::writer::resume::read_keys(reader, i, head_rows, &keys, before, usize::MAX, &mut kept)
        .map_err(WriteError::Read)?;
    Ok(kept)
}

/// The tops of the chunks' ranges of the tracks of piece `i` of `reader`, whose close lists `rows`, from each
/// track's last break on ([FLAG_RELATION_BREAK](crate::FLAG_RELATION_BREAK)), and each track's last chunk in the
/// piece, by its entry, unread.
fn last_chunks_of<B: ByteSource>(
    reader: &TapeReader<B>,
    i: usize,
    rows: &[crate::piece::TrackRange],
) -> Result<(Newest, LastChunks), WriteError<B::Error>> {
    let entries = reader.entries(i).map_err(WriteError::Read)?;
    Ok(last_chunks_in(&entries, rows))
}

/// [last_chunks_of] the index entries `entries` of a piece.
fn last_chunks_in(
    entries: &[IndexEntry],
    rows: &[crate::piece::TrackRange],
) -> (Newest, LastChunks) {
    let mut ranges = Newest::default();
    let mut last: BTreeMap<TrackIdent, IndexEntry> = BTreeMap::new();
    for &e in entries {
        if e.kind != crate::BlockKind::Events || e.repeat {
            continue;
        }
        // the owner by the close's row of the track; a track it doesn't list is of the piece's owner
        let owner = rows
            .iter()
            .find(|r| r.track_id == e.track_id && Some(r.track_number) == e.track())
            .map_or(OwnerId::NONE, |r| r.owner);
        let key = (e.track_id, owner);
        // a track's entries come by offset: a break forgets the ranges before it
        if e.relation_break {
            ranges.restart(key);
        }
        if let Some((_, hi)) = e.range {
            ranges.seed_range(key, hi);
        }
        if last.get(&key).is_none_or(|l| l.offset < e.offset) {
            last.insert(key, e);
        }
    }
    (ranges, last)
}

/// The newest values of the monotone relations of a piece at `start` in `src` up to its close in force, which lists
/// `rows`, from its index entries `entries`: the tops of its chunks' ranges and the relation columns of each track's
/// last chunk, one chunk per track. Without entries (the index chain doesn't read, damage in an older final index),
/// each track the rows give chunks is unread: its next chunk is refused unless the caller accepts a break.
pub(crate) fn newest_of_piece<B: ByteSource>(
    src: &B,
    start: u64,
    entries: Option<&[IndexEntry]>,
    rows: &[crate::piece::TrackRange],
) -> Result<Newest, WriteError<B::Error>> {
    let Some(entries) = entries else {
        let mut newest = Newest::default();
        for r in rows.iter().filter(|r| r.chunks > 0) {
            newest.seed_unread((r.track_id, r.owner));
        }
        return Ok(newest);
    };
    let (mut newest, last) = last_chunks_in(entries, rows);
    let mut buf = Vec::new();
    for (key, e) in &last {
        let chunk = if read_listed(src, start, e, &mut buf)? {
            crate::Block::read(&buf).ok().and_then(|b| b.chunk())
        } else {
            None
        };
        match chunk {
            Some(c) => newest.seed_chunk(*key, &c),
            None => newest.seed_unread(*key),
        }
    }
    Ok(newest)
}

/// Reads the chunk block that entry `e` of the piece at `start` in `src` lists into `buf`; false when its length
/// or offset can't be a block's.
fn read_listed<B: ByteSource>(
    src: &B,
    start: u64,
    e: &IndexEntry,
    buf: &mut Vec<u8>,
) -> Result<bool, WriteError<B::Error>> {
    // an entry read from the file: a block's length at most
    let Ok(len) = crate::FromFile::new(e.len).at_most(crate::MAX_BLOCK_LEN) else {
        return Ok(false);
    };
    let Some(at) = start.checked_add(e.offset) else {
        return Ok(false);
    };
    buf.resize(len, 0);
    src.read_at(at, buf).map_err(crate::sink::sink_err(
        "reading a piece's last chunk of a track",
    ))?;
    Ok(true)
}

/// The newest values of the monotone relations of the pieces `closed`, in order: the tops of every piece's
/// ranges, and the relation columns of each track's last chunk in the newest piece that holds a chunk of the track,
/// read by `read` (which fills the buffer with the block an entry of a closed piece lists, false when it can't).
/// A track whose newest last chunk doesn't read is unread. One chunk per track is read, at most
/// [MAX_BLOCK_LEN](crate::MAX_BLOCK_LEN) bytes.
fn newest_in<K, E>(
    closed: &[Done<K>],
    mut read: impl FnMut(&ChainPiece<K>, &IndexEntry, &mut Vec<u8>) -> Result<bool, WriteError<E>>,
) -> Result<Newest, WriteError<E>> {
    let mut newest = Newest::default();
    for (_, last) in closed {
        let mut later = last.ranges.clone();
        later.after(&newest);
        newest = later;
    }
    let mut known: BTreeSet<TrackIdent> = BTreeSet::new();
    let mut buf = Vec::new();
    for (done, last) in closed.iter().rev() {
        for (key, e) in &last.last_chunks {
            if !known.insert(*key) {
                continue;
            }
            let chunk = if read(done, e, &mut buf)? {
                crate::Block::read(&buf).ok().and_then(|b| b.chunk())
            } else {
                None
            };
            match chunk {
                Some(c) => newest.seed_chunk(*key, &c),
                None => newest.seed_unread(*key),
            }
        }
    }
    Ok(newest)
}

/// Whether piece `i` of `reader` holds a block its writer counted: one the index lists that isn't marked
/// FLAG_REPEAT.
fn holds_counted<B: ByteSource>(
    reader: &TapeReader<B>,
    i: usize,
) -> Result<bool, WriteError<B::Error>> {
    Ok(reader
        .entries(i)
        .map_err(WriteError::Read)?
        .iter()
        .any(|e| !e.repeat))
}
