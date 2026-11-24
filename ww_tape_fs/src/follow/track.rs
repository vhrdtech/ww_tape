//! One track's items from the blocks a follower hands out, decoded as their chunks land.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use shrink_wrap::prelude::*;
use ww_tape::chunk::{ChunkHead, ColumnKind};
use ww_tape::payloads::{Payloads, PayloadsError};
use ww_tape::piece::PieceHeadBlock;
use ww_tape::times::{EventTimes, TimesError};
use ww_tape::{
    Block, BlockBuf, BlockKind, Definitions, Describe, FlagRuns, FlagRunsError, ItemNum, OwnerId,
    Sample, SampleError, SampleShape, Samples, SeriesGrid, Slot, TrackId,
};

use super::{ChainFollower, FollowError, Followed};

/// An item of a track, or a run of them, as a [TrackItems] gives it.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackItem {
    /// An item of an events track.
    Event {
        item: ItemNum,
        /// Its start and end in ticks of the track's clock; the end is the start for an instant.
        start: i64,
        end: i64,
        /// Its flags, 0 for a track without them.
        flags: u32,
        /// Its payload, empty for a track without payloads.
        payload: Vec<u8>,
    },
    /// The samples of a series chunk.
    Samples(Box<SeriesSamples>),
    /// Items the chunks skip: a writer dropped them ([ww_tape_mux::Pressure::Drop]), or their chunk was lost.
    Lost { first: ItemNum, count: u64 },
}

/// The samples of one chunk of a series track, decoded on demand ([Self::decode]).
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesSamples {
    /// The first sample's number on the track, and how many there are.
    pub first: ItemNum,
    pub count: u32,
    /// The grid and scale of the samples: sample `i` lies at `grid_tick + (i - grid_index) * period`, the period
    /// the track's [ww_tape::GridDef] states.
    pub grid: SeriesGrid,
    /// The samples' shape, from the track's definition.
    pub shape: SampleShape,
    block: BlockBuf,
}

impl SeriesSamples {
    /// The samples, lanes interleaved: `count` × lanes values of the track's element type `T`.
    pub fn decode<T: Sample>(&self) -> Result<Vec<T>, TrackError> {
        let chunk = self.block.block().chunk().ok_or(TrackError::NoSamples)?;
        let column = chunk
            .find(ColumnKind::Samples, 0)
            .ok_or(TrackError::NoSamples)?;
        let count = usize::try_from(self.count)
            .ok()
            .ok_or(TrackError::NoSamples)?;
        let samples = Samples::of(&column, self.shape, count).map_err(TrackError::Samples)?;
        let len = count
            .checked_mul(samples.lanes())
            .ok_or(TrackError::NoSamples)?;
        let mut out = vec![T::default(); len];
        samples.decode(0, &mut out).map_err(TrackError::Samples)?;
        Ok(out)
    }

    /// The flags of the samples, as runs of samples sharing a word ([ww_tape::Chunk::flag_runs]); a sample in no
    /// run has the word 0.
    pub fn flag_runs(&self) -> Result<FlagRuns<'_>, TrackError> {
        let chunk = self.block.block().chunk().ok_or(TrackError::NoSamples)?;
        chunk.flag_runs().map_err(TrackError::FlagRuns)
    }
}

/// Why a chunk of the track did not decode.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrackError {
    Times(TimesError),
    Payloads(PayloadsError),
    Samples(SampleError),
    FlagRuns(FlagRunsError),
    /// A series chunk whose track's definition isn't held, or names no numbers: its samples have no shape.
    NoShape,
    /// A series chunk without a samples column.
    NoSamples,
}

impl core::fmt::Display for TrackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Times(_) => write!(f, "the chunk's times don't decode"),
            Self::Payloads(_) => write!(f, "the chunk's payloads don't decode"),
            Self::Samples(_) => write!(f, "the chunk's samples don't decode"),
            Self::FlagRuns(_) => write!(f, "the chunk's flag runs don't decode"),
            Self::NoShape => write!(f, "a series chunk of a track whose definition isn't held"),
            Self::NoSamples => write!(f, "a series chunk without samples"),
        }
    }
}

impl core::error::Error for TrackError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Times(e) => Some(e),
            Self::Payloads(e) => Some(e),
            Self::Samples(e) => Some(e),
            Self::FlagRuns(e) => Some(e),
            Self::NoShape | Self::NoSamples => None,
        }
    }
}

/// Picks one track's chunks out of the blocks a [ChainFollower] hands out and decodes them, in item order.
///
/// Chunks of one track come in item order; a jump in item numbers is a [TrackItem::Lost]. A chunk wholly before
/// the next item expected is passed over.
#[derive(Clone, Debug)]
pub struct TrackItems {
    id: TrackId,
    /// Only the chunks of this owner's track, when set
    owner: Option<OwnerId>,
    /// The owner the last piece head named: whose track a definition of all-zero owner is
    pub(super) piece_owner: OwnerId,
    next_item: Option<ItemNum>,
    from_tick: Option<i64>,
}

impl TrackItems {
    /// The items of the track of id `id` ([ww_tape::TrackKey::id]), whoever owns it ([ww_tape::TrackDef::owner]).
    pub fn new(id: TrackId) -> Self {
        Self {
            id,
            owner: None,
            piece_owner: OwnerId::NONE,
            next_item: None,
            from_tick: None,
        }
    }

    /// The items of the track of id `id` and owner `owner`: chunks of the same id another owner wrote into the chain
    /// are passed over. A definition whose owner is all zeros is the owner's of its piece ([ww_tape::PieceIdent]).
    pub fn owned(id: TrackId, owner: OwnerId) -> Self {
        Self {
            owner: Some(owner),
            ..Self::new(id)
        }
    }

    /// Leave out items that start before `tick`, and samples of chunks that end before it.
    #[must_use]
    pub fn from_tick(mut self, tick: i64) -> Self {
        self.from_tick = Some(tick);
        self
    }

    /// The item number after the last item given; None before the first chunk.
    pub fn next_item(&self) -> Option<ItemNum> {
        self.next_item
    }

    /// Add the items of `block` to `out` when it is a chunk of the track; `defs` are the definitions held at it
    /// ([ChainFollower::definitions]), for a series track's shape.
    pub fn take(
        &mut self,
        block: &BlockBuf,
        defs: &Definitions,
        out: &mut VecDeque<TrackItem>,
    ) -> Result<(), TrackError> {
        let b = block.block();
        if b.kind == BlockKind::PieceHead
            && let Ok(h) = PieceHeadBlock::read(&b)
        {
            self.piece_owner = h.head().ident.owner;
        }
        let Some(chunk) = b.chunk() else {
            return Ok(());
        };
        let head = chunk.head();
        // an overview chunk holds statistics of time steps its track's chunks give
        if head.track_id != self.id || head.level != 0 || !self.owns(defs, &head) {
            return Ok(());
        }
        let end = head.first_item.checked_add(u64::from(head.count));
        if let Some(next) = self.next_item {
            if end.is_none_or(|e| e <= next) {
                return Ok(());
            }
            let skipped = next.offset_to(head.first_item).unwrap_or(0);
            if skipped > 0 {
                out.push_back(TrackItem::Lost {
                    first: next,
                    count: skipped,
                });
            }
        }
        let skip = self
            .next_item
            .and_then(|n| head.first_item.offset_to(n))
            .unwrap_or(0);
        match head.kind {
            BlockKind::Series => self.series(block, &head, defs, out)?,
            _ => self.events(&b, &head, skip, out)?,
        }
        self.next_item = end;
        Ok(())
    }

    /// Whether the chunk of `head` is of the owner followed; a chunk whose track `defs` don't hold is of none.
    fn owns(&self, defs: &Definitions, head: &ChunkHead) -> bool {
        let Some(want) = self.owner else {
            return true;
        };
        match defs.track_identity(head.track_number) {
            Some((_, owner)) if owner == [0; 32] => self.piece_owner == want,
            Some((_, owner)) => OwnerId(owner) == want,
            None => false,
        }
    }

    fn events(
        &self,
        b: &Block<'_>,
        head: &ChunkHead,
        skip: u64,
        out: &mut VecDeque<TrackItem>,
    ) -> Result<(), TrackError> {
        let Some(chunk) = b.chunk() else {
            return Ok(());
        };
        let times = EventTimes::read(&chunk).map_err(TrackError::Times)?;
        let payloads = Payloads::read(&chunk).map_err(TrackError::Payloads)?;
        let skip = usize::try_from(skip).unwrap_or(usize::MAX);
        for i in skip..times.len() {
            let start = times.start(i);
            if self.from_tick.is_some_and(|t| start < t) {
                continue;
            }
            let Some(item) = head.first_item.checked_add(crate::follow::wide(i)) else {
                break;
            };
            out.push_back(TrackItem::Event {
                item,
                start,
                end: times.end(i),
                flags: times.flag(i),
                payload: payloads
                    .as_ref()
                    .and_then(|p| p.get(i))
                    .unwrap_or_default()
                    .to_vec(),
            });
        }
        Ok(())
    }

    fn series(
        &self,
        block: &BlockBuf,
        head: &ChunkHead,
        defs: &Definitions,
        out: &mut VecDeque<TrackItem>,
    ) -> Result<(), TrackError> {
        if self.from_tick.is_some_and(|t| head.latest_end < t) {
            return Ok(());
        }
        let grid = head.grid.ok_or(TrackError::NoSamples)?;
        let shape = defs
            .get(&Slot::Track {
                number: head.track_number,
            })
            .and_then(|item| match Describe::from_ww_bytes(item) {
                Ok(Describe::Track(t)) => t.series_shape(),
                _ => None,
            })
            .ok_or(TrackError::NoShape)?;
        out.push_back(TrackItem::Samples(Box::new(SeriesSamples {
            first: head.first_item,
            count: head.count,
            grid,
            shape,
            block: block.clone(),
        })));
        Ok(())
    }
}

/// A [ChainFollower] that gives one track's items ([ChainFollower::track]).
pub struct TrackFollower {
    chain: ChainFollower,
    items: TrackItems,
    pending: VecDeque<TrackItem>,
}

impl TrackFollower {
    pub(crate) fn new(chain: ChainFollower, mut items: TrackItems) -> Self {
        // the piece head may lie before the first block handed out ([FollowStart::End](super::FollowStart::End));
        // each piece head handed out after it names its own piece's owner
        if let Some(owner) = chain.owner() {
            items.piece_owner = owner;
        }
        Self {
            chain,
            items,
            pending: VecDeque::new(),
        }
    }

    /// The next item when one is there now, else None; never waits.
    pub fn try_next(&mut self) -> Result<Option<TrackItem>, FollowError> {
        loop {
            if let Some(i) = self.pending.pop_front() {
                return Ok(Some(i));
            }
            let Some(f) = self.chain.try_next()? else {
                return Ok(None);
            };
            self.take(&f)?;
        }
    }

    /// The next item, waiting as long as it takes; None once the chain is closed and nothing follows.
    pub fn wait_next(&mut self) -> Result<Option<TrackItem>, FollowError> {
        loop {
            if let Some(i) = self.pending.pop_front() {
                return Ok(Some(i));
            }
            let Some(f) = self.chain.wait_next()? else {
                return Ok(None);
            };
            self.take(&f)?;
        }
    }

    /// [Self::wait_next] as a future, for a task; see [ChainFollower::wait_next_async]. Dropped while it waits, it
    /// takes nothing.
    pub async fn wait_next_async(&mut self) -> Result<Option<TrackItem>, FollowError> {
        loop {
            if let Some(i) = self.pending.pop_front() {
                return Ok(Some(i));
            }
            let Some(f) = self.chain.wait_next_async().await? else {
                return Ok(None);
            };
            self.take(&f)?;
        }
    }

    /// The next item, waiting `timeout` at most; None when none came, or the chain is closed.
    pub fn wait_next_for(&mut self, timeout: Duration) -> Result<Option<TrackItem>, FollowError> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(i) = self.pending.pop_front() {
                return Ok(Some(i));
            }
            let left = deadline
                .and_then(|d| d.checked_duration_since(Instant::now()))
                .unwrap_or_default();
            let Some(f) = self.chain.wait_next_for(left)? else {
                return Ok(None);
            };
            self.take(&f)?;
        }
    }

    /// The chain follower underneath.
    pub fn chain(&self) -> &ChainFollower {
        &self.chain
    }

    fn take(&mut self, f: &Followed) -> Result<(), FollowError> {
        self.items
            .take(&f.block, self.chain.definitions(), &mut self.pending)
            .map_err(|error| FollowError::Track {
                offset: f.offset,
                error,
            })
    }
}
