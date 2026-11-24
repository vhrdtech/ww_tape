//! The bodies of the ww_tape fuzz targets, one function per target: `fuzz/src/tape_<name>.rs` calls it with
//! libFuzzer's input.

mod chunk;
mod flags;
mod grid;
mod hostile;
mod index;
mod payloads;
mod reader;
mod reclose;
mod resume;
mod runs;
mod samples;
mod seal;
mod series;
mod stores;
mod summary;
mod times;

pub use chunk::chunk;
pub use flags::flags;
pub use grid::{GridReach, grid, grid_reach};
pub use hostile::{
    chunk_hostile, flags_hostile, grid_hostile, index_hostile, payloads_hostile, reader_hostile,
    records_hostile, samples_hostile, series_hostile, state_hostile, summary_hostile,
    times_hostile,
};
pub use index::index;
pub use payloads::payloads;
pub use reader::reader;
pub(crate) use reclose::build as reclosed;
pub use reclose::reclose;
pub(crate) use reclose::summarized;
pub use resume::resume;
pub use runs::{runs, runs_hostile};
pub(crate) use resume::{
    Defined, chain_template, chunk as chunk_of, options, removed_chain, template_definitions,
};
pub use samples::samples;
pub use seal::seal;
pub use series::series;
pub use stores::stores;
pub use summary::summary;
pub use times::times;

/// A fuzz target's body.
pub type Target = fn(&[u8]);

/// Every ww_tape fuzz target by its name in `fuzz/Cargo.toml`.
pub const ALL: [(&str, Target); 28] = [
    ("tape-chunk", chunk),
    ("tape-index", index),
    ("tape-payloads", payloads),
    ("tape-reader", reader),
    ("tape-samples", samples),
    ("tape-series", series),
    ("tape-summary", summary),
    ("tape-times", times),
    ("tape-chunk-hostile", chunk_hostile),
    ("tape-index-hostile", index_hostile),
    ("tape-payloads-hostile", payloads_hostile),
    ("tape-reader-hostile", reader_hostile),
    ("tape-records-hostile", records_hostile),
    ("tape-samples-hostile", samples_hostile),
    ("tape-series-hostile", series_hostile),
    ("tape-summary-hostile", summary_hostile),
    ("tape-times-hostile", times_hostile),
    ("tape-state-hostile", state_hostile),
    ("tape-resume", resume),
    ("tape-reclose", reclose),
    ("tape-flags", flags),
    ("tape-flags-hostile", flags_hostile),
    ("tape-stores", stores),
    ("tape-seal", seal),
    ("tape-grid", grid),
    ("tape-grid-hostile", grid_hostile),
    ("tape-runs", runs),
    ("tape-runs-hostile", runs_hostile),
];

/// The target named `name`.
pub fn by_name(name: &str) -> Option<Target> {
    ALL.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// Define `spec` in `defs` as track `number`, which no track has taken: the lower numbers still free go to
/// placeholder tracks, defined and closed, so a template keeps the numbers its seeds were written with.
///
/// # Panics
///
/// When `number` is taken, or a definition is refused.
pub(crate) fn define_track_as(
    defs: &mut ww_tape::Definitions,
    spec: &ww_tape::TrackSpec<'_>,
    number: ww_tape::TrackNum,
) -> ww_tape::TrackNum {
    let mut placeholders = vec![];
    for k in 0u32.. {
        let mut probe = defs.clone();
        let n = probe.define_track(spec.clone()).expect("the track holds");
        if n == number {
            *defs = probe;
            break;
        }
        assert!(n < number, "track {number} is taken: {n} comes next");
        let placeholder = ww_tape::TrackSpec {
            key: ww_tape::track_key(0x91ace, &[ww_tape::Segment::Index(k)]),
            label: "placeholder",
            ..spec.clone()
        };
        placeholders.push(defs.define_track(placeholder).expect("a placeholder holds"));
    }
    for p in placeholders {
        defs.close(p);
    }
    number
}
