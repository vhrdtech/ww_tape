//! Runs of blocks marked FLAG_WITH_NEXT: an unclosed piece a writer wrote with groups of puts and puts before blocks
//! of data, whose blocks gain or lose the mark behind good CRCs, take damage, lose a stretch to zeros, or are cut;
//! and a variant with a run longer than any writer makes, built by hand.

use std::sync::OnceLock;

use ww_tape::{
    BlockId, BlockKind, ClockTime, FLAG_GROUP, FLAG_REPEAT, FLAG_WITH_NEXT, FileBlocks, GroupPut,
    MemSink, RUN_BLOCKS_MAX, Recovery, TapeWriter,
};

use super::{Draw, HEADER_LEN, SESSION, seal};
use crate::hostile::{state_definitions, state_options, state_records};

/// Groups in the template.
const GROUPS: i64 = 6;
/// Bytes of an item of the template's large groups: two don't fit one block of puts.
pub(crate) const LARGE: usize = (32 << 10) + 1;

/// The item of group `k` on state track `t` (0 or 1), `len` bytes.
pub(crate) fn item(k: i64, t: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (k as u8).wrapping_mul(31) ^ (t as u8) ^ (i as u8))
        .collect()
}

/// The template: groups of puts on the two state tracks, every other one large, each followed by a records block of
/// the stream track; left unclosed, so a reader scans its end.
fn template() -> Vec<u8> {
    let o = state_options();
    let mut w = TapeWriter::create_on(MemSink::new(), o.sync);
    w.set_index_policy(o.index)
        .expect("a policy with room for a piece head");
    w.set_near_policy(o.near);
    w.set_layout(o.layout).expect("a new piece takes a layout");
    w.set_producer(&o.producer);
    w.set_session(SESSION).expect("a writable session");
    let (defs, on) = state_definitions(None);
    for k in 0..GROUPS {
        let len = if k % 2 == 0 { LARGE } else { 40 };
        let items = [item(k, 0, len), item(k, 1, len)];
        let puts = [0, 1].map(|t| GroupPut {
            track: on.states[t],
            ticks: k,
            item: &items[t],
        });
        w.put_group(&defs, &puts).expect("a group");
        w.write_block(&defs, &state_records(w.next_id(), on, k))
            .expect("a records block");
    }
    let tape = w.sink().tape();
    w.sync().expect("a memory sink syncs");
    drop(w);
    tape.to_vec().expect("the bytes")
}

/// The template with a run of [RUN_BLOCKS_MAX] blocks of puts marked [FLAG_WITH_NEXT] at its end and an unmarked
/// one after them: longer than any run a writer makes.
fn long_template() -> Vec<u8> {
    let mut f = template_bytes().to_vec();
    let (_, on) = state_definitions(None);
    let last = blocks(&f).last().map(|(at, ..)| *at).unwrap_or(0);
    let seq = ww_tape::Block::read(&f[last..]).map(|b| b.seq).unwrap_or(0);
    for k in 0..=RUN_BLOCKS_MAX {
        let mut buf = [0u8; 96];
        let mut b = ww_tape::BlockWriter::new(
            &mut buf,
            BlockKind::Records,
            BlockId::new(SESSION, seq),
            &[on.clock],
        )
        .expect("a block fits");
        b.push(
            on.states[k % 2],
            ClockTime::new(on.clock, k as i64),
            &[&(k as u32).to_le_bytes()],
        )
        .expect("a record fits");
        let len = b.finish();
        let block = &mut buf[..len];
        let flags = if k < RUN_BLOCKS_MAX {
            FLAG_REPEAT | FLAG_WITH_NEXT
        } else {
            FLAG_REPEAT
        };
        block[6..8].copy_from_slice(&flags.to_le_bytes());
        seal(block);
        f.extend_from_slice(block);
    }
    f
}

fn template_bytes() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(template)
}

/// The template with a run too long ([long_template]), made once.
pub(crate) fn long_bytes() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(long_template)
}

/// The template's bytes: the run of groups and blocks of data a fuzz target cuts and checks against.
pub fn runs_template() -> &'static [u8] {
    template_bytes()
}

/// The blocks of `f`: each one's offset, length and kind.
pub(crate) fn blocks(f: &[u8]) -> Vec<(usize, usize, BlockKind)> {
    let mut rd = FileBlocks::with_recovery(f, Recovery::Prefix);
    let mut out = vec![];
    while let Some(Ok(b)) = rd.next_block() {
        let (len, kind) = (b.len(), b.kind);
        out.push((rd.offset() as usize, len, kind));
    }
    out
}

/// Whether the block at `at` of `f` reads and is marked [FLAG_WITH_NEXT].
pub(crate) fn marked(f: &[u8], at: usize) -> bool {
    f.get(at..)
        .and_then(|b| ww_tape::Block::read(b).ok())
        .is_some_and(|b| b.flags & FLAG_WITH_NEXT != 0)
}

/// The template, or by the input its variant with a run too long, with one to three attacks on its runs drawn from
/// `data`, then perhaps cut.
///
/// An attack marks a block of records or takes its mark away ([FLAG_WITH_NEXT] or [FLAG_GROUP]) behind a good CRC,
/// damages a marked block, zeros a stretch from inside a run, or cuts inside a run.
pub fn runs(data: &[u8]) -> Vec<u8> {
    let mut d = Draw::new(data);
    let mut f = if d.below(8) == 0 {
        long_bytes().to_vec()
    } else {
        template_bytes().to_vec()
    };
    let all = blocks(&f);
    let records: Vec<_> = all
        .iter()
        .filter(|(_, _, k)| *k == BlockKind::Records)
        .collect();
    let runs: Vec<_> = all.iter().filter(|(at, ..)| marked(&f, *at)).collect();
    for _ in 0..1 + d.below(3) {
        match d.below(4) {
            0 => {
                if let Some((at, len, _)) = records.get(d.below(records.len().max(1))) {
                    let flag = if d.bool() { FLAG_WITH_NEXT } else { FLAG_GROUP };
                    let b = &mut f[*at..at + len];
                    let flags = u16::from_le_bytes([b[6], b[7]]) ^ flag;
                    b[6..8].copy_from_slice(&flags.to_le_bytes());
                    seal(b);
                }
            }
            1 => {
                if let Some((at, len, _)) = runs.get(d.below(runs.len().max(1))) {
                    let i = HEADER_LEN + d.below(len.saturating_sub(HEADER_LEN).max(1));
                    if let Some(byte) = f.get_mut(at + i) {
                        *byte ^= d.u8() | 1;
                    }
                }
            }
            2 => {
                if let Some((at, len, _)) = runs.get(d.below(runs.len().max(1))) {
                    let n = 1 + d.below(2 * len);
                    let end = (at + n).min(f.len());
                    f[*at..end].fill(0);
                }
            }
            _ => {
                if let Some((at, len, _)) = runs.get(d.below(runs.len().max(1))) {
                    f.truncate(at + d.below(len + 1));
                    break;
                }
            }
        }
    }
    if d.bool() {
        let cut = d.below(f.len() + 1);
        f.truncate(cut);
    }
    f
}
