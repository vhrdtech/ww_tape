//! The definitions the figures' tapes use: one clock and events tracks on it.

use anyhow::ensure;
use shrink_wrap::prelude::*;
use ww_tape::{ClockNum, TrackNum};
use ww_tape::{
    ClockOrigin, ClockSpec, Definitions, EventsDef, FullVersion, Magnitude, Segment, TrackKind,
    TrackRepeat, TrackSpec, Version, clock_key, track_key,
};

/// The number of the clock of every track, 1 ns ticks since boot: the first clock defined.
pub const CLOCK: u32 = 1;

/// The clock of every track.
fn clock() -> ClockSpec<'static> {
    ClockSpec {
        key: clock_key(0xf1, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    }
}

/// Define the clock of every track in `defs`: its number, [CLOCK].
pub fn define_clock(defs: &mut Definitions) -> anyhow::Result<ClockNum> {
    let c = defs.define_clock(clock())?;
    ensure!(c.get() == CLOCK, "the figures' clock is clock {CLOCK}");
    Ok(c)
}

/// Events track `index` on `clock`, repeated as `repeat`: its key is the index's.
fn track(
    index: u32,
    clock: ClockNum,
    label: &'static str,
    repeat: TrackRepeat,
) -> TrackSpec<'static> {
    TrackSpec {
        key: track_key(0xf1, &[Segment::Index(index)]),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: ww_tape::Items::WireWeaver {
            api: FullVersion::new("figures_api", Version::new(0, 1, 0)),
            bundle: RefVec::Slice { slice: &[] },
            path: RefVec::Slice {
                slice: &[UNib32(1)],
            },
            keys: RefVec::Slice { slice: &[] },
            item: None,
        },
        label,
        repeat,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
        defined_twice: false,
    }
}

/// Define events track `index` on `clock` in `defs` as track number `index`, which the figure's labels name: the
/// lower numbers still free go to tracks defined and closed at once, which leave no definition behind.
pub fn define_track(
    defs: &mut Definitions,
    index: u32,
    clock: ClockNum,
    label: &'static str,
    repeat: TrackRepeat,
) -> anyhow::Result<TrackNum> {
    let spec = track(index, clock, label, repeat);
    let mut closed = Vec::new();
    for k in 0..index {
        let mut probe = defs.clone();
        let n = probe.define_track(spec.clone())?;
        if n.get() == index {
            *defs = probe;
            for c in closed {
                defs.close(c);
            }
            return Ok(n);
        }
        ensure!(n.get() < index, "track {index} is taken");
        let filler = track(0xf111_0000 | k, clock, "filler", TrackRepeat::Stream);
        closed.push(defs.define_track(filler)?);
    }
    anyhow::bail!("track {index} is taken")
}
