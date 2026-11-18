//! [PieceChain]: a recording written as a chain of pieces, cut at chunk boundaries, on files or any [SinkFactory]:
//! each piece a file of its own, or all of them back to back in one file ([PieceFiles]).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{FinishState, OwnerId, PieceHead, PieceId, PieceIdent, TAIL_LEN, TrackStart};
use crate::index::IndexError;
use crate::reader::{ByteSource, TapeReader};
use crate::sink::{Files, SinkFactory, TapeSink};
use crate::writer::resume::SinkSource;
use crate::writer::{
    Finished, Newest, NextPiece, Placed, Resumed, TapeWriter, WriteError, WriterOptions, block_id,
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

/// What a piece may hold past [CutPolicy::bytes]: the chunk that takes it over the limit, the index, finish and tail.
pub const CUT_SLACK: u64 = 1 << 20;

/// Refuse a resumed piece of a chain that kept blocks but no piece head: its head is damaged, and a piece without
/// one has no link to the chain and can't be cut. A piece cut off inside its head keeps nothing and starts again.
fn headed<S: TapeSink>(writer: &TapeWriter<S>) -> Result<(), WriteError> {
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

/// Name of piece `seq` in its chain's directory: the sequence number in 8 digits, so names sort in chain order.
pub fn piece_name(seq: u64) -> String {
    format!("{seq:08}.wwt")
}

/// Where [PieceChain::write_chunk] placed the chunk: the seq of the piece it landed in ([PieceChain::seq] by the
/// time the call returns, past a cut when one happened) and where in that piece.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChunkPlaced {
    pub seq: u64,
    pub offset: u64,
    pub len: u32,
}

/// Where a [PieceChain] keeps its pieces.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PieceFiles {
    /// Each piece a file of its own, `dir/`[piece_name] (`seq`): the path the chain is given is the directory.
    #[default]
    Each,
    /// All pieces back to back in one file (or memory tape): the path the chain is given is the file. A cut starts
    /// the next piece right behind the last ([TapeWriter::cut_in_place]); a resume goes on in the last piece when
    /// it is unfinished, else starts a new one behind it.
    One,
}

/// Writes one recording as pieces on the sinks of a [SinkFactory] (files by default), each its own file
/// `dir/`[piece_name] (`seq`) or all in one ([PieceFiles]), cutting by a [CutPolicy] before a chunk.
pub struct PieceChain<F: SinkFactory = Files> {
    factory: F,
    /// The directory of the pieces, or the file holding them all
    at: PathBuf,
    files: PieceFiles,
    /// None after a failed cut: the chain takes no more blocks
    writer: Option<TapeWriter<F::Sink>>,
    cut: CutPolicy,
    started: Instant,
    seq: u64,
    finished: Vec<Finished>,
}

/// Options of a new [PieceChain].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ChainOptions {
    /// Each piece's writer.
    pub writer: WriterOptions,
    pub cut: CutPolicy,
    /// A file per piece, or all pieces in one.
    pub files: PieceFiles,
}

impl PieceChain<Files> {
    /// Start chain `chain` of `owner` in `dir` (which must exist; with [PieceFiles::One] the file to create) with
    /// its first piece, see [Self::create_on].
    pub fn create(
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
    ) -> Result<Self, WriteError> {
        Self::create_on(Files, dir, owner, chain, tracks, options)
    }

    /// Reopen chain `chain` of `owner` in `dir` after a crash, see [Self::resume_on].
    pub fn resume(
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError> {
        Self::resume_on(Files, dir, owner, chain, options)
    }
}

impl<F: SinkFactory> PieceChain<F> {
    /// Start chain `chain` of `owner` under `dir` with its first piece, on the sink `factory` makes, id
    /// [PieceId::first_of] `(owner, chain)`; `tracks` say where items of tracks carried over from elsewhere continue
    /// (empty for a new recording). Each chain of a owner takes its own number, so its pieces and session tag are its own.
    /// With [PieceFiles::One] `dir` is the file that holds every piece, made with [SinkFactory::create_whole].
    pub fn create_on(
        mut factory: F,
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        tracks: &[TrackStart],
        options: ChainOptions,
    ) -> Result<Self, WriteError> {
        let sink = match options.files {
            PieceFiles::Each => factory.create(&dir.join(piece_name(0)))?,
            PieceFiles::One => factory.create_whole(dir)?,
        };
        let mut writer = TapeWriter::create_on(sink, options.writer.sync);
        writer.apply(&options.writer)?;
        reserve_piece(&mut writer, options.cut);
        writer.begin_piece(&first_head(owner, chain), tracks)?;
        Ok(PieceChain {
            factory,
            at: dir.to_path_buf(),
            files: options.files,
            writer: Some(writer),
            cut: options.cut,
            started: Instant::now(),
            seq: 0,
            finished: Vec::new(),
        })
    }

    /// Reopen chain `chain` of `owner` under `dir` after a crash: the finished pieces are read from their tails
    /// ([Self::finished]), the unfinished last one is resumed ([TapeWriter::resume_on]). After a crash between a cut's finish
    /// and the next piece's start, the next piece is started as the cut would have. `options` are the ones the chain
    /// was written with. A piece ended whole whose name a crash took ([TapeWriter::resume_on]) is named and read as
    /// finished. A chain of a file per piece whose last piece ended complete or aborted is refused
    /// ([WriteError::Ended]).
    ///
    /// With [PieceFiles::One] `dir` is the file. Its last piece, when unfinished (a crash), is resumed with the
    /// same rules as a piece of its own: only a torn end is cut, and a resume that would drop more is refused. When
    /// every piece is finished (the file was closed), a new piece starts behind the last: the next one of the cut
    /// when the last is continued, else a new write session's ([TapeWriter::begin_piece] rules: its own session tag
    /// derived from its id, seqs from 0), its head naming the last piece. An application that starts every write
    /// session in a piece of its own gives the calls a resume asks for again, then cuts ([Self::cut_now]).
    pub fn resume_on(
        factory: F,
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError> {
        match options.files {
            PieceFiles::Each => Self::resume_each(factory, dir, owner, chain, options),
            PieceFiles::One => Self::resume_one(factory, dir, owner, chain, options),
        }
    }

    /// [Self::resume_on] of a chain whose pieces are files of their own.
    fn resume_each(
        mut factory: F,
        dir: &Path,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError> {
        let mut finished: Vec<(Finished, LastPiece)> = Vec::new();
        let mut seq = 0u64;
        let (path, reopened) = loop {
            let path = dir.join(piece_name(seq));
            let src = match factory.finished(&path)? {
                Some(src) => src,
                None => {
                    let Some(sink) = factory.reopen(&path)? else {
                        break (path, None);
                    };
                    match TapeWriter::resume_on(sink, options.writer.clone()) {
                        // ended whole before a crash took its name: named now, read as finished
                        Err(WriteError::Ended(_)) => factory.finished(&path)?.ok_or(
                            WriteError::Index(IndexError::Invalid(
                                "a piece given its name doesn't read as finished",
                            )),
                        )?,
                        r => break (path, Some(r?)),
                    }
                }
            };
            finished.push(read_finished(&src, &path)?);
            seq = seq.checked_add(1).ok_or_else(too_many)?;
        };
        let (mut writer, resumed) = match reopened {
            Some((mut w, resumed)) => {
                headed(&w)?;
                reserve_piece(&mut w, options.cut);
                if w.bytes() == 0 {
                    // cut off inside its head: the piece starts again (a first piece without carried-over tracks)
                    match finished.last() {
                        None => w.begin_piece(&first_head(owner, chain), &[])?,
                        Some((done, last)) => w.begin_next(next_piece(done, last)?)?,
                    }
                }
                (w, resumed)
            }
            None => {
                let Some((done, last)) = finished.last() else {
                    return Err(WriteError::Io {
                        what: "opening the first piece of the chain",
                        error: std::io::ErrorKind::NotFound.into(),
                    });
                };
                if last.state != FinishState::Continued {
                    return Err(WriteError::Ended(last.state));
                }
                let next = next_piece(done, last)?;
                let sink = factory.create(&path)?;
                let mut w = TapeWriter::start_next(sink, &options.writer, next)?;
                reserve_piece(&mut w, options.cut);
                let resumed = Resumed::started(w.bytes());
                (w, resumed)
            }
        };
        writer.take_newest(&newest_in(&finished));
        Ok((
            PieceChain {
                factory,
                at: dir.to_path_buf(),
                files: options.files,
                writer: Some(writer),
                cut: options.cut,
                started: Instant::now(),
                seq,
                finished: finished.into_iter().map(|(f, _)| f).collect(),
            },
            resumed,
        ))
    }

    /// [Self::resume_on] of a chain whose pieces are all in the file `path`.
    fn resume_one(
        mut factory: F,
        path: &Path,
        owner: OwnerId,
        chain: u64,
        options: ChainOptions,
    ) -> Result<(Self, Resumed), WriteError> {
        let Some(mut sink) = factory.open_whole(path)? else {
            return Err(WriteError::Io {
                what: "opening the file of the chain",
                error: std::io::ErrorKind::NotFound.into(),
            });
        };
        let size = sink
            .size()
            .map_err(crate::sink::io_err("reading the length to resume"))?;
        let finished = finished_in(&sink, path)?;
        let seq = u64::try_from(finished.len()).map_err(|_| too_many())?;
        // the bytes after the last finished piece: an unfinished piece, or nothing
        let open_at = match finished.last() {
            Some((f, _)) => f.start.checked_add(f.len).ok_or_else(too_many)?,
            None => 0,
        };
        sink.rebase(open_at)
            .map_err(crate::sink::io_err("finding the last piece"))?;
        let (mut writer, resumed) = if open_at < size || finished.is_empty() {
            let (mut w, resumed) = TapeWriter::resume_on(sink, options.writer.clone())?;
            headed(&w)?;
            reserve_piece(&mut w, options.cut);
            if w.bytes() == 0 {
                // cut off inside its head: the piece starts again
                match finished.last() {
                    None => w.begin_piece(&first_head(owner, chain), &[])?,
                    Some((done, last)) => w.begin_next(following(done, last)?)?,
                }
            }
            (w, resumed)
        } else {
            let Some((done, last)) = finished.last() else {
                return Err(WriteError::Index(IndexError::Invalid(
                    "a file of finished pieces holds one",
                )));
            };
            let mut w = TapeWriter::start_next(sink, &options.writer, following(done, last)?)?;
            reserve_piece(&mut w, options.cut);
            let resumed = Resumed::started(w.bytes());
            (w, resumed)
        };
        writer.take_newest(&newest_in(&finished));
        Ok((
            PieceChain {
                factory,
                at: path.to_path_buf(),
                files: options.files,
                writer: Some(writer),
                cut: options.cut,
                started: Instant::now(),
                seq,
                finished: finished.into_iter().map(|(f, _)| f).collect(),
            },
            resumed,
        ))
    }

    /// The piece being written; [WriteError::Failed] after a failed cut.
    pub fn writer(&mut self) -> Result<&mut TapeWriter<F::Sink>, WriteError> {
        self.writer.as_mut().ok_or(WriteError::Failed)
    }

    /// The pieces finished so far, those before a resume included.
    pub fn finished(&self) -> &[Finished] {
        &self.finished
    }

    /// Sequence number of the piece being written.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The factory making the pieces' sinks, for reading them ([crate::MemTapes::get]).
    pub fn factory(&self) -> &F {
        &self.factory
    }

    /// `call` on the writer; when the piece lists all the state entries it may ([IndexError::StateEntries]), again
    /// on a new piece. A call refused so has written nothing: it checks its room first.
    fn with_room<T>(
        &mut self,
        mut call: impl FnMut(&mut TapeWriter<F::Sink>) -> Result<T, WriteError>,
    ) -> Result<T, WriteError> {
        match call(self.writer()?) {
            Err(WriteError::Index(IndexError::StateEntries { .. })) => {
                self.cut_now()?;
                call(self.writer()?)
            }
            r => r,
        }
    }

    /// A record block ([TapeWriter::write_block]).
    pub fn write_block(&mut self, defs: &Definitions, block: &[u8]) -> Result<(), WriteError> {
        self.with_room(|w| w.write_block(defs, block)).map(|_| ())
    }

    /// Put a state track's item ([TapeWriter::put]).
    pub fn put(
        &mut self,
        defs: &Definitions,
        track: u32,
        ticks: i64,
        item: &[u8],
    ) -> Result<bool, WriteError> {
        self.with_room(|w| w.put(defs, track, ticks, item))
    }

    /// Put a typed item ([TapeWriter::put_item]).
    pub fn put_item(
        &mut self,
        defs: &Definitions,
        item: &crate::TypedItem<'_>,
    ) -> Result<bool, WriteError> {
        self.with_room(|w| w.put_item(defs, item))
    }

    /// A small block placed in the next gap ([TapeWriter::defer_block]).
    /// When the piece has no room for the state entries of the definitions it needs, on a new piece.
    pub fn defer_block(&mut self, defs: &Definitions, block: Vec<u8>) -> Result<(), WriteError> {
        // a small block: a copy for the retry costs little
        self.with_room(|w| w.defer_block(defs, block.clone()))
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
            || c.interval.is_some_and(|t| self.started.elapsed() >= t)
    }

    /// A chunk ([TapeWriter::write_chunk]), in a new piece when [Self::cut_due]: a cut happens before a chunk, never
    /// after the last one, so no piece is left empty. Where it landed.
    pub fn write_chunk(
        &mut self,
        defs: &Definitions,
        head: &ChunkHead,
        columns: &[(ColumnSpec, &[u8])],
    ) -> Result<ChunkPlaced, WriteError> {
        if self.cut_due() {
            self.cut_now()?;
        }
        let Placed { offset, len } = self.with_room(|w| w.write_chunk(defs, head, columns))?;
        Ok(ChunkPlaced {
            seq: self.seq,
            offset,
            len,
        })
    }

    /// Cut to a new piece now.
    pub fn cut_now(&mut self) -> Result<(), WriteError> {
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
        let (done, mut next) = match self.files {
            PieceFiles::Each => {
                current.cut_with(&mut self.factory, &self.at.join(piece_name(seq)))?
            }
            PieceFiles::One => current.cut_in_place()?,
        };
        reserve_piece(&mut next, self.cut);
        self.writer = Some(next);
        self.finished.push(done);
        self.seq = seq;
        self.started = Instant::now();
        Ok(())
    }

    /// Finish the last piece as complete; every piece of the chain.
    pub fn finish(self) -> Result<Vec<Finished>, WriteError> {
        self.finish_as(FinishState::Complete)
    }

    /// Finish the last piece in `state` (complete or aborted); every piece of the chain.
    pub fn finish_as(mut self, state: FinishState) -> Result<Vec<Finished>, WriteError> {
        if let Some(w) = self.writer.as_mut() {
            w.flush_deferred()?;
        }
        let last = self
            .writer
            .take()
            .ok_or(WriteError::Failed)?
            .finish_as(state)?;
        self.finished.push(last);
        Ok(self.finished)
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

/// The piece after the finished piece `done`, continued, as its cut would have started it.
fn next_piece(done: &Finished, last: &LastPiece) -> Result<NextPiece, WriteError> {
    NextPiece::after(
        &last.head,
        done,
        last.next_tracks.clone(),
        last.session,
        last.next_seq,
    )
}

/// The piece a writer starts after the finished piece `done`: the next of the cut when it is continued, else the
/// first of a new write session.
fn following(done: &Finished, last: &LastPiece) -> Result<NextPiece, WriteError> {
    if last.state == FinishState::Continued {
        next_piece(done, last)
    } else {
        NextPiece::reopened(&last.head, done, last.next_tracks.clone(), last.session)
    }
}

fn too_many() -> WriteError {
    WriteError::Index(IndexError::Invalid("piece seq past u64::MAX"))
}

/// The finished pieces at the start of the file in `sink` (named `path`), in order, read from their tails: every
/// piece but an unfinished last one. A piece without its end before another is refused.
fn finished_in<S: TapeSink>(
    sink: &S,
    path: &Path,
) -> Result<Vec<(Finished, LastPiece)>, WriteError> {
    let src = SinkSource(sink);
    let reader = TapeReader::open(&src).map_err(WriteError::Read)?;
    let pieces = reader.pieces();
    let done = pieces.iter().take_while(|p| p.tail.is_some()).count();
    if done.saturating_add(1) < pieces.len() {
        return Err(WriteError::Index(IndexError::Invalid(
            "an unfinished piece before the last one in a file of pieces",
        )));
    }
    (0..done)
        .map(|i| read_finished_at(&reader, i, path))
        .collect()
}

/// What the piece after a finished one starts from.
struct LastPiece {
    state: FinishState,
    head: PieceHead,
    next_tracks: Vec<TrackStart>,
    session: SessionTag,
    next_seq: u64,
    /// The tops of its chunks' ranges, for the monotone relations' newest values
    newest: Newest,
}

/// The finished piece in `src`, named `path`, from its tail, finish and head.
fn read_finished<B: ByteSource<Error = std::io::Error>>(
    src: &B,
    path: &Path,
) -> Result<(Finished, LastPiece), WriteError> {
    let reader = TapeReader::open(src).map_err(WriteError::Read)?;
    if !matches!(reader.pieces(), [p] if p.start == 0) {
        return Err(WriteError::Index(IndexError::Invalid(
            "a finished piece holds one piece",
        )));
    }
    read_finished_at(&reader, 0, path)
}

/// Finished piece `i` of `reader`, in the bytes named `path`, from its tail, finish and head.
fn read_finished_at<B: ByteSource<Error = std::io::Error>>(
    reader: &TapeReader<B>,
    i: usize,
    path: &Path,
) -> Result<(Finished, LastPiece), WriteError> {
    let bad = |what| WriteError::Index(IndexError::Invalid(what));
    let src = reader.source();
    let p = reader.pieces().get(i).ok_or(bad("no such piece"))?;
    if let Some(e @ (IndexError::UnhandledIntegrity { .. } | IndexError::UnknownIntegrity { .. })) =
        p.head_error
    {
        return Err(WriteError::Index(e));
    }
    let (Some(tail), Some((finish, ranges)), Some((head, starts))) =
        (p.tail, p.finish.as_ref(), p.head.as_ref())
    else {
        return Err(bad(
            "a finished piece of a chain has a head, a finish and a tail",
        ));
    };
    // the tail carries the session and seq of the last counted block of the piece
    let tail_at = p
        .end
        .checked_sub(crate::reader::wide(TAIL_LEN))
        .ok_or(bad("a piece shorter than its tail"))?;
    let mut header = [0u8; HEADER_LEN];
    src.read_at(tail_at, &mut header)
        .map_err(crate::sink::io_err("reading a tail"))?;
    let id = block_id(&header).ok_or(bad("a tail shorter than a header"))?;
    // the tail's seq is the last counted block's, or 0 (a first piece) or the piece before's when it holds none
    let next_seq = if head.prev.is_none() && !holds_counted(reader, i)? {
        0
    } else {
        id.seq.wrapping_add(1)
    };
    let next_tracks = crate::index::next_starts(starts, ranges);
    Ok((
        Finished {
            path: path.to_path_buf(),
            hash: finish.hash,
            len: tail.piece_len,
            ident: finish.ident,
            next: finish.next,
            start: p.start,
            tracks: next_tracks.clone(),
        },
        LastPiece {
            state: finish.state,
            head: *head,
            next_tracks,
            session: id.session,
            next_seq,
            newest: newest_of(reader, i, ranges)?,
        },
    ))
}

/// The tops of the ranges of the chunks of piece `i` of `reader`, whose finish lists `rows`: the newest values of
/// its tracks' range relations.
fn newest_of<B: ByteSource<Error = std::io::Error>>(
    reader: &TapeReader<B>,
    i: usize,
    rows: &[crate::piece::TrackRange],
) -> Result<Newest, WriteError> {
    let mut newest = Newest::default();
    for e in reader.entries(i).map_err(WriteError::Read)? {
        let Some((_, hi)) = e.range.filter(|_| e.kind == crate::BlockKind::Events) else {
            continue;
        };
        // the owner by the finish's row of the track; a track it doesn't list is of the piece's owner
        let owner = rows
            .iter()
            .find(|r| r.track_id == e.track_id && r.track_number == e.track_number)
            .map_or(OwnerId::NONE, |r| r.owner);
        newest.seed_range((e.track_id, owner), hi);
    }
    Ok(newest)
}

/// The newest values of the monotone relations of the pieces `finished`.
fn newest_in(finished: &[(Finished, LastPiece)]) -> Newest {
    let mut newest = Newest::default();
    for (_, last) in finished {
        newest.merge(&last.newest);
    }
    newest
}

/// Whether piece `i` of `reader` holds a block its writer counted: one the index lists that isn't marked
/// FLAG_REPEAT.
fn holds_counted<B: ByteSource<Error = std::io::Error>>(
    reader: &TapeReader<B>,
    i: usize,
) -> Result<bool, WriteError> {
    Ok(reader
        .entries(i)
        .map_err(WriteError::Read)?
        .iter()
        .any(|e| !e.repeat))
}
