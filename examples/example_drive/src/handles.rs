//! The drive's hour recorded through [ww_tape_mux] handles: one thread per source into one chain, the pattern a
//! live recorder follows.
//!
//! [`Drive::record`] drives the same handles from one thread in the story's fixed order, so that the bytes are the
//! same every run. Here the mode's puts, the log and temperature records, `can0`'s frames and `phase_a`'s samples
//! each come from a thread of its own, each through its own handle; one mux does every chore (cuts, the close) and
//! places them. Which unit lands first is up to the threads, so the bytes differ from the story's. What each track
//! holds is the same: [decode] reads it back, and a test compares it with [`Drive::record`]'s.

use std::thread::{self, ScopedJoinHandle};

use ww_tape::chunk::ColumnKind;
use ww_tape::payloads::Payloads;
use ww_tape::samples::{SampleShape, Samples};
use ww_tape::times::EventTimes;
use ww_tape::{
    BlockId, BlockKind, Element, ElementDesc, FileBlocks, MemPieces, RecordTrack, Recovery,
    SessionTag, TrackNum,
};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};

use crate::defs::{DriveNumbers, define_on_mux};
use crate::record::{PHASE_CODEC, first_at, item_us, options, records_block};
use crate::signals::{Mode, US_PER_MS, frame, log_line, temp_centi};
use crate::writing::{joined, phase_values};
use crate::{Drive, Error, OWNER};

/// Records of one records block at most: a block every so many log lines and temperatures, time ordered.
const RECORDS_BATCH: usize = 256;
/// Frames of one `push_all` at most.
const CAN0_BATCH: u64 = 4096;

/// What [record] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViaHandles {
    /// The bytes of each piece of the chain.
    pub pieces: Vec<Vec<u8>>,
    /// Puts of the mode, sent by its own thread.
    pub puts: u64,
    /// Records (log lines and temperatures), sent by their own thread.
    pub records: u64,
    /// Frames of `can0`, sent by their own thread.
    pub frames: u64,
    /// Samples of `phase_a`, sent by their own thread.
    pub samples: u64,
}

/// Record `drive`'s hour into one chain in memory, through a [TapeMux] and its handles: one thread per source.
///
/// # Errors
///
/// A setting of `drive` the recording can't be made with, or a mux or handle call that failed.
pub fn record(drive: &Drive) -> Result<ViaHandles, Error> {
    // --8<-- [start:create]
    let mux_tapes = MemPieces::new();
    let mux = TapeMux::create(
        mux_tapes.clone(),
        OWNER,
        drive.chain,
        &[],
        options(drive),
        MuxOptions::default(),
    )?;
    let numbers = define_on_mux(
        &mux,
        drive.phase_hz,
        drive.summaries,
        drive.near_bytes.get(),
    )?;
    let end_us = i64::from(drive.minutes) * 60_000 * US_PER_MS;
    // --8<-- [end:create]

    let (puts, records, frames, samples) = thread::scope(|s| -> Result<_, Error> {
        // --8<-- [start:mode]
        let mode_t = s.spawn(|| -> Result<u64, Error> {
            let state = mux.state(numbers.mode)?;
            let mut mode = Mode::at(0);
            state.put(0, mode.item())?;
            let mut n = 1u64;
            while let Some(next) = mode.next_change_us().filter(|&c| c < end_us) {
                mode = Mode::at(next);
                state.put(next, mode.item())?;
                n += 1;
            }
            Ok(n)
        });
        // --8<-- [end:mode]
        let records_t = s.spawn(|| -> Result<u64, Error> {
            let log = mux.records();
            let mut items: Vec<(TrackNum, i64, Vec<u8>)> = Vec::new();
            let log_total = first_at(end_us, u64::from(drive.log_per_hour));
            for n in 0..log_total {
                let us = item_us(n, u64::from(drive.log_per_hour));
                items.push((numbers.log, us, log_line(n, us / US_PER_MS).into_bytes()));
            }
            let temp_total = first_at(end_us, u64::from(drive.temp_per_hour));
            for n in 0..temp_total {
                let us = item_us(n, u64::from(drive.temp_per_hour));
                items.push((
                    numbers.temp,
                    us,
                    temp_centi(us / US_PER_MS).to_le_bytes().to_vec(),
                ));
            }
            items.sort_by_key(|(track, us, _)| (*us, *track));
            let mut sent = 0u64;
            for batch in items.chunks(RECORDS_BATCH) {
                let block = records_block(
                    BlockId::new(SessionTag::NONE, 0),
                    numbers.board_clock,
                    batch,
                )?;
                log.send(block)?;
                sent += batch.len() as u64;
            }
            Ok(sent)
        });
        // --8<-- [start:can0]
        let can0_t = s.spawn(|| -> Result<u64, Error> {
            let mut can0 = mux.events(numbers.can0, EventsOptions::default())?;
            let period_us = 1_000_000 / u64::from(drive.can_hz);
            let total = first_at(end_us, u64::from(drive.can_hz) * 3_600);
            let mut k = 0u64;
            while k < total {
                let take = CAN0_BATCH.min(total - k);
                let payloads: Vec<(i64, Vec<u8>)> =
                    (k..k + take).map(|i| frame(i, period_us)).collect();
                let events: Vec<Event<'_>> = payloads
                    .iter()
                    .map(|(start, payload)| Event {
                        start: *start,
                        end: *start,
                        payload,
                        ..Event::default()
                    })
                    .collect();
                can0.push_all(&events)?;
                k += take;
            }
            can0.finish()?;
            Ok(total)
        });
        // --8<-- [end:can0]
        // --8<-- [start:phase_a]
        let phase_t = s.spawn(|| -> Result<u64, Error> {
            let mut phase_a = mux.series::<i16>(
                numbers.phase_a,
                SeriesOptions {
                    codec: PHASE_CODEC,
                    summaries: drive.summaries,
                    ..SeriesOptions::default()
                },
            )?;
            let total = first_at(end_us, u64::from(drive.phase_hz) * 3_600);
            let step = u64::from(drive.phase_hz).max(1);
            let mut n = 0u64;
            while n < total {
                let take = step.min(total - n);
                phase_a.push(&phase_values(drive, n, take))?;
                n += take;
            }
            phase_a.finish()?;
            Ok(total)
        });
        // --8<-- [end:phase_a]
        let join = |h: ScopedJoinHandle<'_, Result<u64, Error>>| joined(h);
        Ok((
            join(mode_t)?,
            join(records_t)?,
            join(can0_t)?,
            join(phase_t)?,
        ))
    })?;

    let tapes = mux_tapes;
    // --8<-- [start:close]
    let closed = mux.close()?;
    // --8<-- [end:close]
    let pieces = closed
        .iter()
        .map(|p| {
            tapes
                .get(&p.tape)?
                .ok_or(Error::Settings("a closed piece"))?
                .to_vec()
                .map_err(Error::from)
        })
        .collect::<Result<_, _>>()?;
    Ok(ViaHandles {
        pieces,
        puts,
        records,
        frames,
        samples,
    })
}

/// What a chain of the drive's holds, track by track and in time order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Decoded {
    /// The log lines: ticks and text.
    pub log: Vec<(i64, Vec<u8>)>,
    /// The temperatures: ticks and item.
    pub temp: Vec<(i64, Vec<u8>)>,
    /// The mode's changes: ticks and item, the first record of each; copies are no change.
    pub mode: Vec<(i64, Vec<u8>)>,
    /// The frames of `can0`: start and payload.
    pub frames: Vec<(i64, Vec<u8>)>,
    /// The samples of `phase_a`.
    pub samples: Vec<i16>,
}

/// Read the items of every track of `pieces` back, the drive's tracks numbered as `numbers`.
///
/// # Errors
///
/// A piece that doesn't read, or a chunk that doesn't decode.
pub fn decode(pieces: &[Vec<u8>], numbers: &DriveNumbers) -> Result<Decoded, Error> {
    let mut out = Decoded::default();
    for piece in pieces {
        let mut walk = FileBlocks::with_recovery(&piece[..], Recovery::Prefix);
        while let Some(block) = walk.next_block() {
            let block = block?;
            match block.kind {
                BlockKind::Records => {
                    for r in block.records() {
                        let r = r?;
                        let (RecordTrack::Data(t), Some(at)) = (r.track, r.at) else {
                            continue;
                        };
                        let item = (at.ticks, r.item.to_vec());
                        if t == numbers.log {
                            out.log.push(item);
                        } else if t == numbers.temp {
                            out.temp.push(item);
                        } else if t == numbers.mode && out.mode.last().is_none_or(|m| m.1 != item.1)
                        {
                            out.mode.push(item);
                        }
                    }
                }
                BlockKind::Events => {
                    let chunk = block.chunk().ok_or(Error::Settings("a chunk of frames"))?;
                    if chunk.head().track_number != numbers.can0 {
                        continue;
                    }
                    let times = EventTimes::read(&chunk)?;
                    let payloads = Payloads::read(&chunk)?
                        .ok_or(Error::Settings("the payloads of a chunk of frames"))?;
                    for j in 0..times.len() {
                        let payload = payloads
                            .get(j)
                            .ok_or(Error::Settings("a payload of a chunk of frames"))?;
                        out.frames.push((times.start(j), payload.to_vec()));
                    }
                }
                BlockKind::Series => {
                    let chunk = block.chunk().ok_or(Error::Settings("a chunk of samples"))?;
                    let head = chunk.head();
                    let column = chunk
                        .find(ColumnKind::Samples, 0)
                        .ok_or(Error::Settings("the samples of a chunk"))?;
                    let count = usize::try_from(head.count)
                        .map_err(|_e| Error::Settings("a chunk's sample count"))?;
                    let samples = Samples::of(
                        &column,
                        SampleShape::scalar(ElementDesc::of(Element::I16)),
                        count,
                    )?;
                    let mut values = vec![0i16; count];
                    samples.decode(0, &mut values)?;
                    out.samples.extend(values);
                }
                _ => {}
            }
        }
    }
    Ok(out)
}
