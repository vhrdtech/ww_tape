//! [PieceChain]: a recording written as a chain of pieces, cut at chunk boundaries, on files or any store of
//! [Tapes]: each piece a tape of its own, or all of them back to back in one tape ([PieceTapes]).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::time::Duration;

use super::{CloseReason, OwnerId, PieceHead, PieceId, PieceIdent, TAIL_LEN, TrackStart, UnitHash};
use crate::index::{IndexEntry, IndexError, TrackKey};
use crate::reader::{ByteSource, TapeReader};
use crate::sink::{TapeSink, Tapes, sink_err};
use crate::writer::resume::SinkSource;
use crate::writer::{
    Closed, Newest, NextPiece, Placed, ResumeError, Resumed, TapeWriter, WriteError, WriterOptions,
    block_id,
};
use crate::{ChunkHead, ColumnSpec, Definitions, HEADER_LEN, SessionTag};

/// When a [PieceChain] cuts to a new piece; checked before each chunk. Zero or None turns a limit off.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct CutPolicy {
    /// Once a piece holds this many bytes.
    pub bytes: u64,
    /// Once a piece holds this many chunk blocks.
    pub chunks: u64,
    /// Once a piece is this old (since it was started or resumed).
    pub interval: Option<Duration>,
}

/// What a piece may hold past [CutPolicy::bytes]: the chunk that takes it over the limit, the index, close and tail.
pub const CUT_SLACK: u64 = 1 << 20;

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
    if cut.bytes > 0 {
        writer.reserve(cut.bytes.saturating_add(CUT_SLACK));
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

/// How a [PieceChain] keeps its pieces in its [Tapes].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PieceTapes {
    /// Each piece a tape of its own, piece `seq` under [Tapes::key_of] (`seq`): a file per piece in a directory.
    #[default]
    Each,
    /// All pieces back to back in one tape, under [Tapes::key_of] (0): one file. A cut starts the next piece right
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

/// Writes one recording as pieces on the tapes of a store of [Tapes] (files by default), each piece a tape of its
/// own or all in one ([PieceTapes]), cutting by a [CutPolicy] before a chunk.
pub struct PieceChain<T: Tapes> {
    tapes: T,
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

impl<T: Tapes> PieceChain<T> {
    /// Start chain `chain` of `owner` in `tapes` with its first piece, id [PieceId::first_of] `(owner, chain)`;
    /// `tracks` say where items of tracks carried over from elsewhere continue (empty for a new recording). Each
    /// chain of a owner takes its own number, so its pieces and session tag are its own. With [PieceTapes::One] the
    /// tape that holds every piece is made with [Tapes::create_whole].
    pub fn create_on(
        mut tapes: T,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
    ) -> Result<Self, WriteError<T::Error>> {
        let first = tapes.key_of(0);
        let sink = match options.tapes {
            PieceTapes::Each => tapes
                .create(&first)
                .map_err(sink_err("creating the first piece"))?,
            PieceTapes::One => tapes
                .create_whole(&first)
                .map_err(sink_err("creating the tape of the chain"))?,
        };
        let mut writer = TapeWriter::create_on(sink, options.writer.sync);
        writer.apply(&options.writer)?;
        reserve_piece(&mut writer, options.cut);
        writer.begin_piece(&first_head(owner, chain), tracks)?;
        Ok(Self {
            tapes,
            pieces: options.tapes,
            writer: Some(writer),
            cut: options.cut,
            started: options.writer.clock.now_ms(),
            seq: 0,
            closed: Vec::new(),
        })
    }

    /// Reopen chain `chain` of `owner` in `tapes` to write on, after a crash or after its close: the pieces closed by
    /// a cut are read from their tails ([Self::closed]), the last piece is resumed ([TapeWriter::resume_on]): an
    /// unclosed one goes on after its last good block, a closed one is written on after its last close record. After
    /// a crash between a cut's close and the next piece's start, the next piece is started as the cut would have.
    /// `options` are the ones the chain was written with. A piece cut whole whose name a crash took
    /// ([ResumeError::Continued]) is named and read as closed.
    ///
    /// With [PieceTapes::One] the chain is in one tape, and its last piece is resumed with the same rules as a piece
    /// of its own: only a torn end is cut, and a resume that would drop more is refused. An application that starts
    /// every write session in a piece of its own gives the calls a resume asks for again, then cuts
    /// ([Self::cut_now]).
    pub fn resume_on(
        tapes: T,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError<T::Error>> {
        match options.tapes {
            PieceTapes::Each => Self::resume_each(tapes, owner, chain, options),
            PieceTapes::One => Self::resume_one(tapes, owner, chain, options),
        }
    }

    /// [Self::resume_on] of a chain whose pieces are tapes of their own.
    fn resume_each(
        mut tapes: T,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError<T::Error>> {
        let mut closed: Vec<Done<T::Key>> = Vec::new();
        let mut seq = 0u64;
        let (key, reopened) = loop {
            let key = tapes.key_of(seq);
            let named = tapes
                .closed(&key)
                .map_err(sink_err("opening a closed piece"))?;
            // a piece the chain goes on from: cut, closed by its last close record
            let cut = match &named {
                Some(src) => cut_piece(src)?,
                None => None,
            };
            let (piece, last) = match cut {
                Some(done) => done,
                None => {
                    let Some(sink) = tapes
                        .reopen(&key)
                        .map_err(sink_err("reopening the last piece"))?
                    else {
                        break (key, None);
                    };
                    match TapeWriter::resume_on(sink, options.writer.clone()) {
                        // cut whole before a crash took its name: named now, read as closed
                        Err(WriteError::Resume(ResumeError::Continued { .. })) => {
                            let src = tapes
                                .closed(&key)
                                .map_err(sink_err("opening a closed piece"))?
                                .ok_or(WriteError::Index(IndexError::Invalid(
                                    "a piece given its name doesn't read as closed",
                                )))?;
                            read_closed(&src)?
                        }
                        r => break (key, Some(r?)),
                    }
                }
            };
            closed.push((ChainPiece { tape: key, piece }, last));
            seq = seq.checked_add(1).ok_or_else(too_many)?;
        };
        let newest = newest_in(&closed, |done, e, buf| {
            let src = tapes
                .closed(&done.tape)
                .map_err(sink_err("opening a closed piece"))?
                .ok_or(WriteError::Index(IndexError::Invalid(
                    "a closed piece of the chain is gone",
                )))?;
            read_listed(&src, done.piece.start, e, buf)
        })?;
        let (mut writer, resumed) = match reopened {
            Some((mut w, resumed)) => {
                headed(&w)?;
                reserve_piece(&mut w, options.cut);
                if w.bytes() == 0 {
                    // cut off inside its head: the piece starts again (a first piece without carried-over tracks)
                    match closed.last() {
                        None => w.begin_piece(&first_head(owner, chain), &[])?,
                        Some((done, last)) => w.begin_next(next_piece(&done.piece, last)?)?,
                    }
                }
                (w, resumed)
            }
            None => {
                let Some((done, last)) = closed.last() else {
                    return Err(WriteError::Resume(ResumeError::NoPiece));
                };
                let next = next_piece(&done.piece, last)?;
                let sink = tapes
                    .create(&key)
                    .map_err(sink_err("creating the next piece"))?;
                let mut w = TapeWriter::start_next(sink, &options.writer, next)?;
                reserve_piece(&mut w, options.cut);
                let resumed = Resumed::started(w.bytes());
                (w, resumed)
            }
        };
        writer.take_newest(&newest);
        Ok((
            Self {
                tapes,
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
        mut tapes: T,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError<T::Error>> {
        let key = tapes.key_of(0);
        let Some(mut sink) = tapes
            .open_whole(&key)
            .map_err(sink_err("opening the tape of the chain"))?
        else {
            return Err(WriteError::Resume(ResumeError::NoPiece));
        };
        let size = sink
            .size()
            .map_err(crate::sink::sink_err("reading the length to resume"))?;
        let mut closed = closed_in(&sink, &key)?;
        // a last piece the chain doesn't go on from is written on, after its last close record
        if closed.last().is_some_and(|(_, last)| !last.continued) {
            closed.pop();
        }
        let newest = newest_in(&closed, |done, e, buf| {
            read_listed(&SinkSource(&sink), done.piece.start, e, buf)
        })?;
        #[expect(
            clippy::map_err_ignore,
            reason = "a TryFromIntError says only that the number is out of range, which this error says"
        )]
        let seq = u64::try_from(closed.len()).map_err(|_| too_many())?;
        // the bytes after the last piece cut: the last piece, or nothing
        let open_at = match closed.last() {
            Some((f, _)) => f
                .piece
                .start
                .checked_add(f.piece.len)
                .ok_or_else(too_many)?,
            None => 0,
        };
        sink.rebase(open_at)
            .map_err(crate::sink::sink_err("finding the last piece"))?;
        let (mut writer, resumed) = if open_at < size || closed.is_empty() {
            let (mut w, resumed) = TapeWriter::resume_on(sink, options.writer.clone())?;
            headed(&w)?;
            reserve_piece(&mut w, options.cut);
            if w.bytes() == 0 {
                // cut off inside its head: the piece starts again
                match closed.last() {
                    None => w.begin_piece(&first_head(owner, chain), &[])?,
                    Some((done, last)) => w.begin_next(next_piece(&done.piece, last)?)?,
                }
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
        Ok((
            Self {
                tapes,
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

    /// Sequence number of the piece being written.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The store of the pieces' tapes, for reading them ([crate::MemTapes::get]).
    pub fn tapes(&self) -> &T {
        &self.tapes
    }

    /// The key of the tape piece `seq` is in.
    fn tape_of(&self, seq: u64) -> T::Key {
        match self.pieces {
            PieceTapes::Each => self.tapes.key_of(seq),
            PieceTapes::One => self.tapes.key_of(0),
        }
    }

    /// `call` on the writer; when the piece lists all the state entries it may ([IndexError::StateEntries]), again
    /// on a new piece. A call refused so has written nothing: it checks its room first. The new piece carries the
    /// definitions of the call's tracks first ([TapeWriter::want]), so the call fits it.
    fn with_room<R>(
        &mut self,
        defs: &Definitions,
        mut call: impl FnMut(&mut TapeWriter<T::Sink>) -> Result<R, WriteError<T::Error>>,
    ) -> Result<R, WriteError<T::Error>> {
        match call(self.writer()?) {
            Err(WriteError::Index(IndexError::StateEntries { .. })) => {
                self.cut_now(defs)?;
                call(self.writer()?)
            }
            r => r,
        }
    }

    /// A record block ([TapeWriter::write_block]).
    pub fn write_block(
        &mut self,
        defs: &Definitions,
        block: &[u8],
    ) -> Result<(), WriteError<T::Error>> {
        self.with_room(defs, |w| w.write_block(defs, block))
            .map(|_| ())
    }

    /// Put a state track's item ([TapeWriter::put]).
    pub fn put(
        &mut self,
        defs: &Definitions,
        track: u32,
        ticks: i64,
        item: &[u8],
    ) -> Result<bool, WriteError<T::Error>> {
        self.with_room(defs, |w| w.put(defs, track, ticks, item))
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
        let c = self.cut;
        (c.bytes > 0 && w.bytes() >= c.bytes)
            || (c.chunks > 0 && w.chunks() >= c.chunks)
            || c.interval
                .is_some_and(|t| w.clock().passed(self.started, t))
    }

    /// A chunk ([TapeWriter::write_chunk]), in a new piece when [Self::cut_due]: a cut happens before a chunk, never
    /// after the last one, so no piece is left empty. Where it landed.
    pub fn write_chunk(
        &mut self,
        defs: &Definitions,
        head: &ChunkHead,
        columns: &[(ColumnSpec, &[u8])],
    ) -> Result<ChunkPlaced, WriteError<T::Error>> {
        if self.cut_due() {
            // the next piece carries the chunk's track first
            self.writer()?.want(&[head.track_number]);
            self.cut_now(defs)?;
        }
        let Placed { offset, len, unit } =
            self.with_room(defs, |w| w.write_chunk(defs, head, columns))?;
        Ok(ChunkPlaced {
            seq: self.seq,
            offset,
            len,
            unit,
        })
    }

    /// Cut to a new piece now, its head followed by the definitions `defs` holds of the tracks it carries
    /// ([TapeWriter::cut_on]). A cut refused before the piece ends keeps the piece being written.
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
                let key = self.tapes.key_of(seq);
                current.cut_with(defs, &mut self.tapes, &key)
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

/// The head of the first piece of chain `chain` of `owner`.
fn first_head(owner: OwnerId, chain: u64) -> PieceHead {
    PieceHead {
        ident: PieceIdent {
            id: PieceId::first_of(&owner, chain),
            owner,
            seq: 0,
        },
        prev: None,
        prev_session: SessionTag::NONE,
        integrity: crate::Integrity::Full,
    }
}

/// The piece after the closed piece `done`, continued, as its cut would have started it.
fn next_piece<E>(done: &Closed, last: &LastPiece) -> Result<NextPiece, WriteError<E>> {
    NextPiece::after(
        &last.head,
        done,
        last.next_tracks.clone(),
        last.session,
        last.next_seq,
    )
}

fn too_many<E>() -> WriteError<E> {
    WriteError::Index(IndexError::Invalid("piece seq past u64::MAX"))
}

/// The closed pieces at the start of the tape `key` in `sink`, in order, read from their tails: every piece but
/// an unclosed last one. A piece without its end before another is refused.
fn closed_in<S: TapeSink, K: Clone>(
    sink: &S,
    key: &K,
) -> Result<Vec<Done<K>>, WriteError<S::Error>> {
    let src = SinkSource(sink);
    let reader = TapeReader::open(&src).map_err(WriteError::Read)?;
    let pieces = reader.pieces();
    let done = pieces.iter().take_while(|p| p.is_closed()).count();
    if done.saturating_add(1) < pieces.len() {
        return Err(WriteError::Index(IndexError::Invalid(
            "an unclosed piece before the last one in a file of pieces",
        )));
    }
    (0..done)
        .map(|i| {
            let (piece, last) = read_closed_at(&reader, i)?;
            Ok((
                ChainPiece {
                    tape: key.clone(),
                    piece,
                },
                last,
            ))
        })
        .collect()
}

/// A closed piece of a chain, and what the piece after it starts from.
type Done<K> = (ChainPiece<K>, LastPiece);

/// Each track's last chunk in a piece, by its entry.
type LastChunks = BTreeMap<TrackKey, IndexEntry>;

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
    last_chunks: BTreeMap<TrackKey, IndexEntry>,
}

/// The closed piece in `src`, from its tail, close and head.
fn read_closed<B: ByteSource>(src: &B) -> Result<(Closed, LastPiece), WriteError<B::Error>> {
    let reader = TapeReader::open(src).map_err(WriteError::Read)?;
    if !matches!(reader.pieces(), [p] if p.start == 0) {
        return Err(WriteError::Index(IndexError::Invalid(
            "a closed piece holds one piece",
        )));
    }
    read_closed_at(&reader, 0)
}

/// The piece in `src` when the chain goes on from it: one piece, closed by a cut (its last close record names the
/// next piece), no bytes after its tail; None for any other.
fn cut_piece<B: ByteSource>(src: &B) -> Result<Option<(Closed, LastPiece)>, WriteError<B::Error>> {
    let reader = TapeReader::open(src).map_err(WriteError::Read)?;
    let cut = matches!(reader.pieces(), [p] if p.start == 0
        && p.is_closed()
        && p.close.as_ref().is_some_and(|(c, _)| c.next.is_some()));
    match cut {
        true => read_closed_at(&reader, 0).map(Some),
        false => Ok(None),
    }
}

/// Closed piece `i` of `reader`, from its tail, close and head.
fn read_closed_at<B: ByteSource>(
    reader: &TapeReader<B>,
    i: usize,
) -> Result<(Closed, LastPiece), WriteError<B::Error>> {
    let bad = |what| WriteError::Index(IndexError::Invalid(what));
    let src = reader.source();
    let p = reader.pieces().get(i).ok_or(bad("no such piece"))?;
    if let Some(e @ (IndexError::UnhandledIntegrity { .. } | IndexError::UnknownIntegrity { .. })) =
        p.head_error
    {
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
    Ok((
        Closed {
            hash: close.hash,
            len: tail.piece_len,
            ident: close.ident,
            next: close.next,
            start: p.start,
            tracks: next_tracks.clone(),
        },
        LastPiece {
            continued: close.next.is_some(),
            head: *head,
            next_tracks,
            session: id.session,
            next_seq,
            ranges: newest,
            last_chunks,
        },
    ))
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
    let mut last: BTreeMap<TrackKey, IndexEntry> = BTreeMap::new();
    for &e in entries {
        if e.kind != crate::BlockKind::Events || e.repeat {
            continue;
        }
        // the owner by the close's row of the track; a track it doesn't list is of the piece's owner
        let owner = rows
            .iter()
            .find(|r| r.track_id == e.track_id && r.track_number == e.track_number)
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
    let mut known: BTreeSet<TrackKey> = BTreeSet::new();
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
