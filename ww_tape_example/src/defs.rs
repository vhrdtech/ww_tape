//! The drive's definitions: its two clocks, the map between them and its five tracks.

use exact_units::{Bytes, Count};

use crate::Error;
use shrink_wrap::prelude::*;
use ww_tape::summary::{SeriesStats, SeriesSummarySpec};
use ww_tape::{
    ClockDef, ClockMap, ClockOrigin, Definitions, Describe, Element, EventsDef, FullVersion,
    Magnitude, MapSegment, Ratio64, Segment, SeriesDef, StatePolicy, TrackDef, TrackKind,
    TrackRepeat, UNIX_CLOCK_KEY, Version, clock_key, track_key,
};

/// The root of the drive's API: every track and clock key starts from it.
const API_ROOT: u64 = 0xd51e_0001;
/// The board's microsecond timer since boot: the clock of every track.
pub const BOARD_CLOCK: u32 = 1;
/// The host's wall clock in ns since the Unix epoch, which [BOARD_CLOCK] maps to.
pub const HOST_CLOCK: u32 = 2;

/// The firmware's log lines.
pub const LOG: u32 = 1;
/// The controller's mode, a state track.
pub const MODE: u32 = 2;
/// CAN frames on the vehicle bus.
pub const CAN0: u32 = 3;
/// Phase current, 16-bit fixed point in mA.
pub const PHASE_A: u32 = 4;
/// Board temperature.
pub const TEMP: u32 = 5;

/// The host's wall clock at the board's boot: 2026-11-20 09:00:00 UTC, in ns since the Unix epoch.
pub const BOOT_UNIX_NS: i64 = 1_795_683_600_000_000_000;
/// The board's crystal runs 20 ppm fast: one µs of the board lasts 999.98 ns of the host's clock.
const BOARD_NS_PER_US: (u64, u64) = (49_999, 50);
/// The map's error bound, ns: the USB frame time the host stamps within.
const MAP_ERROR_NS: u64 = 125_000;

/// The key of a track at `name` in the drive's API.
pub fn key(name: &str) -> u64 {
    track_key(API_ROOT, &[Segment::Name(name)])
}

fn clocks() -> [Describe<'static>; 2] {
    [
        Describe::Clock(ClockDef {
            number: UNib32(BOARD_CLOCK),
            key: clock_key(API_ROOT, &[Segment::Name("timer")], 1),
            rate: Magnitude::pow10(6).into(),
            resolution: Magnitude::pow10(-6).into(),
            origin: ClockOrigin::Boot,
            label: "board µs since boot",
        }),
        Describe::Clock(ClockDef {
            number: UNib32(HOST_CLOCK),
            key: UNIX_CLOCK_KEY,
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::UnixEpoch,
            label: "host",
        }),
    ]
}

/// A track of the drive on [BOARD_CLOCK].
fn track(
    number: u32,
    name: &'static str,
    repeat: TrackRepeat,
    kind: TrackKind<'static>,
) -> Describe<'static> {
    Describe::Track(TrackDef {
        number: UNib32(number),
        key: key(name),
        clock: UNib32(BOARD_CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        api: FullVersion::new("drive_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: PATHS
                .get(usize::try_from(number).unwrap_or(0))
                .map_or(&[], |p| p.as_slice()),
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
        label: name,
        repeat,
        kind,
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    })
}

/// Each track's resource path in the drive's API, by track number.
static PATHS: [[UNib32; 1]; 6] = [
    [UNib32(0)],
    [UNib32(1)],
    [UNib32(2)],
    [UNib32(3)],
    [UNib32(4)],
    [UNib32(5)],
];

/// The summaries of `phase_a`: minimum, maximum, sum and sum of squares per node of 1024 samples.
pub fn phase_summaries() -> SeriesSummarySpec {
    SeriesSummarySpec {
        base_shift: 10,
        stats: SeriesStats {
            min: true,
            max: true,
            sum: true,
            sum_sq: true,
            clipped: false,
            flags: false,
            power_mean: false,
            power_max: false,
        },
        clip: None,
    }
}

/// The `phase_a` series definition at `hz` samples a second.
pub fn phase_def(hz: u32, summaries: bool) -> Option<SeriesDef<'static>> {
    let mut def = SeriesDef::of::<i16>(None, 1, Ratio64::new(1_000_000, u64::from(hz))?);
    def.scale = Magnitude::pow10(-3);
    def.summaries = summaries.then(phase_summaries);
    debug_assert_eq!(def.shape.element, Element::I16, "i16 samples");
    Some(def)
}

/// The drive's definitions with `phase_a` at `hz`, its summaries when `summaries`, and the mode kept within `near`
/// bytes of its use.
pub fn definitions(hz: u32, summaries: bool, near: u32) -> Result<Definitions, Error> {
    let phase = phase_def(hz, summaries).ok_or(Error::Settings("phase_a's rate is 0"))?;
    let segment = MapSegment::new(
        0,
        None,
        BOOT_UNIX_NS,
        Ratio64::new(BOARD_NS_PER_US.0, BOARD_NS_PER_US.1)
            .ok_or(Error::Settings("the board's rate"))?,
        MAP_ERROR_NS,
    )
    .ok_or(Error::Settings("the clock map's segment"))?;
    let segments = [segment];
    let map = Describe::Map(ClockMap {
        from: UNib32(BOARD_CLOCK),
        to: UNib32(HOST_CLOCK),
        segments: RefVec::Slice { slice: &segments },
    });
    let mode = TrackRepeat::State(StatePolicy {
        near: Bytes::new(near),
        copies: Count::new(1),
    });
    let mut defs = Definitions::new();
    let [board, host] = clocks();
    defs.define_all(&[
        board,
        host,
        map,
        track(
            LOG,
            "log",
            TrackRepeat::Stream,
            TrackKind::Events(EventsDef::INSTANTS),
        ),
        track(MODE, "mode", mode, TrackKind::Events(EventsDef::INSTANTS)),
        track(
            CAN0,
            "can0",
            TrackRepeat::Stream,
            TrackKind::Events(EventsDef::INSTANTS),
        ),
        track(
            PHASE_A,
            "phase_a",
            TrackRepeat::Stream,
            TrackKind::Series(phase),
        ),
        track(
            TEMP,
            "temp",
            TrackRepeat::Stream,
            TrackKind::Events(EventsDef::INSTANTS),
        ),
    ])?;
    Ok(defs)
}
