//! A series chunk's flag runs on any bytes: `[count u32][column]` read as the runs of a chunk of `count` samples
//! answers without panic, and runs it takes are in the writer's one form; pushes drawn from the input give a column a
//! reader takes back to the same flags.
use ww_tape::flag_runs::{FLAG_RUNS_MAX, FlagRuns, FlagRunsBuilder};
use ww_tape::{Block, SampleShape};

/// The runs `runs` of a chunk of `count` samples hold the format's rules: at most [FLAG_RUNS_MAX], their spans cover
/// the samples in order, each sample's word is its span's, and a builder given the spans writes the same column.
pub(crate) fn check(runs: &FlagRuns<'_>, count: u32, column: &[u8]) {
    assert!(runs.len() <= FLAG_RUNS_MAX, "{} runs", runs.len());
    let mut at = 0u32;
    let mut b = FlagRunsBuilder::new();
    for (span, flags) in runs.spans(count) {
        assert_eq!(span.start, at, "spans cover the samples in order");
        assert!(span.end > at, "a span holds a sample");
        assert_eq!(
            runs.flags_at(span.start),
            flags,
            "the word of the span's first sample"
        );
        assert_eq!(runs.flags_at(span.end - 1), flags, "the word of its last");
        b.push(span.end - span.start, flags)
            .expect("the runs of a column fit a builder");
        at = span.end;
    }
    assert_eq!(at, count, "spans end at the count");
    let mut again = vec![];
    b.write(&mut again);
    assert_eq!(again, column, "one form only");
}

/// The flag runs of the chunk block `b`, read and salvaged, checked when they read; the samples pushed with them into
/// series summaries of `shape` when given.
pub(crate) fn walk(b: &[u8], shape: Option<SampleShape>) {
    let Ok(block) = Block::salvage(b) else { return };
    let Some(c) = block.chunk() else { return };
    let count = c.head().count;
    let Ok(runs) = c.flag_runs() else { return };
    let column = c
        .find(ww_tape::ColumnKind::Flags, 0)
        .map_or(&[][..], |col| col.bytes);
    check(&runs, count, column);
    let Some(shape) = shape else { return };
    let (Some(lanes), Ok(n)) = (shape.lanes(), usize::try_from(count)) else {
        return;
    };
    // a chunk's samples of the hostile track are i16: summaries over zeros, flagged as the runs say
    let Some(len) = n.checked_mul(lanes as usize).filter(|l| *l <= 1 << 16) else {
        return;
    };
    let spec = ww_tape::summary::SeriesSummarySpec {
        base_shift: 4,
        stats: ww_tape::summary::SeriesStats::all(shape.element),
        clip: None,
    };
    let start = ww_tape_summary::SeriesStart {
        track_id: c.head().track_id,
        track_number: c.head().track_number,
        shape,
        first_item: c.head().first_item,
        grid: ww_tape::SeriesGrid::start(c.head().first_item.get(), 0, ww_tape::Magnitude::ONE),
        period: ww_tape::Ratio64::int(1).expect("a ratio"),
        tile: ww_tape::TileCoords::NONE,
        tile_cells: 1,
    };
    if let Ok(mut s) = ww_tape_summary::SeriesSummaries::<i16>::new(start, spec) {
        let values = vec![0i16; len];
        let _ = s.push_runs(&values, &runs);
        let _ = s.finish();
    }
}

pub fn flags(data: &[u8]) {
    let (count, column) = data.split_at_checked(4).unwrap_or((&[0; 4], data));
    let count = u32::from_le_bytes(count.try_into().expect("4 bytes"));
    if let Ok(runs) = FlagRuns::of(column, count) {
        check(&runs, count, column);
    }
    // pushes from the input: a count and a word of few bits each, read back as each sample's word
    let mut b = FlagRunsBuilder::new();
    let mut want = vec![];
    for p in data.chunks(2).take(512) {
        let n = u32::from(p[0] % 8);
        let flags = u32::from(p.get(1).copied().unwrap_or(0) % 4);
        if b.push(n, flags).is_ok() {
            want.extend(std::iter::repeat_n(flags, n as usize));
        }
    }
    let total = want.len() as u32;
    let mut column = vec![];
    b.write(&mut column);
    if b.is_empty() {
        assert!(column.is_empty(), "no run, no column");
        return;
    }
    let runs = FlagRuns::of(&column, total).expect("a builder's column reads back");
    check(&runs, total, &column);
    for (i, w) in want.iter().enumerate() {
        assert_eq!(runs.flags_at(i as u32), *w, "sample {i}");
    }
}
