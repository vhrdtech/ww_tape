//! A mux small enough for one page: three producer threads, each with its own handle, into one chain in memory.
//!
//! [threads] runs them on [std::thread]; `example_drive`'s tests run the same calls as `_async` on tokio tasks.

use std::thread;

use shrink_wrap::prelude::*;
use ww_tape::{
    BlockKind, ChainOptions, ChainPiece, ClockNum, ClockOrigin, ClockSpec, EventsDef, FileBlocks,
    Items, Magnitude, MemPieces, OwnerId, Ratio64, Recovery, Segment, SeriesDef, TrackKind,
    TrackNum, TrackRepeat, TrackSpec, clock_key, track_key,
};
use ww_tape_mux::{Event, EventsOptions, MuxOptions, SeriesOptions, TapeMux};

use crate::Error;
use crate::writing::joined;

/// The owner of this example's chain: not the drive's ([crate::OWNER]), so a test can run both in one process.
pub const OWNER: OwnerId = OwnerId([0x51; 32]);
/// The root of this example's track and clock keys.
const API_ROOT: u64 = 0xe5a3_0001;
/// Pings each events thread sends in one [`EventsHandle::push_all`](ww_tape_mux::EventsHandle::push_all) call.
pub const PINGS: u32 = 1_000;
/// Levels the series thread sends in one [`SeriesHandle::push`](ww_tape_mux::SeriesHandle::push) call.
pub const LEVELS: usize = 3_000;

/// The clock every track of this example runs on: nanoseconds since some boot, not tied to a wall clock.
pub fn clock() -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(API_ROOT, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "example",
    }
}

/// An events track named `name`: one sensor's pings, as instants with no payload.
pub fn pings_track(clock: ClockNum, name: &'static str) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(API_ROOT, &[Segment::Name(name)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::RawBytes,
        label: name,
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// The series track `levels`: one channel of `i16`, a sample a tick.
#[expect(
    clippy::expect_used,
    reason = "1 is not 0: Ratio64::int(1) is always Some"
)]
pub fn levels_track(clock: ClockNum) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(API_ROOT, &[Segment::Name("levels")]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: Items::numbers::<i16>(None),
        label: "levels",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Series(SeriesDef::of::<i16>(
            1,
            Ratio64::int(1).expect("1 is not 0"),
        )),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// What [threads] (or its async twin) read back after the close: each sensor's pings, and the levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Counts {
    /// Pings of the track `pings_track` named `pings_a`.
    pub pings_a: u64,
    /// Pings of the track `pings_track` named `pings_b`.
    pub pings_b: u64,
    /// Samples of `levels_track`.
    pub levels: u64,
}

/// The bytes of every piece `closed` names, read out of `tapes`.
///
/// # Errors
///
/// A piece `tapes` doesn't hold, or whose bytes don't read.
pub fn pieces_of(tapes: &MemPieces, closed: &[ChainPiece<u64>]) -> Result<Vec<Vec<u8>>, Error> {
    closed
        .iter()
        .map(|p| {
            tapes
                .get(&p.tape)?
                .ok_or(Error::Settings("a closed piece"))?
                .to_vec()
                .map_err(Error::from)
        })
        .collect()
}

/// Every chunk of `pings_a`, `pings_b` and `levels` in `pieces`, summed by track: what each thread's items came to.
///
/// # Errors
///
/// A piece that doesn't read.
pub fn read_back(
    pieces: &[Vec<u8>],
    pings_a: TrackNum,
    pings_b: TrackNum,
    levels: TrackNum,
) -> Result<Counts, Error> {
    let mut out = Counts::default();
    for piece in pieces {
        let mut walk = FileBlocks::with_recovery(&piece[..], Recovery::Prefix);
        while let Some(block) = walk.next_block() {
            let block = block?;
            let Some(chunk) = block.chunk() else { continue };
            let head = chunk.head();
            match (block.kind, head.track_number) {
                (BlockKind::Events, t) if t == pings_a => out.pings_a += u64::from(head.count),
                (BlockKind::Events, t) if t == pings_b => out.pings_b += u64::from(head.count),
                (BlockKind::Series, t) if t == levels => out.levels += u64::from(head.count),
                _ => {}
            }
        }
    }
    Ok(out)
}

// --8<-- [start:threads]
/// Two sensors push pings on their own events track, and a third pushes levels on a series track.
///
/// Each thread holds its own handle into one chain in memory; [read_back] after the close shows every item arrived.
///
/// # Errors
///
/// A mux or handle call that failed.
pub fn threads() -> Result<Counts, Error> {
    let tapes = MemPieces::new();
    let mux = TapeMux::create(
        tapes.clone(),
        OWNER,
        0,
        &[],
        ChainOptions::default(),
        MuxOptions::default(),
    )?;
    let clock = mux.define_clock(clock())?;
    let pings_a = mux.define_track(pings_track(clock, "pings_a"))?;
    let pings_b = mux.define_track(pings_track(clock, "pings_b"))?;
    let levels = mux.define_track(levels_track(clock))?;

    thread::scope(|s| -> Result<(), Error> {
        let a = s.spawn(|| -> Result<(), Error> {
            let mut h = mux.events(pings_a, EventsOptions::default())?;
            let events: Vec<Event<'_>> = (0..PINGS)
                .map(|i| Event {
                    start: i64::from(i),
                    end: i64::from(i),
                    ..Event::default()
                })
                .collect();
            h.push_all(&events)?;
            h.finish()?;
            Ok(())
        });
        let b = s.spawn(|| -> Result<(), Error> {
            let mut h = mux.events(pings_b, EventsOptions::default())?;
            let events: Vec<Event<'_>> = (0..PINGS)
                .map(|i| Event {
                    start: i64::from(i),
                    end: i64::from(i),
                    ..Event::default()
                })
                .collect();
            h.push_all(&events)?;
            h.finish()?;
            Ok(())
        });
        let c = s.spawn(|| -> Result<(), Error> {
            let mut h = mux.series::<i16>(levels, SeriesOptions::default())?;
            h.push(&vec![0i16; LEVELS])?;
            h.finish()?;
            Ok(())
        });
        joined(a)?;
        joined(b)?;
        joined(c)
    })?;

    let pieces = pieces_of(&tapes, &mux.close()?)?;
    read_back(&pieces, pings_a, pings_b, levels)
}
// --8<-- [end:threads]
