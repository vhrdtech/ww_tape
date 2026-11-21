//! The events handle: items of one events track encoded into chunks on the producer's thread.

use std::sync::Arc;

use shrink_wrap::DeserializeShrinkWrap;
use std::time::Duration;

use exact_units::Bytes;
use ww_tape::payloads::EncodedPayloads;
use ww_tape::times::{EncodedRelation, EncodedTimes};
use ww_tape::{Describe, Ends, ItemNum, Tapes, TrackDef, TrackId, TrackKind, TrackNum};
use ww_tape_summary::{EventFields, EventRuns};

use crate::TapeMux;
use crate::error::{CallError, EncodeError, ItemError, MuxError};
use crate::handle::{Pressure, Sender, Sent, common};
use crate::open::{Buffers, Encoder, Open, Sealed};
use crate::reply::reply;
use crate::shared::Work;
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
    /// Payload bytes of a chunk at most; an item that would pass it seals the chunk before it.
    pub chunk_bytes: Bytes<u32>,
    /// How long the first item of an open chunk waits at most before the writing thread seals the chunk; None:
    /// until the chunk is full or sealed by the producer.
    pub latency: Option<Duration>,
    /// Whether the mux builds the track's item tree, each piece's in that piece.
    pub summaries: bool,
    /// The track's first item: past 0 when a resumed chain goes on with the track.
    pub first_item: ItemNum,
}

impl Default for EventsOptions {
    fn default() -> Self {
        Self {
            chunk_items: CHUNK_ITEMS_DEFAULT,
            chunk_bytes: CHUNK_BYTES_DEFAULT,
            latency: Some(LATENCY_DEFAULT),
            summaries: true,
            first_item: ItemNum::new(0),
        }
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
    /// The first item of the open chunk
    next_item: u64,
    starts: Vec<i64>,
    ends: Vec<i64>,
    flags: Vec<u32>,
    payload: Vec<u8>,
    /// Where each payload ends in [Self::payload]
    ends_at: Vec<usize>,
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

    /// Whether `len` more payload bytes pass the chunk's bytes.
    fn full_for(&self, len: usize) -> bool {
        self.items() >= self.chunk_items
            || (self.items() > 0 && self.payload.len().saturating_add(len) > self.chunk_bytes)
    }

    /// Check item `start..end` with `flags`, `payload` and `values` against the track and the items before.
    fn check(
        &self,
        start: i64,
        end: i64,
        flags: u32,
        payload: &[u8],
        values: &[u64],
    ) -> Result<(), CallError> {
        let refuse = |why| CallError::Item {
            item: self.item_num(),
            why,
        };
        if self.last_start.is_some_and(|last| start < last) {
            return Err(refuse(ItemError::Order));
        }
        if end < start || (!self.fields.latest_end && end != start) {
            return Err(refuse(ItemError::Span));
        }
        if !self.fields.flags && flags != 0 {
            return Err(refuse(ItemError::Flags));
        }
        if payload.len() > ITEM_BYTES_MAX {
            return Err(refuse(ItemError::TooLong { len: payload.len() }));
        }
        if values.len() != self.stored.len() {
            return Err(refuse(ItemError::Relations {
                values: values.len(),
                stored: self.stored.len(),
            }));
        }
        for ((s, v), last) in self.stored.iter().zip(values).zip(&self.last_values) {
            if s.monotone && last.is_some_and(|l| *v < l) {
                return Err(refuse(ItemError::RelationDecreases {
                    relation: s.relation,
                }));
            }
        }
        Ok(())
    }

    /// Add an item checked before.
    fn add(&mut self, start: i64, end: i64, flags: u32, payload: &[u8], values: &[u64]) {
        if let Some(runs) = &mut self.runs {
            // checked as the runs check: start order, spans, flags
            let r = runs.push(start, end, flags);
            debug_assert!(r.is_ok(), "an item checked before goes into its run");
        }
        self.starts.push(start);
        self.ends.push(end);
        self.flags.push(flags);
        self.payload.extend_from_slice(payload);
        self.ends_at.push(self.payload.len());
        for ((col, last), v) in self
            .values
            .iter_mut()
            .zip(&mut self.last_values)
            .zip(values)
        {
            col.push(*v);
            *last = Some(*v);
        }
        self.last_start = Some(start);
    }

    fn encode(&mut self, buffers: &dyn Buffers) -> Result<Option<Sealed>, EncodeError> {
        if self.starts.is_empty() {
            return Ok(None);
        }
        let ends = self.fields.latest_end.then_some(self.ends.as_slice());
        let flags = self.fields.flags.then_some(self.flags.as_slice());
        let times = EncodedTimes::encode(&self.starts, ends, flags).map_err(EncodeError::Times)?;
        let mut from = 0;
        let payloads = EncodedPayloads::encode(self.ends_at.iter().map(|&to| {
            let p = self.payload.get(from..to).unwrap_or_default();
            from = to;
            p
        }))
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
        let columns: Vec<_> = times
            .columns()
            .chain(payloads.columns())
            .chain(relations.iter().map(EncodedRelation::column))
            .map(|(spec, bytes)| {
                let mut b = buffers.buffer();
                b.extend_from_slice(bytes);
                (spec, b)
            })
            .collect();
        let hash = ww_tape::piece::columns_payload_hash(columns.iter().map(|(_, b)| b.as_slice()));
        let summary = self.runs.as_mut().map(|runs| {
            runs.seal();
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
        self.ends_at.clear();
        self.values.iter_mut().for_each(Vec::clear);
        Ok(Some(Sealed {
            head,
            columns,
            hash,
            summary,
        }))
    }
}

impl Encoder for EventsChunk {
    fn seal(&mut self, buffers: &dyn Buffers) -> Result<Option<Sealed>, EncodeError> {
        self.encode(buffers)
    }
}

/// Writes the items of one events track from one producer: instants, spans, flags, relation values and payloads.
///
/// Items are encoded into the open chunk on the producer's thread, its payload hashed there too, and the leaves of
/// the track's item tree computed; the writing thread only places the chunk and builds the levels above. A chunk
/// is sealed when it holds [EventsOptions::chunk_items] items or [EventsOptions::chunk_bytes] payload bytes, by
/// [Self::seal], and by the writing thread when its first item waited [EventsOptions::latency]. Dropping the handle
/// seals and sends the open chunk; [Self::finish] does so and returns the error.
pub struct EventsHandle<T: Tapes> {
    s: Sender<T>,
    open: Arc<Open<T, EventsChunk>>,
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
        let (tx, rx) = reply();
        let bytes = self
            .shared
            .control(Work::TrackDef { track, reply: tx }, rx)?
            .ok_or(CallError::Kind { track })?;
        let Ok(Describe::Track(def)) = Describe::from_ww_bytes(&bytes) else {
            return Err(CallError::Kind { track }.into());
        };
        let chunk = events_chunk(&def, options).ok_or(CallError::Kind { track })?;
        let s = Sender::new(self.shared.clone(), Some(track));
        let open = Open::register(&self.shared, s.slot.clone(), chunk, options.latency);
        Ok(EventsHandle { s, open, track })
    }
}

/// The open chunk of events track `def` cut as `options` say; None for another kind of track or options out of
/// range.
fn events_chunk(def: &TrackDef<'_>, options: EventsOptions) -> Option<EventsChunk> {
    let TrackKind::Events(e) = &def.kind else {
        return None;
    };
    if def.repeat.state().is_some() {
        return None;
    }
    let chunk_items = usize::try_from(options.chunk_items)
        .ok()
        .filter(|n| (1..=ww_tape::times::ITEMS_MAX).contains(n))?;
    let chunk_bytes = usize::try_from(options.chunk_bytes.get()).ok()?;
    let fields = EventFields {
        flags: e.flags,
        latest_end: e.ends == Ends::Spans,
    };
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
        next_item: options.first_item.get(),
        starts: Vec::new(),
        ends: Vec::new(),
        flags: Vec::new(),
        payload: Vec::new(),
        ends_at: Vec::new(),
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
        self.s.ready()?;
        let mut e = crate::queue::lock(&self.open.encoder);
        e.check(start, end, flags, payload, values)?;
        // the chunk this item doesn't fit goes first
        let before = if e.full_for(payload.len()) {
            self.open.seal_locked(&mut e).map_err(CallError::Encode)?
        } else {
            None
        };
        e.add(start, end, flags, payload, values);
        let full = e.items() >= e.chunk_items;
        let after = if full {
            self.open.seal_locked(&mut e).map_err(CallError::Encode)?
        } else {
            None
        };
        // never held while sending: the writing thread may need the chunk to seal it
        drop(e);
        let mut sent = None;
        for (work, bytes) in before.into_iter().chain(after) {
            sent = Some(self.s.send(work, bytes)?);
        }
        // the latency runs only once the chunks sealed before are queued: the writing thread seals the open chunk
        // behind them, never ahead
        if !full {
            self.open.opened();
        }
        Ok(sent)
    }

    /// Seal the open chunk and send it now; None when it holds no item.
    pub fn seal(&mut self) -> Result<Option<Sent>, MuxError<T::Error>> {
        let unit = {
            let mut e = crate::queue::lock(&self.open.encoder);
            self.open.seal_locked(&mut e).map_err(CallError::Encode)?
        };
        match unit {
            Some((work, bytes)) => Ok(Some(self.s.send(work, bytes)?)),
            None => Ok(None),
        }
    }

    /// Seal and send the open chunk, then return once everything this handle sent is placed.
    pub fn finish(mut self) -> Result<(), MuxError<T::Error>> {
        self.seal()?;
        self.s.flush(false)
    }
}

impl<T: Tapes> Drop for EventsHandle<T> {
    /// Seal and send the open chunk, as far as the mux takes it.
    fn drop(&mut self) {
        let unit = {
            let mut e = crate::queue::lock(&self.open.encoder);
            self.open.seal_locked(&mut e)
        };
        if let Ok(Some((work, bytes))) = unit {
            // a closed or failed mux takes nothing: the items are lost as after a close
            drop(self.s.send(work, bytes));
        }
    }
}
