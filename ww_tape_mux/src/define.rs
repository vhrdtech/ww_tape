//! Definitions sent to the writing thread: encoded on the caller's thread, since specs borrow, and held by the
//! thread's [Definitions] from the bytes.

use shrink_wrap::DeserializeShrinkWrap;
use ww_tape::open::encode;
use ww_tape::{
    ClockMap, ClockNum, ClockSpec, Definitions, Describe, DescribeError, MapSegment, TrackNum,
    TrackSpec,
};

/// The number a spec is encoded with before the thread numbers it: any, the thread drops it.
const UNNUMBERED: u32 = 1;

/// Track `t`, encoded.
pub(crate) fn track(t: TrackSpec<'_>) -> Result<Vec<u8>, DescribeError> {
    encode(&Describe::Track(t.numbered(TrackNum::new(UNNUMBERED))))
}

/// Clock `c` and its maps to the clocks `maps` name, encoded.
pub(crate) fn clock(
    c: ClockSpec<'_>,
    maps: &[(ClockNum, &[MapSegment])],
) -> Result<Vec<Vec<u8>>, DescribeError> {
    let from = ClockNum::new(UNNUMBERED);
    let mut items = vec![encode(&Describe::Clock(c.numbered(from)))?];
    for (to, segments) in maps {
        items.push(encode(&Describe::Map(ClockMap {
            from,
            to: *to,
            segments: shrink_wrap::RefVec::Slice { slice: segments },
        }))?);
    }
    Ok(items)
}

/// What `items` encode.
fn decode(items: &[Vec<u8>]) -> Result<Vec<Describe<'_>>, DescribeError> {
    items
        .iter()
        .map(|i| Describe::from_ww_bytes(i).map_err(DescribeError::Decode))
        .collect()
}

/// Hold the track `item` encodes in `defs`; its number.
pub(crate) fn hold_track(defs: &mut Definitions, item: &[u8]) -> Result<TrackNum, DescribeError> {
    match Describe::from_ww_bytes(item).map_err(DescribeError::Decode)? {
        Describe::Track(t) => defs.define_track(t.spec()),
        _ => Err(DescribeError::Unknown),
    }
}

/// Hold the clock and maps `items` encode ([clock]) in `defs`; the clock's number.
pub(crate) fn hold_clock(
    defs: &mut Definitions,
    items: &[Vec<u8>],
) -> Result<ClockNum, DescribeError> {
    let mut decoded = decode(items)?.into_iter();
    let Some(Describe::Clock(c)) = decoded.next() else {
        return Err(DescribeError::Unknown);
    };
    let mut maps: Vec<(ClockNum, Vec<MapSegment>)> = Vec::new();
    for d in decoded {
        let Describe::Map(m) = d else {
            return Err(DescribeError::Unknown);
        };
        let segments = m
            .segments
            .iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(DescribeError::Decode)?;
        maps.push((m.to, segments));
    }
    let maps: Vec<(ClockNum, &[MapSegment])> =
        maps.iter().map(|(to, s)| (*to, s.as_slice())).collect();
    defs.define_clock_with_maps(c.spec(), &maps)
}

/// Hold what `items` encode in `defs`, as one batch.
pub(crate) fn hold_all(defs: &mut Definitions, items: &[Vec<u8>]) -> Result<(), DescribeError> {
    defs.define_all(&decode(items)?)
}
