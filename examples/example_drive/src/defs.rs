//! The drive's definitions: its two clocks, the map between them and its five tracks, and the numbers they get.

use exact_units::{Bytes, Count};
use ww_tape::{ClockNum, TrackNum};

use crate::Error;
use shrink_wrap::prelude::*;
use ww_tape::summary::{SeriesStats, SeriesSummarySpec};
use ww_tape::{
    ClockMap, ClockOrigin, ClockSpec, Definitions, Describe, EventsDef, FullVersion, GridDef,
    Items, Magnitude, MapSegment, MemError, PieceStore, Ratio64, SampleShape, Segment, SeriesValue,
    StatePolicy, TrackKey, TrackKind, TrackRepeat, TrackSpec, UNIX_CLOCK_KEY, Version, clock_key,
    track_key,
};
use ww_tape_mux::TapeMux;

/// The root of the drive's API: every track and clock key starts from it.
const API_ROOT: u64 = 0xd51e_0001;
// --8<-- [start:ids_item]
/// The numbers the drive's definitions were given ([Definitions::define_clock], [Definitions::define_track]): what its
/// records, chunks and maps name each clock and track by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriveNumbers {
    /// The board's microsecond timer since boot: the clock of every track.
    pub board_clock: ClockNum,
    /// The host's wall clock in ns since the Unix epoch, which the board's clock maps to.
    pub host_clock: ClockNum,
    /// The firmware's log lines.
    pub log: TrackNum,
    /// The controller's mode, a state track.
    pub mode: TrackNum,
    /// CAN frames on the vehicle bus.
    pub can0: TrackNum,
    /// Phase current, 16-bit fixed point in mA.
    pub phase_a: TrackNum,
    /// Board temperature.
    pub temp: TrackNum,
}
// --8<-- [end:ids_item]

/// The host's wall clock at the board's boot: 2026-11-20 09:00:00 UTC, in ns since the Unix epoch.
pub const BOOT_UNIX_NS: i64 = 1_795_683_600_000_000_000;
/// The board's crystal runs 20 ppm fast: one µs of the board lasts 999.98 ns of the host's clock.
const BOARD_NS_PER_US: (u64, u64) = (49_999, 50);
/// The map's error bound, ns: a USB high-speed microframe, within which the host stamps what it receives.
const MAP_ERROR_NS: u64 = 125_000;

// --8<-- [start:key_item]
/// The key of a track at `name` in the drive's API.
pub fn key(name: &str) -> TrackKey {
    track_key(API_ROOT, &[Segment::Name(name)])
}
// --8<-- [end:key_item]

fn clocks() -> [ClockSpec<'static>; 2] {
    [
        ClockSpec {
            key: clock_key(API_ROOT, &[Segment::Name("timer")], 1),
            rate: Magnitude::pow10(6).into(),
            resolution: Magnitude::pow10(-6).into(),
            origin: ClockOrigin::Boot,
            label: "board µs since boot",
        },
        ClockSpec {
            key: UNIX_CLOCK_KEY,
            rate: Magnitude::pow10(9).into(),
            resolution: Magnitude::pow10(-9).into(),
            origin: ClockOrigin::UnixEpoch,
            label: "host",
        },
    ]
}

/// The drive's track `name` on clock `clock`, at resource path `path` of its API.
fn track(
    clock: ClockNum,
    path: &'static [UNib32; 1],
    name: &'static str,
    repeat: TrackRepeat,
    kind: TrackKind<'static>,
) -> TrackSpec<'static> {
    TrackSpec {
        key: key(name),
        clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: match kind {
            TrackKind::Series(_) => Items::numbers::<i16>(None),
            _ => Items::WireWeaver {
                api: FullVersion::new("drive_api", Version::new(0, 1, 0)),
                bundle: RefVec::Slice { slice: &[] },
                path: RefVec::Slice {
                    slice: path.as_slice(),
                },
                keys: RefVec::Slice { slice: &[] },
                item: None,
            },
        },
        label: name,
        repeat,
        kind,
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
        defined_twice: false,
    }
}

/// The tracks' resource paths in the drive's API: `log`, `mode`, `can0`, `phase_a`, `temp`.
static PATHS: [[UNib32; 1]; 5] = [
    [UNib32(1)],
    [UNib32(2)],
    [UNib32(3)],
    [UNib32(4)],
    [UNib32(5)],
];

/// The summaries of `phase_a`: minimum, maximum, sum and sum of squares per node of 1024 samples.
pub fn phase_summaries() -> SeriesSummarySpec {
    // --8<-- [start:spec]
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
    // --8<-- [end:spec]
}

// --8<-- [start:phase_def_item]
/// The `phase_a` series definition at `hz` samples a second.
pub fn phase_def(hz: u32, summaries: bool) -> Option<GridDef<'static>> {
    let mut def = GridDef::of::<i16>(1, Ratio64::new(1_000_000, u64::from(hz))?);
    def.scale = Magnitude::pow10(-3);
    def.summaries = summaries.then(phase_summaries);
    Some(def)
}

/// The shape of `phase_a`'s samples: its track's items are i16 numbers, one channel.
pub fn phase_shape(def: &GridDef<'_>) -> SampleShape {
    def.shape(<i16 as SeriesValue>::ELEMENT)
}
// --8<-- [end:phase_def_item]

/// The drive's definitions with `phase_a` at `hz`, its summaries when `summaries`, and the mode kept within `near`
/// bytes of its use; with the numbers they were given.
pub fn definitions(
    hz: u32,
    summaries: bool,
    near: u32,
) -> Result<(Definitions, DriveNumbers), Error> {
    define(Definitions::new(), hz, summaries, near)
}

/// [definitions] defined into `defs`: those of a resumed recording ([Definitions::with_numbers]) keep the numbers
/// the recording gave them.
pub fn define(
    mut defs: Definitions,
    hz: u32,
    summaries: bool,
    near: u32,
) -> Result<(Definitions, DriveNumbers), Error> {
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
    let mode = TrackRepeat::State(StatePolicy {
        near: Bytes::new(near),
        copies: Count::new(2),
    });
    let events = TrackKind::Events(EventsDef::INSTANTS);
    let [board, host] = clocks();
    let [log, mode_path, can0, phase_a, temp] = &PATHS;
    // --8<-- [start:define]
    let board_clock = defs.define_clock(board)?;
    let host_clock = defs.define_clock(host)?;
    defs.define(&Describe::Map(ClockMap {
        from: board_clock,
        to: host_clock,
        segments: RefVec::Slice { slice: &segments },
    }))?;
    let on_board = |path, name, repeat, kind| track(board_clock, path, name, repeat, kind);
    let numbers = DriveNumbers {
        board_clock,
        host_clock,
        log: defs.define_track(on_board(log, "log", TrackRepeat::Stream, events.clone()))?,
        mode: defs.define_track(on_board(mode_path, "mode", mode, events.clone()))?,
        can0: defs.define_track(on_board(can0, "can0", TrackRepeat::Stream, events.clone()))?,
        phase_a: defs.define_track(on_board(
            phase_a,
            "phase_a",
            TrackRepeat::Stream,
            TrackKind::Series(phase),
        ))?,
        temp: defs.define_track(on_board(temp, "temp", TrackRepeat::Stream, events))?,
    };
    // --8<-- [end:define]
    Ok((defs, numbers))
}

/// [define] on `mux` instead of into a [Definitions] of its own: the numbers its clocks and tracks get.
pub(crate) fn define_on_mux<T>(
    mux: &TapeMux<T>,
    hz: u32,
    summaries: bool,
    near: u32,
) -> Result<DriveNumbers, Error>
where
    T: PieceStore<Error = MemError> + Send + 'static,
    T::Sink: Send,
    T::Key: Send,
{
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
    let mode = TrackRepeat::State(StatePolicy {
        near: Bytes::new(near),
        copies: Count::new(2),
    });
    let events = TrackKind::Events(EventsDef::INSTANTS);
    let [board, host] = clocks();
    let [log, mode_path, can0, phase_a, temp] = &PATHS;
    // --8<-- [start:define_on_mux]
    let host_clock = mux.define_clock(host)?;
    let board_clock = mux.define_clock_with_maps(board, &[(host_clock, &segments)])?;
    let on_board = |path, name, repeat, kind| track(board_clock, path, name, repeat, kind);
    Ok(DriveNumbers {
        board_clock,
        host_clock,
        log: mux.define_track(on_board(log, "log", TrackRepeat::Stream, events.clone()))?,
        mode: mux.define_track(on_board(mode_path, "mode", mode, events.clone()))?,
        can0: mux.define_track(on_board(can0, "can0", TrackRepeat::Stream, events.clone()))?,
        phase_a: mux.define_track(on_board(
            phase_a,
            "phase_a",
            TrackRepeat::Stream,
            TrackKind::Series(phase),
        ))?,
        temp: mux.define_track(on_board(temp, "temp", TrackRepeat::Stream, events))?,
    })
    // --8<-- [end:define_on_mux]
}
