//! A damaged last tail never costs a complete close: a resume refuses every flip and rot of it, a tail ending in a
//! zero byte included, and cuts only what a crash leaves (bytes missing, or zeros from a sector boundary or from the
//! tail's first byte).

use crate::reclose::{reopen, write};
use crate::resume::begin;
use ww_tape::piece::TAIL_LEN;
use ww_tape::{ALIGNMENT, MemSink, TapeSink, TapeWriter};

/// A piece closed `n` times, `per` record blocks per stretch from tick `from`: the bytes after each close.
fn closed_from(n: usize, per: i64, alignment: u32, from: i64) -> Vec<Vec<u8>> {
    let mut w = begin(alignment);
    write(&mut w, from..from + per);
    let tape = w.sink().tape();
    w.close().unwrap();
    let mut out = vec![tape.to_vec().unwrap()];
    for k in 1..n as i64 {
        let (mut w, _) = reopen(out.last().unwrap(), alignment);
        write(&mut w, from + k * per..from + (k + 1) * per);
        let tape = w.sink().tape();
        w.close().unwrap();
        out.push(tape.to_vec().unwrap());
    }
    out
}

/// What a resume of `bytes` does: the bytes it keeps or its error, and the tape after.
fn resume_of(bytes: &[u8], alignment: u32) -> (Result<u64, ww_tape::ResumeError>, Vec<u8>) {
    let mut sink = MemSink::new();
    sink.write_all(bytes).unwrap();
    let tape = sink.tape();
    let r = match TapeWriter::resume_on(sink, crate::resume::options(alignment)) {
        Ok((_, resumed)) => Ok(resumed.kept_bytes),
        Err(ww_tape::WriteError::Resume(e)) => Err(e),
        Err(e) => panic!("{e:?}"),
    };
    (r, tape.to_vec().unwrap())
}

/// The first piece closed twice whose last byte, the top byte of its tail's CRC, is zero: one in 256.
fn ending_in_zero(alignment: u32) -> Vec<u8> {
    (0..6000)
        .map(|k| closed_from(2, 3, alignment, k * 10).pop().unwrap())
        .find(|p| p.last() == Some(&0))
        .expect("a tail ending in a zero byte within 6000 pieces")
}

/// Every bit of the last tail flipped, one at a time: the resume refuses and changes no byte. Pieces whose tail ends
/// in a zero byte too, where the end read as blank before.
#[test]
fn a_flip_of_any_bit_of_the_last_tail_refuses() {
    for alignment in [0, ALIGNMENT] {
        for piece in [
            closed_from(3, 6, alignment, 0).pop().unwrap(),
            ending_in_zero(alignment),
        ] {
            let tail = piece.len() - TAIL_LEN;
            for at in tail..piece.len() {
                for bit in 0..8 {
                    let mut d = piece.clone();
                    d[at] ^= 1 << bit;
                    let (r, after) = resume_of(&d, alignment);
                    assert!(
                        r.is_err(),
                        "alignment {alignment}, bit {bit} at {at}: {r:?}"
                    );
                    assert!(
                        after == d,
                        "alignment {alignment}, bit {bit} at {at}: bytes changed"
                    );
                }
            }
        }
    }
}

/// Rot of the last tail's header or end refuses with the bytes kept. What a crash leaves (the last bytes missing, the
/// tail or the last 4 KiB zero) is cut back to the data before the final index.
#[test]
fn rot_in_the_last_tail_refuses_and_a_crash_is_cut() {
    for alignment in [0, ALIGNMENT] {
        let all = closed_from(3, 6, alignment, 0);
        let (f, before) = (&all[2], all[1].len());
        let n = f.len();
        let t = n - TAIL_LEN;
        let edit = |change: &dyn Fn(&mut Vec<u8>)| {
            let mut d = f.clone();
            change(&mut d);
            d
        };
        let rotted = [
            (
                "kind and length",
                edit(&|d| (d[t + 5], d[t + 8]) = (0x63, 0xff)),
            ),
            (
                "4 bytes at 5..9",
                edit(&|d| d[t + 5..t + 9].copy_from_slice(&[0x63, 0xa5, 0x5a, 0xff])),
            ),
            ("length", edit(&|d| d[t + 8] = 0xff)),
            ("kind", edit(&|d| d[t + 5] = 0x63)),
            ("last byte zero", edit(&|d| d[n - 1] = 0)),
            ("last 2 bytes zero", edit(&|d| d[n - 2..].fill(0))),
            ("last 64 bytes zero", edit(&|d| d[n - 64..].fill(0))),
        ];
        for (name, d) in rotted {
            let (r, after) = resume_of(&d, alignment);
            assert!(r.is_err(), "alignment {alignment}, {name}: {r:?}");
            assert!(after == d, "alignment {alignment}, {name}: bytes changed");
        }
        let crashed = [
            ("cut by 1 byte", edit(&|d| d.truncate(n - 1))),
            ("whole tail zero", edit(&|d| d[t..].fill(0))),
            ("last 4 KiB zero", edit(&|d| d[n - 4096..].fill(0))),
        ];
        for (name, d) in crashed {
            let (r, after) = resume_of(&d, alignment);
            let kept = r.unwrap_or_else(|e| panic!("alignment {alignment}, {name}: {e:?}"));
            assert!(
                kept as usize >= before,
                "alignment {alignment}, {name}: the close before stays"
            );
            assert_eq!(after.len() as u64, kept, "alignment {alignment}, {name}");
            assert_eq!(after[..], f[..after.len()], "alignment {alignment}, {name}");
        }
    }
}

/// A splitmix64 stream.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Runs of 2 to 64 random or zero bytes at random places in the last 16 KiB of pieces closed two and three times,
/// 20,000 per piece, fill and layout: a resume never cuts a complete close. A run this short never blanks a whole
/// tail, the one zero fill a crash can leave there.
#[test]
fn rot_never_cuts_a_complete_close() {
    const SEED: u64 = 0x7732_625f_3730_3037;
    const TRIALS: u64 = 20_000;
    let mut cut = vec![];
    for alignment in [0, ALIGNMENT] {
        let all = closed_from(3, 6, alignment, 0);
        for (bi, base) in [&all[1], &all[2]].into_iter().enumerate() {
            for zeros in [false, true] {
                let mut rng = Rng(SEED
                    ^ (u64::from(alignment) << 32)
                    ^ ((bi as u64 + 1) << 8)
                    ^ u64::from(zeros));
                let lo = base.len().saturating_sub(16384);
                for trial in 0..TRIALS {
                    let at = lo + rng.below((base.len() - lo) as u64) as usize;
                    let end = (at + 2 + rng.below(63) as usize).min(base.len());
                    let mut d = base.clone();
                    for b in &mut d[at..end] {
                        *b = if zeros { 0 } else { rng.next() as u8 };
                    }
                    let (r, after) = resume_of(&d, alignment);
                    if r.is_err() {
                        assert!(after == d, "trial {trial}: a refusal changed bytes");
                    } else if after.len() < base.len() {
                        cut.push((alignment, bi + 2, zeros, trial, at..end));
                    }
                }
            }
        }
    }
    assert_eq!(
        cut,
        vec![],
        "(alignment, closes, zeros, trial, bytes) that cut a complete close"
    );
}

/// Where the close record in force of `bytes` starts, and its final index.
fn last_close(bytes: &[u8]) -> (usize, usize) {
    let (at, c) = crate::reclose::closes(bytes).pop().unwrap();
    (at as usize, c.final_index.unwrap() as usize)
}

/// A close record that fails its check and runs into zeros to the end: refused when the zeros start inside its last
/// sector, cut back to the close's final index when they start in an earlier sector of it and so cross a sector
/// boundary, as a write cut off by a crash leaves them.
#[test]
fn a_damaged_close_is_cut_only_where_its_zeros_cross_a_sector_boundary() {
    for alignment in [0, ALIGNMENT] {
        let f = closed_from(3, 6, alignment, 0).pop().unwrap();
        let n = f.len();
        let (close_at, index_at) = last_close(&f);
        let close_end = n - TAIL_LEN;
        assert!(
            close_end % 512 > 2 && close_end - close_at > 1024,
            "alignment {alignment}"
        );
        let mut d = f.clone();
        d[close_end - 2..].fill(0);
        let (r, after) = resume_of(&d, alignment);
        assert!(
            matches!(r, Err(ww_tape::ResumeError::Damaged { .. })),
            "alignment {alignment}, zeros inside the last sector: {r:?}"
        );
        assert!(after == d, "alignment {alignment}: bytes changed");
        let from = (close_at + 4).max(close_end - close_end % 512 - 100);
        let mut d = f.clone();
        d[from..].fill(0);
        let (r, after) = resume_of(&d, alignment);
        assert_eq!(
            r.unwrap_or_else(|e| panic!("alignment {alignment}, zeros from {from}: {e:?}")),
            index_at as u64,
            "alignment {alignment}: cut back to the close's final index"
        );
        assert_eq!(after[..], f[..index_at], "alignment {alignment}");
        if alignment == 0 {
            // the example of docs/format.md, "What counts as a torn end"
            assert_eq!((n, index_at), (12288, 9541));
            let mut d = f.clone();
            d[9641..].fill(0);
            assert_eq!(resume_of(&d, 0).0.unwrap(), 9541);
        }
    }
}

/// A good tail in the bytes a resume would cut that follows no close record is damage: the bytes before the piece's
/// last final index, then its tail, refuse at the tail and keep every byte.
#[test]
fn a_tail_after_no_close_refuses() {
    for alignment in [0, ALIGNMENT] {
        let f = closed_from(3, 6, alignment, 0).pop().unwrap();
        let (_, index_at) = last_close(&f);
        let tail_at = f.len() - TAIL_LEN;
        let d = [&f[..index_at], &f[tail_at..]].concat();
        let (r, after) = resume_of(&d, alignment);
        assert!(
            matches!(r, Err(ww_tape::ResumeError::Damaged { offset }) if offset == index_at as u64),
            "alignment {alignment}: {r:?}"
        );
        assert!(after == d, "alignment {alignment}: bytes changed");
    }
}
