//! The drive's hour recorded through [ww_tape_mux] handles: one thread per source into one chain, the pattern a
//! live recorder follows.
//!
//! [`Drive::record`] drives every source from one thread into a [`ww_tape::PieceChain`] directly, so that a crash at
//! any exact byte ([`Drive::crash`]) reproduces: the mux's own writing thread and its queue would make that byte no
//! longer synchronous. This module shows the pattern the design favors for a live recorder instead: the mode's puts,
//! the log and temperature records, `can0`'s frames and `phase_a`'s samples each come from a thread of its own, each
//! through its own handle; one mux does every chore (cuts, the close) and places them.
//!
//! The chain this writes is its own, independent of [`Drive::record`]'s: nothing here is compared byte for byte
//! against the story.

use std::thread::{self, ScopedJoinHandle};

use ww_tape::{BlockId, MemTapes, SessionTag, TrackNum};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};

use crate::defs::define_on_mux;
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
    /// Pieces of the chain, and their bytes.
    pub pieces: usize,
    pub bytes: u64,
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
    let mux = TapeMux::create(
        MemTapes::new(),
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

    let (puts, records, frames, samples) = thread::scope(|s| -> Result<_, Error> {
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
        let join = |h: ScopedJoinHandle<'_, Result<u64, Error>>| joined(h);
        Ok((
            join(mode_t)?,
            join(records_t)?,
            join(can0_t)?,
            join(phase_t)?,
        ))
    })?;

    let closed = mux.close()?;
    Ok(ViaHandles {
        pieces: closed.len(),
        bytes: closed.iter().map(|p| p.piece.len).sum(),
        puts,
        records,
        frames,
        samples,
    })
}
