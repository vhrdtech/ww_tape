//! The events handle: items of one events track encoded into chunks on the producer's thread.

use std::sync::Arc;

use shrink_wrap::DeserializeShrinkWrap;
use std::time::Duration;

use exact_units::Bytes;
use ww_tape::chunk::Chunk;
use ww_tape::payloads::{EncodedPayloads, PayloadCodec};
use ww_tape::times::{EncodedRelation, EncodedTimes, EventTimes};
use ww_tape::{ColumnSpec, Describe, Ends, ItemNum, Tapes, TrackDef, TrackId, TrackKind, TrackNum};
use ww_tape_summary::{EventFields, EventRuns};

use crate::TapeMux;
use crate::error::{CallError, EncodeError, ItemError, MuxError};
use crate::handle::{Backlog, Pressure, Sender, Sent, common};
use crate::open::{Buffers, Completed, Encoder, Open};
use crate::summaries::ChunkSummary;

/// Default of [EventsOptions::chunk_items].
pub const CHUNK_ITEMS_DEFAULT: u32 = 4096;

/// Default of [EventsOptions::chunk_bytes] and [SeriesOptions::chunk_bytes](crate::SeriesOptions::chunk_bytes).
pub const CHUNK_BYTES_DEFAULT: Bytes<u32> = Bytes::new(1 << 20);

/// Default of [EventsOptions::latency] and [SeriesOptions::latency](crate::SeriesOptions::latency).
pub const LATENCY_DEFAULT: Duration = Duration::from_secs(1);

/// The longest payload of one item an events handle takes: a chunk holds it alone.
pub const ITEM_BYTES_MAX: usize = 16 << 20;

/// How an [EventsHandle] cuts its items into chunks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EventsOptions {
    /// Items of a chunk at most, 1 to [ITEMS_MAX](ww_tape::times::ITEMS_MAX).
    pub chunk_items: u32,
    /// Payload bytes of a chunk at most; an item that would pass it completes the chunk before it.
    pub chunk_bytes: Bytes<u32>,
    /// How long the first item of an open chunk waits at most before the writing thread completes the chunk; None:
    /// until the chunk is full or completed by the producer.
    pub latency: Option<Duration>,
    /// Whether the mux builds the track's item tree, each piece's in that piece.
    pub summaries: bool,
    /// The track's first item: past 0 when a resumed chain goes on with the track.
    pub first_item: ItemNum,
    /// How each chunk's payloads column is coded: [PayloadCodec::Auto] tries LZ4 and keeps it when smaller,
    /// [PayloadCodec::Raw] runs no encoder (payloads that don't compress). A codec this crate doesn't know is refused
    /// when the handle opens.
    pub payload_codec: PayloadCodec,
}

impl Default for EventsOptions {
    fn default() -> Self {
        Self {
            chunk_items: CHUNK_ITEMS_DEFAULT,
            chunk_bytes: CHUNK_BYTES_DEFAULT,
            latency: Some(LATENCY_DEFAULT),
            summaries: true,
            first_item: ItemNum::new(0),
            payload_codec: PayloadCodec::Auto,
        }
    }
}

/// One item of an events track, as [EventsHandle::push_all] takes it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Event<'a> {
    /// The tick it starts at.
    pub start: i64,
    /// The tick it ends at: the start for an instant.
    pub end: i64,
    /// Its flags: 0 on a track without flags.
    pub flags: u32,
    /// Its payload, [ITEM_BYTES_MAX] bytes at most.
    pub payload: &'a [u8],
    /// Its value of each relation the track keeps values of, in the order of the track's relations.
    pub values: &'a [u64],
}

/// The length of a payload checked against [ITEM_BYTES_MAX].
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "a payload checked holds ITEM_BYTES_MAX bytes at most: it fits a u32"
)]
fn payload_len(payload: &[u8]) -> u32 {
    const {
        assert!(
            ITEM_BYTES_MAX <= u32::MAX as usize,
            "a payload length fits a u32"
        )
    };
    debug_assert!(payload.len() <= ITEM_BYTES_MAX, "a payload checked");
    payload.len() as u32
}

/// Copy `src` into `dst` of its length: one of 4 to 16 bytes as two overlapping words, not a call to `memcpy`.
fn copy_payload(dst: &mut [u8], src: &[u8]) {
    fn words<const N: usize>(dst: &mut [u8], src: &[u8]) {
        if let (Some(d), Some(s)) = (dst.first_chunk_mut::<N>(), src.first_chunk::<N>()) {
            *d = *s;
        }
        if let (Some(d), Some(s)) = (dst.last_chunk_mut::<N>(), src.last_chunk::<N>()) {
            *d = *s;
        }
    }
    debug_assert_eq!(dst.len(), src.len(), "a payload's room");
    match src.len() {
        8..=16 => words::<8>(dst, src),
        4..=7 => words::<4>(dst, src),
        _ => dst.copy_from_slice(src),
    }
}

/// A relation the track keeps values of: its index and whether its values never decrease.
#[derive(Copy, Clone)]
struct Stored {
    relation: u16,
    monotone: bool,
}

/// The open chunk of an events track.
pub(crate) struct EventsChunk {
    track: TrackNum,
    track_id: TrackId,
    fields: EventFields,
    stored: Vec<Stored>,
    chunk_items: usize,
    chunk_bytes: usize,
    payload_codec: PayloadCodec,
    /// The first item of the open chunk
    next_item: u64,
    starts: Vec<i64>,
    ends: Vec<i64>,
    flags: Vec<u32>,
    payload: Vec<u8>,
    /// The length of each payload in [Self::payload]
    lens: Vec<u32>,
    /// Per stored relation, its values
    values: Vec<Vec<u64>>,
    last_start: Option<i64>,
    /// Per stored relation, the last value
    last_values: Vec<Option<u64>>,
    runs: Option<EventRuns>,
}

impl EventsChunk {
    fn items(&self) -> usize {
        self.starts.len()
    }

    /// The number the next item gets.
    fn item_num(&self) -> ItemNum {
        let n = u64::try_from(self.items()).unwrap_or(u64::MAX);
        ItemNum::new(self.next_item.saturating_add(n))
    }

    /// How many items from the front of `items` the open chunk takes, each checked against the track and the items
    /// before; why the next one is refused when that is where they stop. An item that would pass the chunk's items
    /// stops them unchecked, one that would pass its bytes checked: the chunk is completed before it.
    ///
    /// One pass finds where the chunk is full and whether any item before is refused, without a branch per check;
    /// only then [Self::take_one_by_one] finds which.
    fn take(&self, items: &[Event<'_>]) -> (usize, Option<ItemError>) {
        let count = self.items();
        let run = items
            .get(..self.chunk_items.saturating_sub(count))
            .unwrap_or(items);
        let mut bytes = self.payload.len();
        let mut last = self.last_start.unwrap_or(i64::MIN);
        let mut refused = false;
        let mut n = run.len();
        for (i, it) in run.iter().enumerate() {
            bytes = bytes.saturating_add(it.payload.len());
            if bytes > self.chunk_bytes && (count > 0 || i > 0) {
                n = i;
                break;
            }
            refused |= self.refuses(it, last);
            last = it.start;
        }
        let taken = run.get(..n).unwrap_or_default();
        // the item past the bytes is refused before the chunk is completed for it
        let next_refused = n < run.len() && run.get(n).is_some_and(|it| self.refuses(it, last));
        if refused || next_refused || self.decreases(taken) {
            return self.take_one_by_one(items);
        }
        (n, None)
    }

    /// Whether item `it` is refused after an item starting at `last`, but for its relation values' order.
    fn refuses(&self, it: &Event<'_>, last: i64) -> bool {
        (it.start < last)
            | (it.end < it.start)
            | (!self.fields.latest_end & (it.end != it.start))
            | (!self.fields.flags & (it.flags != 0))
            | (it.payload.len() > ITEM_BYTES_MAX)
            | (it.values.len() != self.stored.len())
    }

    /// Whether a monotone relation's values decrease in `items`, from the open chunk's last ones on.
    fn decreases(&self, items: &[Event<'_>]) -> bool {
        self.stored
            .iter()
            .zip(&self.last_values)
            .enumerate()
            .filter(|(_, (s, _))| s.monotone)
            .any(|(j, (_, last))| {
                let mut prev = last.unwrap_or(0);
                let mut down = false;
                for v in items.iter().filter_map(|it| it.values.get(j).copied()) {
                    down |= v < prev;
                    prev = v;
                }
                down
            })
    }

    /// [Self::take], an item at a time: where it stops and why.
    fn take_one_by_one(&self, items: &[Event<'_>]) -> (usize, Option<ItemError>) {
        let mut count = self.items();
        let mut bytes = self.payload.len();
        let mut last_start = self.last_start;
        let mut last_values: Option<&[u64]> = None;
        for (i, it) in items.iter().enumerate() {
            if count >= self.chunk_items {
                return (i, None);
            }
            if let Err(why) = self.check(it, last_start, last_values) {
                return (i, Some(why));
            }
            let len = it.payload.len();
            if count > 0 && bytes.saturating_add(len) > self.chunk_bytes {
                return (i, None);
            }
            count = count.saturating_add(1);
            bytes = bytes.saturating_add(len);
            last_start = Some(it.start);
            last_values = Some(it.values);
        }
        (items.len(), None)
    }

    /// Check item `it` against the track, the start of the item before and its relation values; None for the item
    /// before the open chunk's.
    fn check(
        &self,
        it: &Event<'_>,
        last_start: Option<i64>,
        last_values: Option<&[u64]>,
    ) -> Result<(), ItemError> {
        if last_start.is_some_and(|last| it.start < last) {
            return Err(ItemError::Order);
        }
        if it.end < it.start || (!self.fields.latest_end && it.end != it.start) {
            return Err(ItemError::Span);
        }
        if !self.fields.flags && it.flags != 0 {
            return Err(ItemError::Flags);
        }
        if it.payload.len() > ITEM_BYTES_MAX {
            return Err(ItemError::TooLong {
                len: it.payload.len(),
            });
        }
        if it.values.len() != self.stored.len() {
            return Err(ItemError::Relations {
                values: it.values.len(),
                stored: self.stored.len(),
            });
        }
        for (j, (s, v)) in self.stored.iter().zip(it.values).enumerate() {
            let last = match last_values {
                Some(l) => l.get(j).copied(),
                None => self.last_values.get(j).copied().flatten(),
            };
            if s.monotone && last.is_some_and(|l| *v < l) {
                return Err(ItemError::RelationDecreases {
                    relation: s.relation,
                });
            }
        }
        Ok(())
    }

    /// Add items [Self::take] took, column by column.
    fn add(&mut self, items: &[Event<'_>]) {
        let Some(last) = items.last() else {
            return;
        };
        let from = self.items();
        self.starts.extend(items.iter().map(|it| it.start));
        if self.fields.latest_end {
            self.ends.extend(items.iter().map(|it| it.end));
        }
        if self.fields.flags {
            self.flags.extend(items.iter().map(|it| it.flags));
        }
        if let (Some(runs), [it]) = (&mut self.runs, items) {
            // one item, as [EventsHandle::push] gives it: no pass over columns
            let r = runs.push(it.start, it.end, it.flags);
            debug_assert!(r.is_ok(), "an item checked before goes into its run");
        } else if let Some(runs) = &mut self.runs {
            let starts = self.starts.get(from..).unwrap_or_default();
            let ends = self
                .fields
                .latest_end
                .then(|| self.ends.get(from..).unwrap_or_default());
            let flags = self
                .fields
                .flags
                .then(|| self.flags.get(from..).unwrap_or_default());
            // checked as the runs check: start order, spans, flags
            let r = runs.push_columns(starts, ends, flags);
            debug_assert!(r.is_ok(), "items checked before go into their runs");
        }
        let from = self.payload.len();
        let len = items
            .iter()
            .fold(0usize, |n, it| n.saturating_add(it.payload.len()));
        self.payload.resize(from.saturating_add(len), 0);
        let mut rest = self.payload.get_mut(from..).unwrap_or_default();
        for it in items {
            let Some((now, later)) = rest.split_at_mut_checked(it.payload.len()) else {
                debug_assert!(false, "room for every payload");
                break;
            };
            copy_payload(now, it.payload);
            rest = later;
        }
        self.lens
            .extend(items.iter().map(|it| payload_len(it.payload)));
        for (j, (col, l)) in self
            .values
            .iter_mut()
            .zip(&mut self.last_values)
            .enumerate()
        {
            // each item holds a value per stored relation: checked
            col.extend(items.iter().filter_map(|it| it.values.get(j).copied()));
            *l = last.values.get(j).copied();
        }
        self.last_start = Some(last.start);
    }

    fn encode(&mut self, buffers: &dyn Buffers) -> Result<Option<Completed>, EncodeError> {
        if self.starts.is_empty() {
            return Ok(None);
        }
        let ends = self.fields.latest_end.then_some(self.ends.as_slice());
        let flags = self.fields.flags.then_some(self.flags.as_slice());
        let times = EncodedTimes::encode(&self.starts, ends, flags).map_err(EncodeError::Times)?;
        let payloads = EncodedPayloads::from_parts(
            self.payload_codec,
            &self.lens,
            &self.payload,
            buffers.buffer(),
        )
        .map_err(EncodeError::Payloads)?;
        let relations = self
            .stored
            .iter()
            .zip(&self.values)
            .map(|(s, v)| EncodedRelation::encode(s.relation, v, s.monotone))
            .collect::<Result<Vec<_>, _>>()
            .map_err(EncodeError::Times)?;
        let first_item = ItemNum::new(self.next_item);
        let head = times.head.chunk_head(self.track_id, self.track, first_item);
        let copy = |(spec, bytes): (ColumnSpec, &[u8])| {
            let mut b = buffers.buffer();
            b.extend_from_slice(bytes);
            (spec, b)
        };
        let columns: Vec<_> = times
            .columns()
            .map(copy)
            .chain(payloads.into_columns())
            .chain(relations.iter().map(EncodedRelation::column).map(copy))
            .collect();
        let summary = self.runs.as_mut().map(|runs| {
            runs.complete();
            ChunkSummary::Events {
                fields: self.fields,
                runs: runs.take_runs(),
            }
        });
        self.next_item = self.next_item.saturating_add(u64::from(times.head.count));
        self.starts.clear();
        self.ends.clear();
        self.flags.clear();
        self.payload.clear();
        self.lens.clear();
        self.values.iter_mut().for_each(Vec::clear);
        Ok(Some(Completed {
            head,
            columns,
            summary,
        }))
    }
}

impl Encoder for EventsChunk {
    fn complete(&mut self, buffers: &dyn Buffers) -> Result<Option<Completed>, EncodeError> {
        self.encode(buffers)
    }
}

/// Writes the items of one events track from one producer: instants, spans, flags, relation values and payloads.
///
/// Items are encoded into the open chunk on the producer's thread, its payload hashed there too, and the leaves of
/// the track's item tree computed; the writing thread only places the chunk and builds the levels above. A chunk
/// is completed when it holds [EventsOptions::chunk_items] items or [EventsOptions::chunk_bytes] payload bytes, by
/// [Self::complete], and by the writing thread when its first item waited [EventsOptions::latency]. Dropping the handle
/// completes and sends the open chunk; [Self::finish] does so and returns the error.
pub struct EventsHandle<T: Tapes> {
    s: Sender<T>,
    open: Arc<Open<T, EventsChunk>>,
    backlog: Backlog<T>,
    track: TrackNum,
}

common!(EventsHandle);

impl<T> TapeMux<T>
where
    T: Tapes + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
    T::Error: Send + Sync,
{
    /// A handle that writes the items of events track `track`. Refused for a track the mux doesn't hold, a series
    /// or state track, or options out of range.
    pub fn events(
        &self,
        track: TrackNum,
        options: EventsOptions,
    ) -> Result<EventsHandle<T>, MuxError<T::Error>> {
        crate::wait::block_on(self.events_async(track, options))
    }

    /// [Self::events] as a future.
    pub async fn events_async(
        &self,
        track: TrackNum,
        options: EventsOptions,
    ) -> Result<EventsHandle<T>, MuxError<T::Error>> {
        let bytes = self.track_def(track).await?;
        let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&bytes) else {
            return Err(CallError::Kind { track }.into());
        };
        let chunk = events_chunk(&def, options).ok_or(CallError::Kind { track })?;
        if !options.summaries {
            self.plain(track).await?;
        }
        let s = Sender::new(self.shared.clone(), Some(track));
        let open = Open::register(&self.shared, s.slot.clone(), chunk, options.latency);
        Ok(EventsHandle {
            s,
            open,
            backlog: Backlog::new(),
            track,
        })
    }
}

/// The fields of the item tree of events track `def`; None for another kind of track.
fn event_fields(def: &TrackDef<'_>) -> Option<EventFields> {
    let TrackKind::Events(e) = &def.kind else {
        return None;
    };
    if def.repeat.state().is_some() {
        return None;
    }
    Some(EventFields {
        flags: e.flags,
        latest_end: e.ends == Ends::Spans,
    })
}

/// The summary of events chunk `chunk` of track `def` read back: the runs its handle computed of its items.
/// None for a chunk of another kind of track, or one whose times don't read.
pub(crate) fn read_summary(def: &TrackDef<'_>, chunk: &Chunk<'_>) -> Option<ChunkSummary> {
    let fields = event_fields(def)?;
    let times = EventTimes::read(chunk).ok()?;
    let n = times.len();
    let starts: Vec<i64> = (0..n).map(|i| times.start(i)).collect();
    let ends: Option<Vec<i64>> = fields
        .latest_end
        .then(|| (0..n).map(|i| times.end(i)).collect());
    let flags: Option<Vec<u32>> = fields
        .flags
        .then(|| (0..n).map(|i| times.flag(i)).collect());
    let mut runs = EventRuns::new(fields, chunk.head().first_item);
    runs.push_columns(&starts, ends.as_deref(), flags.as_deref())
        .ok()?;
    runs.complete();
    Some(ChunkSummary::Events {
        fields,
        runs: runs.take_runs(),
    })
}

/// The open chunk of events track `def` cut as `options` say; None for another kind of track or options out of
/// range.
fn events_chunk(def: &TrackDef<'_>, options: EventsOptions) -> Option<EventsChunk> {
    let fields = event_fields(def)?;
    let chunk_items = usize::try_from(options.chunk_items)
        .ok()
        .filter(|n| (1..=ww_tape::times::ITEMS_MAX).contains(n))?;
    let chunk_bytes = usize::try_from(options.chunk_bytes.get()).ok()?;
    if let PayloadCodec::Named(c) = options.payload_codec
        && c != ww_tape::Codec::RAW
        && ww_tape::payloads::coder(c).is_none()
    {
        return None;
    }
    let relations = ww_tape::times::StoredRelations::of(def);
    let stored: Vec<Stored> = (0..u16::try_from(ww_tape::RELATIONS_MAX).ok()?)
        .filter(|i| relations.stored.contains(*i))
        .map(|relation| Stored {
            relation,
            monotone: relations.monotone.contains(relation),
        })
        .collect();
    let n = stored.len();
    Some(EventsChunk {
        track: def.number,
        track_id: def.id(),
        fields,
        stored,
        chunk_items,
        chunk_bytes,
        payload_codec: options.payload_codec,
        next_item: options.first_item.get(),
        starts: Vec::new(),
        ends: Vec::new(),
        flags: Vec::new(),
        payload: Vec::new(),
        lens: Vec::new(),
        values: vec![Vec::new(); n],
        last_start: None,
        last_values: vec![None; n],
        runs: options
            .summaries
            .then(|| EventRuns::new(fields, options.first_item)),
    })
}

impl<T: Tapes> EventsHandle<T>
where
    T: 'static,
    T::Error: Send + Sync,
    T::Key: Send,
    T::Sink: Send,
{
    /// The track this handle writes.
    pub fn track(&self) -> TrackNum {
        self.track
    }

    /// The number the next item gets.
    pub fn next_item(&self) -> ItemNum {
        crate::queue::lock(&self.open.encoder).item_num()
    }

    /// Add an item from `start` to `end` (the start for an instant) with `flags` and `payload`, on a track that keeps
    /// no relation values. A chunk it fills is sent: what became of it.
    pub fn push(
        &mut self,
        start: i64,
        end: i64,
        flags: u32,
        payload: &[u8],
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_related(start, end, flags, payload, &[])
    }

    /// Add an item with its value of each relation the track keeps values of, in the order of the track's
    /// relations; see [Self::push].
    pub fn push_related(
        &mut self,
        start: i64,
        end: i64,
        flags: u32,
        payload: &[u8],
        values: &[u64],
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_all(&[Event {
            start,
            end,
            flags,
            payload,
            values,
        }])
    }

    /// Add `items` in order, in one pass under one hold of the open chunk: the bytes are those of pushing them one by
    /// one. Each chunk they fill is sent: what became of the last. A refused item refuses the rest: the items before
    /// it are added and their chunks sent, and the refusal names its number.
    pub fn push_all(&mut self, items: &[Event<'_>]) -> Result<Option<Sent>, MuxError<T::Error>> {
        let (open, refused) = self.add(items)?;
        let sent = self.backlog.send(&self.s)?;
        self.opened(open);
        refused.map_or(Ok(sent), |r| Err(r.into()))
    }

    /// [Self::push] as a future, waiting for room by waker.
    ///
    /// Dropped while it waits, the item stays added and the chunk it completed stays with the handle: its next push,
    /// complete, finish or drop sends that chunk first.
    pub async fn push_async(
        &mut self,
        start: i64,
        end: i64,
        flags: u32,
        payload: &[u8],
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_related_async(start, end, flags, payload, &[])
            .await
    }

    /// [Self::push_related] as a future; see [Self::push_async].
    pub async fn push_related_async(
        &mut self,
        start: i64,
        end: i64,
        flags: u32,
        payload: &[u8],
        values: &[u64],
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.push_all_async(&[Event {
            start,
            end,
            flags,
            payload,
            values,
        }])
        .await
    }

    /// [Self::push_all] as a future; see [Self::push_async].
    pub async fn push_all_async(
        &mut self,
        items: &[Event<'_>],
    ) -> Result<Option<Sent>, MuxError<T::Error>> {
        let (open, refused) = self.add(items)?;
        let sent = self.backlog.send_async(&self.s).await?;
        self.opened(open);
        refused.map_or(Ok(sent), |r| Err(r.into()))
    }

    /// Check and add items, the chunks they complete into the backlog: whether the open chunk holds items after, and
    /// why an item was refused when one was.
    fn add(
        &mut self,
        items: &[Event<'_>],
    ) -> Result<(bool, Option<CallError>), MuxError<T::Error>> {
        self.s.ready()?;
        let mut e = crate::queue::lock(&self.open.encoder);
        let mut rest = items;
        while !rest.is_empty() {
            let (n, why) = e.take(rest);
            let (now, later) = rest.split_at(n);
            e.add(now);
            rest = later;
            if let Some(why) = why {
                let item = e.item_num();
                return Ok((e.items() > 0, Some(CallError::Item { item, why })));
            }
            // full, or the next item doesn't fit
            if e.items() >= e.chunk_items || !rest.is_empty() {
                match self.open.complete_locked(&mut e) {
                    Ok(Some(unit)) => self.backlog.push(unit),
                    Ok(None) => {}
                    Err(err) => return Ok((e.items() > 0, Some(CallError::Encode(err)))),
                }
            }
        }
        // dropped before sending: the writing thread may need the chunk to complete it
        Ok((e.items() > 0, None))
    }

    /// Start the open chunk's latency once the chunks completed before are queued, so the writing thread completes it
    /// behind them, never ahead.
    fn opened(&self, open: bool) {
        if open {
            self.open.opened();
        }
    }

    /// Complete the open chunk and send it now; None when it holds no item.
    pub fn complete(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.complete_open()?;
        self.backlog.send(&self.s)
    }

    /// [Self::complete] as a future; dropped while waiting, the chunk stays with the handle, as for [Self::push_async].
    pub async fn complete_async(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        self.complete_open()?;
        self.backlog.send_async(&self.s).await
    }

    fn complete_open(&mut self) -> Result<(), MuxError<T::Error>> {
        let mut e = crate::queue::lock(&self.open.encoder);
        if let Some(unit) = self
            .open
            .complete_locked(&mut e)
            .map_err(CallError::Encode)?
        {
            self.backlog.push(unit);
        }
        Ok(())
    }

    /// Complete and send the open chunk, then return once everything this handle sent is placed.
    pub fn finish(mut self) -> Result<(), MuxError<T::Error>> {
        self.complete()?;
        self.s.flush(false)
    }

    /// [Self::finish] as a future. Async code finishes a handle so: its drop sends what it holds blocking.
    pub async fn finish_async(mut self) -> Result<(), MuxError<T::Error>> {
        self.complete_async().await?;
        self.s.flush_async(false).await
    }
}

impl<T: Tapes> Drop for EventsHandle<T> {
    /// Complete and send the open chunk and any chunk a dropped async call left, as far as the mux takes them; waits
    /// for room on this thread.
    fn drop(&mut self) {
        let unit = {
            let mut e = crate::queue::lock(&self.open.encoder);
            self.open.complete_locked(&mut e)
        };
        if let Ok(Some(unit)) = unit {
            self.backlog.push(unit);
        }
        // a closed or failed mux takes nothing: the items are lost as after a close
        drop(self.backlog.send(&self.s));
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::indexing_slicing,
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "tests fail by panicking"
    )]

    use super::*;

    /// Every length, short or long, copies its bytes and nothing past them.
    #[test]
    fn copy_payload_copies_every_length() {
        let src: Vec<u8> = (1..=40).collect();
        for n in 0..=40 {
            let mut dst = [0u8; 42];
            copy_payload(&mut dst[1..=n], &src[..n]);
            assert_eq!(&dst[1..=n], &src[..n], "length {n}");
            assert_eq!(dst[0], 0, "length {n}: before");
            assert!(dst[n + 1..].iter().all(|&b| b == 0), "length {n}: after");
        }
        let len = payload_len(&src);
        assert_eq!(len, src.len() as u32);
    }
}
