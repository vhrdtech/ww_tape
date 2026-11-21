//! The definitions the figures' tapes use: one clock and events tracks on it.

use shrink_wrap::prelude::*;
use ww_tape::{
    ClockDef, ClockOrigin, Describe, EventsDef, FullVersion, Magnitude, Segment, TrackDef,
    TrackKind, TrackRepeat, Version, clock_key, track_key,
};
use ww_tape::{ClockNum, TrackNum};

/// The clock of every track: 1 ns ticks since boot.
pub const CLOCK: u32 = 1;

pub fn clock() -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: ClockNum::new(CLOCK),
        key: clock_key(0xf1, &[], 1),
        rate: Magnitude::pow10(9).into(),
        resolution: Magnitude::pow10(-9).into(),
        origin: ClockOrigin::Boot,
        label: "host",
    })
}

/// Events track `number` on [CLOCK], repeated as `repeat`.
pub fn track(number: u32, label: &'static str, repeat: TrackRepeat) -> Describe<'static> {
    Describe::Track(TrackDef {
        number: TrackNum::new(number),
        key: track_key(0xf1, &[Segment::Index(number)]),
        clock: ClockNum::new(CLOCK),
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
    })
}
