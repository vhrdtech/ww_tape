//! `puts.svg`: a state track put before every block of data of a plain track, and the few puts that write. The
//! writer runs with its near rule shrunk to fit the drawing: `near` 8 KiB and copies 2 KiB apart instead of 32 MiB and
//! 1 MiB. What is written, and where, is read back from the bytes.

use anyhow::{Context, ensure};
use exact_units::{Bytes, Count};
use ww_tape::index::IndexPolicy;
use ww_tape::writer::NearPolicy;
use ww_tape::{
    Block, BlockKind, BlockWriter, ClockTime, Definitions, FLAG_REPEAT, FLAG_WITH_NEXT, FileBlocks,
    MemSink, StatePolicy, SyncPolicy, TapeWriter, TrackRepeat,
};

use crate::OrDebug;
use crate::defs::{CLOCK, clock, track};
use crate::svg::Svg;

/// The plain track of the data and the state track put before each of its blocks.
const DATA: u32 = 1;
const STATE: u32 = 5;
/// The shrunk near rule.
const NEAR: u32 = 8 * 1024;
const COPY_SPACING: u64 = 2 * 1024;
/// Blocks of data written, and the bytes of each one's record.
const BLOCKS: u32 = 26;
const RECORD: usize = 900;

/// What a block written by the run holds.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Kind {
    /// Data of the plain track.
    Data,
    /// Puts of definitions on track 0.
    Definitions,
    /// A put or copy of the state track's item.
    State,
}

struct Placed {
    at: u64,
    len: u64,
    kind: Kind,
    flags: u16,
}

/// The run: the end of its bytes, its record blocks, and how many puts the writer reported as written.
fn run() -> anyhow::Result<(u64, Vec<Placed>, usize)> {
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    w.set_producer("figures");
    w.set_index_policy(IndexPolicy::NONE)?;
    w.set_near_policy(NearPolicy {
        copy_spacing: COPY_SPACING,
        ..NearPolicy::default()
    });
    let mut defs = Definitions::new();
    defs.define_all(&[
        clock(),
        track(DATA, "readings", TrackRepeat::Stream),
        track(
            STATE,
            "link session",
            TrackRepeat::State(StatePolicy {
                near: Bytes::new(NEAR),
                copies: Count::new(1),
            }),
        ),
    ])?;
    let mut written = 0;
    for k in 0..BLOCKS {
        let tick = i64::from(k) * 1_000_000;
        if w.put(&defs, STATE, tick, b"session 7: 2 Mbit/s, CRC on")? {
            written += 1;
        }
        let mut buf = vec![0u8; RECORD + 64];
        let mut b =
            BlockWriter::new(&mut buf, BlockKind::Records, w.next_id(), &[CLOCK]).or_debug()?;
        b.push(DATA, ClockTime::new(CLOCK, tick), &[&[0x5a; RECORD]])
            .or_debug()?;
        let n = b.finish();
        w.write_block(&defs, buf.get(..n).context("the block")?)?;
    }
    let tape = w.sink().tape();
    w.finish()?;
    let bytes = tape.to_vec()?;

    let mut walk = FileBlocks::new(&bytes[..]);
    let mut out = Vec::new();
    let mut end = 0;
    while let Some(b) = walk.next_block() {
        let b: Block<'_> = b.or_debug()?;
        if b.kind != BlockKind::Records {
            continue;
        }
        let (flags, len) = (b.flags, u64::try_from(b.len())?);
        let tracks: Vec<u32> = b
            .records()
            .map(|r| r.map(|r| r.track))
            .collect::<Result<_, _>>()
            .or_debug()?;
        let at = walk.offset();
        let kind = if flags & FLAG_REPEAT == 0 {
            ensure!(tracks == [DATA], "a block of data holds the data track");
            Kind::Data
        } else if tracks.contains(&STATE) {
            // the first put of the state track carries its definition on track 0 too
            ensure!(
                tracks.iter().all(|t| *t == 0 || *t == STATE),
                "puts of track 0 and the state track: {tracks:?}"
            );
            Kind::State
        } else {
            ensure!(
                tracks.iter().all(|t| *t == 0),
                "puts of definitions: {tracks:?}"
            );
            Kind::Definitions
        };
        out.push(Placed {
            at,
            len,
            kind,
            flags,
        });
        end = at + len;
    }
    Ok((end, out, written))
}

fn flag_names(flags: u16) -> String {
    let mut names = vec![];
    if flags & FLAG_REPEAT != 0 {
        names.push("FLAG_REPEAT");
    }
    if flags & FLAG_WITH_NEXT != 0 {
        names.push("FLAG_WITH_NEXT");
    }
    names.join(" + ")
}

pub fn puts() -> anyhow::Result<String> {
    let (end, placed, written) = run()?;
    ensure!(
        placed.iter().filter(|p| p.kind == Kind::State).count() == written,
        "a block of the state track for every put the writer reports as written"
    );
    let (left, width_plot) = (128.0, 560.0);
    let kib = 1024.0;
    let end_kib = (end as f64 / kib).ceil();
    let px = |at: u64| left + at as f64 / kib / end_kib * width_plot;
    let (lane_defs, lane_state, lane_data, axis) = (34.0, 92.0, 150.0, 186.0);
    let mut s = Svg::new(
        left + width_plot + 24.0,
        axis + 86.0,
        &format!(
            "A state track put before every block of data: most puts write nothing; the first writes the item, a use {} KiB on writes one copy, and a use more than {} KiB after the newest record writes it again",
            COPY_SPACING / 1024,
            NEAR / 1024
        ),
    );
    s.text(left - 12.0, lane_defs + 4.0, "end", "", "track 0");
    s.text(
        left - 12.0,
        lane_defs + 19.0,
        "end",
        "soft small",
        "definitions",
    );
    s.text(left - 12.0, lane_state + 4.0, "end", "", "track 5, state");
    s.text(
        left - 12.0,
        lane_state + 19.0,
        "end",
        "soft small",
        "put at every use",
    );
    s.text(left - 12.0, lane_data + 4.0, "end", "", "track 1");
    s.text(left - 12.0, lane_data + 19.0, "end", "soft small", "data");

    // the newest record of the state item, the first of its cycle, and whether the cycle has its copy
    let mut last_state: Option<u64> = None;
    let mut cycle_first = 0u64;
    let mut copied = false;
    let mut state_flags = None;
    for p in &placed {
        let (x, w) = (px(p.at), (px(p.at + p.len) - px(p.at)).max(2.0));
        match p.kind {
            Kind::Data => {
                s.rect(
                    (x + 0.5, lane_data - 8.0),
                    (w - 1.0, 22.0),
                    2.0,
                    "tint-blue",
                );
                // a use of the state item: where the block of data lands
                s.line((x, lane_state + 18.0), (x, lane_state + 26.0), "thin");
            }
            Kind::Definitions => {
                s.rect((x, lane_defs - 8.0), (w.max(3.0), 22.0), 1.0, "purple");
            }
            Kind::State => {
                // the near rule, checked against where the writer put each record
                let what = match last_state {
                    None => "first",
                    Some(prev) if p.at - prev < u64::from(NEAR) => {
                        ensure!(!copied, "one copy per cycle");
                        ensure!(
                            p.at - cycle_first >= COPY_SPACING,
                            "a copy lies the copy spacing on"
                        );
                        copied = true;
                        "copy"
                    }
                    Some(_) => "again",
                };
                if what != "copy" {
                    cycle_first = p.at;
                    copied = false;
                }
                // where a use finds this record near: up to NEAR bytes on
                let reach = (p.at + u64::from(NEAR)).min(end);
                s.rect(
                    (x, lane_state - 6.0),
                    (px(reach) - x, 18.0),
                    2.0,
                    "tint-purple",
                );
                s.rect((x, lane_state - 8.0), (w.max(3.0), 22.0), 1.0, "purple");
                s.text(x + 5.0, lane_state - 13.0, "start", "purple small", what);
                last_state = Some(p.at);
                match state_flags {
                    None => state_flags = Some(p.flags),
                    Some(f) => ensure!(
                        f == p.flags,
                        "every put of the state track has one set of flags"
                    ),
                }
            }
        }
    }
    let state_flags = state_flags.context("the state track was written")?;
    // the axis, in KiB
    s.line((left, axis), (left + width_plot, axis), "thin");
    let step = 4u32;
    let mut k = 0u32;
    while f64::from(k) <= end_kib {
        let x = left + f64::from(k) / end_kib * width_plot;
        s.line((x, axis), (x, axis + 4.0), "thin");
        s.text(x, axis + 17.0, "middle", "soft small", &format!("{k}"));
        k += step;
    }
    s.text(
        left + width_plot / 2.0,
        axis + 34.0,
        "middle",
        "small",
        "offset in the piece, KiB",
    );
    s.text(
        left,
        axis + 58.0,
        "start",
        "soft small",
        &format!(
            "Shaded: the {} KiB after a record where a use finds it near. Puts that write go in blocks marked {}.",
            NEAR / 1024,
            flag_names(state_flags)
        ),
    );
    s.text(
        left,
        axis + 76.0,
        "start",
        "soft small",
        "Ticks under the state lane: the uses, one before each block of data.",
    );
    Ok(s.finish())
}
