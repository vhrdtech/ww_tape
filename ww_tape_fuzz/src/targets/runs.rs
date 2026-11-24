//! `tape-runs` and `tape-runs-hostile`: runs of blocks marked [FLAG_WITH_NEXT](ww_tape::FLAG_WITH_NEXT), taken whole or not at all by a scan, a
//! follower and a resume.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ww_tape::{
    GroupPut, Medium, MemSink, OpenOptions, PieceTapes, Slot, TapeReader, TapeSink, TapeWriter,
};
use ww_tape_fs::follow::{ChainFollower, FollowError, FollowOptions};

use crate::hostile::{self, runs_item as item, runs_large as LARGE};

/// A file of its own for each case of this process.
fn scratch_file(bytes: &[u8]) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ww_tape_fuzz_runs_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let path = dir.join(format!("{n}.wwt"));
    std::fs::write(&path, bytes).expect("a scratch file");
    path
}

/// Every block a follower hands out of the file holding `bytes`, by offset, each checked against the bytes; whether
/// it stopped at a run too long.
fn followed(bytes: &[u8]) -> (Vec<u64>, bool) {
    let path = scratch_file(bytes);
    let mut out = Vec::new();
    let mut too_long = false;
    if let Ok(mut f) = ChainFollower::new(
        &path,
        FollowOptions {
            tapes: PieceTapes::One,
            ..FollowOptions::default()
        },
    ) {
        loop {
            match f.try_next() {
                Ok(Some(b)) => {
                    let at = b.offset as usize;
                    let got = b.block.bytes();
                    assert_eq!(
                        bytes.get(at..at + got.len()),
                        Some(got),
                        "a block handed out is the file's"
                    );
                    out.push(b.offset);
                }
                Ok(None) => break,
                Err(FollowError::Definitions { .. }) => {}
                Err(FollowError::RunTooLong { .. }) => {
                    too_long = true;
                    break;
                }
                Err(_) => break,
            }
        }
    }
    _ = std::fs::remove_file(&path);
    (out, too_long)
}

/// Whether the block at `at` of `f` is marked [FLAG_WITH_NEXT](ww_tape::FLAG_WITH_NEXT).
fn marked(f: &[u8], at: u64) -> bool {
    hostile::runs_marked(f, at as usize)
}

/// What every reader of hostile bytes keeps to: a scan lists no block inside a run it dropped; a follower hands out no marked block last; a resume drops a run only from where it keeps to.
fn invariants(f: &[u8]) {
    let file = OpenOptions::default();
    let raw = OpenOptions {
        medium: Medium::Raw,
        layout: Some(hostile::state_options().layout),
        ..file
    };
    for options in [file, raw] {
        let Ok(r) = TapeReader::open_with(f, options) else {
            continue;
        };
        for (i, p) in r.pieces().iter().enumerate() {
            let Ok(entries) = r.entries(i) else {
                continue;
            };
            for e in &entries {
                let at = p.start + e.offset;
                for (run, len) in &p.dropped_runs {
                    assert!(
                        at < *run || at >= run + len,
                        "an entry at {at} inside a dropped run"
                    );
                }
            }
        }
    }
    let (out, too_long) = followed(f);
    if let Some(last) = out.last()
        && !too_long
    {
        assert!(
            !marked(f, *last),
            "a follower hands out a marked block last"
        );
    }
    let mut sink = MemSink::new();
    sink.write_all(f).expect("a memory sink takes the bytes");
    if let Ok((_, resumed)) = TapeWriter::resume_on(sink, hostile::state_options())
        && let Some(run) = &resumed.dropped_run
    {
        assert_eq!(
            run.start, resumed.kept_bytes,
            "a dropped run starts where the kept bytes end"
        );
    }
}

/// `tape-runs-hostile`: the run template with its marks taken away or added behind good CRCs, damage, zeros and cuts
/// inside runs, and a run longer than a writer makes ([hostile::runs]); every reader keeps to [invariants].
pub fn runs_hostile(data: &[u8]) {
    invariants(&hostile::runs(data));
}

/// `tape-runs`: groups of puts the input sizes, on one or both state tracks, each perhaps with a records block
/// after it, cut at a byte the input picks. A scan holds in force the items of the last group whole before the cut
/// and none of a torn one; a follower hands out no block of a torn group; a resume drops a torn group, after which
/// the group given again is in force.
pub fn runs(data: &[u8]) {
    let mut d = crate::hostile::Draw::new(data);
    let o = hostile::state_options();
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_index_policy(o.index).expect("a policy");
    w.set_near_policy(o.near);
    w.set_layout(o.layout).expect("a layout");
    w.set_producer(&o.producer);
    w.set_session(hostile::SESSION).expect("a session");
    let tape = w.sink().tape();
    let (defs, on) = hostile::state_definitions(None);
    // per group: its start and end, and its items by track
    let mut groups: Vec<(u64, u64, Vec<(usize, Vec<u8>)>)> = Vec::new();
    for k in 0..1 + d.below(5) as i64 {
        let len = [8, 300, LARGE, 50 << 10][d.below(4)];
        let tracks: &[usize] = [&[0usize][..], &[1], &[0, 1]][d.below(3)];
        let items: Vec<(usize, Vec<u8>)> = tracks
            .iter()
            .map(|t| (*t, item(k + 100, *t, len)))
            .collect();
        let puts: Vec<GroupPut<'_>> = items
            .iter()
            .map(|(t, i)| GroupPut {
                track: on.states[*t],
                ticks: k,
                item: i,
            })
            .collect();
        let start = w.bytes();
        w.put_group(&defs, &puts).expect("a group");
        groups.push((start, w.bytes(), items));
        if d.bool() {
            w.write_block(&defs, &hostile::state_records(w.next_id(), on, k))
                .expect("a records block");
        }
    }
    let full = tape.to_vec().expect("the bytes");
    let cut = d.below(full.len() + 1);
    let f = &full[..cut];
    let cut = cut as u64;
    // the item in force on each track: that of the last group whole before the cut
    let mut want: [Option<&[u8]>; 2] = [None, None];
    let mut torn = None;
    for (start, end, items) in &groups {
        if *end <= cut {
            for (t, i) in items {
                want[*t] = Some(i);
            }
        } else if *start < cut {
            torn = Some((*start, *end, items));
        }
    }
    if let Ok(r) = TapeReader::open(f) {
        for t in 0..2 {
            let got = r
                .state_at(
                    0,
                    Slot::State {
                        track: on.states[t],
                    },
                    u64::MAX,
                )
                .ok()
                .and_then(|l| l.item)
                .map(|i| i.item);
            assert_eq!(got.as_deref(), want[t], "track {t} after a cut at {cut}");
        }
    }
    let (out, _) = followed(f);
    if let Some((start, end, _)) = torn {
        assert!(
            out.iter().all(|o| *o < start || *o >= end),
            "a follower hands out a block of a torn group"
        );
    }
    let mut sink = MemSink::new();
    sink.write_all(f).expect("a memory sink takes the bytes");
    let resumed_tape = sink.tape();
    let Ok((mut again, resumed)) = TapeWriter::resume_on(sink, hostile::state_options()) else {
        return;
    };
    let Some((start, _, items)) = torn else {
        return;
    };
    assert!(resumed.kept_bytes <= start, "a resume drops a torn group");
    let puts: Vec<GroupPut<'_>> = items
        .iter()
        .map(|(t, i)| GroupPut {
            track: on.states[*t],
            ticks: 0,
            item: i,
        })
        .collect();
    again.put_group(&defs, &puts).expect("the group again");
    let bytes = resumed_tape.to_vec().expect("the bytes");
    let r = TapeReader::open(&bytes[..]).expect("the resumed piece reads");
    for (t, i) in items {
        let got = r
            .state_at(
                0,
                Slot::State {
                    track: on.states[*t],
                },
                u64::MAX,
            )
            .expect("a lookup")
            .item
            .map(|x| x.item);
        assert_eq!(
            got.as_deref(),
            Some(&i[..]),
            "the group given again is in force"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of more blocks than [ww_tape::RUN_BLOCKS_MAX] stops a follower, which hands out none of it; a scan lists
    /// it as dropped and none of its blocks.
    #[test]
    fn a_run_too_long_stops_a_follower() {
        let f = hostile::runs_long();
        let (out, too_long) = followed(f);
        assert!(too_long, "the follower stops at the run");
        let run_at = hostile::runs_template().len() as u64;
        assert!(
            out.iter().all(|o| *o < run_at),
            "none of the run handed out"
        );
        let r = TapeReader::open(f).expect("the piece opens");
        let p = &r.pieces()[0];
        assert_eq!(p.dropped_runs.first().map(|(at, _)| *at), Some(run_at));
        invariants(f);
    }
}
