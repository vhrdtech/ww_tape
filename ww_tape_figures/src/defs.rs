//! The definitions the figures' tapes use: one clock and events tracks on it.

use shrink_wrap::prelude::*;
use ww_tape::{
    ClockDef, ClockOrigin, Describe, EventsDef, FullVersion, Magnitude, Segment, TrackDef,
    TrackKind, TrackRepeat, Version, clock_key, track_key,
};

/// The clock of every track: 1 ns ticks since boot.
pub const CLOCK: u32 = 1;

pub fn clock() -> Describe<'static> {
    Describe::Clock(ClockDef {
        number: UNib32(CLOCK),
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
        number: UNib32(number),
        key: track_key(0xf1, &[Segment::Index(number)]),
        clock: UNib32(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("figures_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: &[UNib32(1)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label,
        repeat,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        parent: None,
        context: RefVec::Slice { slice: &[] },
        entity_key: None,
        owner: [0; 32],
    })
}
