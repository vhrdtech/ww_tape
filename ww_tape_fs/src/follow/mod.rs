//! Following a chain while another process writes it: [ChainFollower] hands out each block once it checks, piece
//! after piece.
//!
//! A follower reads the piece being written by its valid prefix ([FileBlocks]) and remembers where that prefix ends
//! ([WalkPoint]). When the file grows it reads on from there; what it read past the point and didn't hand out (a
//! block the writer is still writing, or a torn end a crash left) is read again. A writer that resumes after a crash
//! cuts back that torn end, and the blocks that wait for one after them: puts that belong with the next block, and a
//! piece's end before its tail. The follower holds such blocks back until the block they wait for checks, then hands
//! them out with it. So it never hands out a block that doesn't check, and never one that a resume took back.
//!
//! At the tail of a piece whose close names a next piece, the follower moves to the next piece's file
//! ([crate::piece_name]); in a chain of one file ([PieceTapes::One]) the next piece follows in the same file. A piece
//! closed and written on again ([append](ww_tape::piece)) reads on after its tail. It holds the definitions the
//! blocks carry ([ChainFollower::definitions]), so a reader decodes what it hands out ([TrackFollower]).
//!
//! It wakes on inotify where there is one (Linux), else it looks again every [FollowOptions::poll]; the poll also
//! bounds a wait under inotify, for file systems that don't report changes. A task waits as a thread does, in
//! [ChainFollower::wait_next_async] and [TrackFollower::wait_next_async]: a thread of the follower's own does the
//! waiting and wakes it, so they need no async runtime.
//!
//! Several chains, one per process, merge by time in a [TimeMerge].

mod merge;
mod track;
mod wake;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ww_tape::index::{IndexBlock, IndexKind};
use ww_tape::piece::{PieceHeadBlock, Tail};
use ww_tape::{
    BlockBuf, BlockKind, Cut, Definitions, DescribeError, FLAG_WITH_NEXT, FileBlocks, FileError,
    Medium, OwnerId, PieceTapes, RecordTrack, WalkPoint,
};

pub use merge::{Merged, TimeMerge};
pub use track::{SeriesSamples, TrackError, TrackFollower, TrackItem, TrackItems};

use crate::piece_name;
use wake::Wake;

/// Default of [FollowOptions::poll].
pub const POLL_DEFAULT: Duration = Duration::from_millis(100);

/// Where a [ChainFollower] starts.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FollowStart {
    /// At the first block of the oldest piece there.
    #[default]
    First,
    /// At the first block of the newest piece there.
    Newest,
    /// After the blocks there now: only blocks written later are handed out. The definitions of the newest piece
    /// are held from the start.
    End,
}

/// Options of a [ChainFollower].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FollowOptions {
    pub start: FollowStart,
    /// How long a wait lasts at most before the follower looks again, also under inotify.
    pub poll: Duration,
    /// Whether the chain keeps each piece in a file of its own in a directory, or every piece in one file.
    pub tapes: PieceTapes,
}

impl Default for FollowOptions {
    fn default() -> Self {
        Self {
            start: FollowStart::First,
            poll: POLL_DEFAULT,
            tapes: PieceTapes::Each,
        }
    }
}

/// A block a [ChainFollower] handed out: it checks, and it lies in the valid prefix of its piece.
#[derive(Clone, Debug)]
pub struct Followed {
    /// The sequence number of its piece file (0 with [PieceTapes::One]: every piece is in one file).
    pub piece: u64,
    /// Where it starts in its file.
    pub offset: u64,
    pub block: BlockBuf,
}

/// Why a [ChainFollower] could not read on.
#[derive(Debug)]
#[non_exhaustive]
pub enum FollowError {
    /// Reading the chain's directory or a piece file failed; the next call tries again.
    Io(std::io::Error),
    /// The definitions of a block handed out don't hold together with those before (a writer bug or damage): the
    /// block is handed out still, the definitions before it stay.
    Definitions { offset: u64, error: DescribeError },
    /// A chunk of the track a [TrackFollower] follows, at `offset`, didn't decode; the follower reads on after it.
    Track { offset: u64, error: TrackError },
}

impl core::fmt::Display for FollowError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "reading the chain: {e}"),
            Self::Definitions { offset, .. } => {
                write!(
                    f,
                    "the definitions of the block at byte {offset} don't hold"
                )
            }
            Self::Track { offset, .. } => write!(f, "the chunk at byte {offset} doesn't decode"),
        }
    }
}

impl core::error::Error for FollowError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Definitions { error, .. } => Some(error),
            Self::Track { error, .. } => Some(error),
        }
    }
}

impl From<std::io::Error> for FollowError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Follows a chain in a directory (or one file) while a writer in any process writes it; see the [module
/// docs](self).
pub struct ChainFollower {
    path: PathBuf,
    options: FollowOptions,
    /// The piece read now; None until one is there.
    piece: Option<u64>,
    file: Option<File>,
    /// Where the walk of the piece goes on.
    point: WalkPoint,
    defs: Definitions,
    /// Blocks read and not yet handed out, and errors met reading them, in file order.
    ready: VecDeque<Result<Followed, FollowError>>,
    /// The last block read is the tail of a close that names no next piece.
    closed: bool,
    /// Why the last walk stopped short of the end of its file.
    stopped: Option<Cut>,
    /// The owner the newest piece head read names
    owner: Option<OwnerId>,
    wake: Wake,
}

impl ChainFollower {
    /// Follow the chain at `path`: its directory, or with [PieceTapes::One] its file. Nothing needs to be there yet.
    pub fn new(path: &Path, options: FollowOptions) -> Result<Self, FollowError> {
        let watched = match options.tapes {
            PieceTapes::Each => path,
            PieceTapes::One => path.parent().unwrap_or(Path::new(".")),
        };
        let mut f = Self {
            path: path.to_path_buf(),
            options,
            piece: None,
            file: None,
            point: WalkPoint::start(Medium::File),
            defs: Definitions::new(),
            ready: VecDeque::new(),
            closed: false,
            stopped: None,
            owner: None,
            wake: Wake::new(watched),
        };
        match options.start {
            FollowStart::First => {}
            FollowStart::Newest => f.piece = f.newest()?,
            FollowStart::End => {
                f.piece = f.newest()?;
                f.read_more()?;
                f.ready.clear();
            }
        }
        Ok(f)
    }

    /// The next block when one is there now, else None; never waits.
    pub fn try_next(&mut self) -> Result<Option<Followed>, FollowError> {
        if self.ready.is_empty() {
            self.read_more()?;
        }
        self.ready.pop_front().transpose()
    }

    /// The next block, waiting for it as long as it takes; None once the chain is closed and nothing follows the
    /// close. (A writer may still write on after a close; a later call reads on.)
    pub fn wait_next(&mut self) -> Result<Option<Followed>, FollowError> {
        loop {
            if let Some(f) = self.try_next()? {
                return Ok(Some(f));
            }
            if self.closed {
                return Ok(None);
            }
            self.wake.wait(self.options.poll);
        }
    }

    /// [Self::wait_next] as a future, for a task.
    ///
    /// A thread of the follower's own waits for the files to change and wakes the task: no runtime is needed, and
    /// none is blocked. Dropped while it waits, it takes nothing: a timeout of the caller's runtime bounds the wait,
    /// as [Self::wait_next_for] does for a thread.
    pub async fn wait_next_async(&mut self) -> Result<Option<Followed>, FollowError> {
        loop {
            if let Some(f) = self.try_next()? {
                return Ok(Some(f));
            }
            if self.closed {
                return Ok(None);
            }
            self.wake.changed(self.options.poll).await?;
        }
    }

    /// The next block, waiting for it `timeout` at most; None when none came, or the chain is closed.
    pub fn wait_next_for(&mut self, timeout: Duration) -> Result<Option<Followed>, FollowError> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(f) = self.try_next()? {
                return Ok(Some(f));
            }
            let left = deadline.and_then(|d| d.checked_duration_since(Instant::now()));
            let Some(left) = left.filter(|l| !l.is_zero()) else {
                return Ok(None);
            };
            if self.closed {
                return Ok(None);
            }
            self.wake.wait(left.min(self.options.poll));
        }
    }

    /// Follow the items of the track of id `id` from here on, whoever owns it ([TrackItems]).
    pub fn track(self, id: ww_tape::TrackId) -> TrackFollower {
        TrackFollower::new(self, TrackItems::new(id))
    }

    /// Follow the items of the track of id `id` and owner `owner` ([TrackItems::owned]): in a chain several owners
    /// write, the chunks of another owner's track of the same id are passed over.
    pub fn track_of(self, id: ww_tape::TrackId, owner: ww_tape::OwnerId) -> TrackFollower {
        TrackFollower::new(self, TrackItems::owned(id, owner))
    }

    /// The definitions the blocks handed out so far carry (and with [FollowStart::End] those of the piece before).
    pub fn definitions(&self) -> &Definitions {
        &self.defs
    }

    /// The owner the newest piece head read names ([ww_tape::PieceIdent::owner]), also one read before the blocks
    /// handed out ([FollowStart::End]); None before the first.
    pub fn owner(&self) -> Option<OwnerId> {
        self.owner
    }

    /// The piece read now; None while there is none.
    pub fn piece(&self) -> Option<u64> {
        self.piece
    }

    /// Whether the last block read is the tail of a close that names no next piece: the chain is closed, unless a
    /// writer writes on.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Why the last read stopped short of the end of the file: [Cut::Truncated] while a writer writes a block, or
    /// at a torn end a resume will cut back; another cut at damage a writer won't write past. None when it read to
    /// the end.
    pub fn stopped(&self) -> Option<Cut> {
        self.stopped
    }

    /// Whether a change of the files wakes a wait (inotify), or only the poll interval does.
    pub fn is_woken(&self) -> bool {
        self.wake.watches()
    }

    /// The file of piece `seq`.
    fn path_of(&self, seq: u64) -> PathBuf {
        match self.options.tapes {
            PieceTapes::Each => self.path.join(piece_name(seq)),
            PieceTapes::One => self.path.clone(),
        }
    }

    /// The sequence numbers of the piece files there, ascending (only 0 with [PieceTapes::One], when the file is).
    fn seqs(&self) -> Result<Vec<u64>, FollowError> {
        if self.options.tapes == PieceTapes::One {
            return Ok(if self.path.exists() { vec![0] } else { vec![] });
        }
        let entries = match std::fs::read_dir(&self.path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut seqs = Vec::new();
        for e in entries {
            let name = e?.file_name();
            let Some(seq) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".wwt"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            if name.to_str() == Some(piece_name(seq).as_str()) {
                seqs.push(seq);
            }
        }
        seqs.sort_unstable();
        Ok(seqs)
    }

    fn newest(&self) -> Result<Option<u64>, FollowError> {
        Ok(self.seqs()?.last().copied())
    }

    /// Read the blocks that came since the last read into `ready`, moving on to the next piece at a tail that names
    /// one.
    fn read_more(&mut self) -> Result<(), FollowError> {
        loop {
            if self.piece.is_none() {
                self.piece = self.seqs()?.first().copied();
            }
            let Some(piece) = self.piece else {
                return Ok(());
            };
            let file = match self.file.take() {
                Some(f) => f,
                None => match File::open(self.path_of(piece)) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(e.into()),
                },
            };
            let next_piece = self.walk(piece, &file);
            self.file = Some(file);
            match next_piece? {
                Some(next) if self.options.tapes == PieceTapes::Each => {
                    self.piece = Some(next);
                    self.file = None;
                    self.point = WalkPoint::start(Medium::File);
                }
                _ => return Ok(()),
            }
        }
    }

    /// Walk `file` of piece `piece` from the point on; the next piece's seq when a tail naming one ended the walk.
    ///
    /// A block a resume may still take back ([held_back]) is handed out with the block it waits for; until that
    /// block checks, the point stays before it and the next walk reads it again.
    fn walk(&mut self, piece: u64, mut file: &File) -> Result<Option<u64>, FollowError> {
        file.seek(SeekFrom::Start(self.point.offset()))?;
        let mut walk = FileBlocks::resume(file, self.point);
        self.stopped = None;
        let mut held: Vec<(u64, BlockBuf)> = Vec::new();
        while let Some(next) = walk.next_block() {
            let bytes = match next {
                Ok(b) if b.is_intact() => b.bytes().to_vec(),
                // a Prefix walk hands out no damaged chunk; were it to, it would not be handed on
                Ok(_) => break,
                Err(FileError::Io(e)) => return Err(e.into()),
                Err(FileError::Cut { why, .. }) => {
                    self.stopped = Some(why);
                    break;
                }
                // a gap, a block stepped over, a later copy that comes next: the walk goes on
                Err(_) => continue,
            };
            let offset = walk.offset();
            let Ok(block) = BlockBuf::new(bytes) else {
                break;
            };
            let Some(point) = walk.point() else {
                break;
            };
            if held_back(&block) {
                held.push((offset, block));
                continue;
            }
            self.point = point;
            let mut next_piece = None;
            for (offset, block) in held.drain(..).chain([(offset, block)]) {
                next_piece = self.hand_out(piece, offset, block);
            }
            if next_piece.is_some() {
                return Ok(next_piece);
            }
        }
        Ok(None)
    }

    /// Take the definitions of `block` at `offset` of `piece`, note a close, and queue it; the next piece's seq when
    /// it is a tail naming one.
    fn hand_out(&mut self, piece: u64, offset: u64, block: BlockBuf) -> Option<u64> {
        let b = block.block();
        self.closed = false;
        let mut next_piece = None;
        match b.kind {
            BlockKind::Records => {
                let mut buf = Vec::new();
                let held = b
                    .decode(&mut buf)
                    .map_err(DescribeError::Block)
                    .and_then(|d| match d.first_track() {
                        Some(RecordTrack::Definitions) => self.defs.take_block(&d),
                        _ => Ok(0),
                    });
                if let Err(error) = held {
                    self.ready
                        .push_back(Err(FollowError::Definitions { offset, error }));
                }
            }
            BlockKind::PieceHead => {
                if let Ok(h) = PieceHeadBlock::read(&b) {
                    self.owner = Some(h.head().ident.owner);
                }
            }
            BlockKind::Tail => match Tail::read(block.bytes()).map(|t| t.next) {
                Ok(Some(_)) => next_piece = piece.checked_add(1),
                Ok(None) => self.closed = true,
                Err(_) => {}
            },
            _ => {}
        }
        self.ready.push_back(Ok(Followed {
            piece,
            offset,
            block,
        }));
        next_piece
    }
}

/// Whether a resume after a crash right after `block` takes it back ([TapeWriter::resume_on]): puts that belong with
/// the block after them ([FLAG_WITH_NEXT]), and what ends a piece before its tail (a close summary, a
/// final index, a close).
///
/// [TapeWriter::resume_on]: ww_tape::TapeWriter::resume_on
fn held_back(block: &BlockBuf) -> bool {
    let b = block.block();
    if b.flags & FLAG_WITH_NEXT != 0 {
        return true;
    }
    match b.kind {
        BlockKind::CloseSummary | BlockKind::Close => true,
        BlockKind::Index => IndexBlock::read(&b).is_ok_and(|ix| ix.kind() == IndexKind::Final),
        _ => false,
    }
}

/// `n` as a u64, which holds every usize.
pub(crate) fn wide(n: usize) -> u64 {
    const { assert!(usize::BITS <= u64::BITS, "a usize fits a u64") };
    #[expect(clippy::as_conversions, reason = "a usize fits a u64, asserted above")]
    let w = n as u64;
    w
}
