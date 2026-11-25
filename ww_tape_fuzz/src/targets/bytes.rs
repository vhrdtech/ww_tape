//! The bytes track targets: `tape-bytes` reads values of every bytes track a file of any bytes seems to hold;
//! `tape-bytes-hostile` reads the values of a chain whose parts, seals and index entries lie behind good CRCs.

use std::collections::BTreeSet;

use crate::hostile;
use ww_tape::bytes_track::{Absence, ValueError, ValueId, ValueSeal, read_value};
use ww_tape::reader::OutOfRange;
use ww_tape::{BlockKind, ItemNum, TapeReader, TrackId};

/// Tracks of a file whose values `tape-bytes` reads.
const TRACKS_MAX: usize = 4;
/// Values of a track `tape-bytes` reads.
const VALUES_MAX: usize = 8;

type Read = (
    Result<ValueSeal, ValueError<OutOfRange, core::convert::Infallible>>,
    Vec<u8>,
);

/// Read value `value` of `track` from `pieces`, checking what holds of every read: a value absent sent nothing to
/// the sink, and one read whole has the length, hash and id its seal states.
fn read(pieces: &[&TapeReader<&[u8]>], track: TrackId, value: ValueId) -> Read {
    let mut out = Vec::new();
    let r = read_value(pieces, track, value, |b| {
        out.extend_from_slice(b);
        Ok(())
    });
    match &r {
        Ok(seal) => {
            assert_eq!(seal.id, value, "a value read is the one asked");
            assert_eq!(
                seal.len,
                out.len() as u64,
                "a value read has its stated length"
            );
            assert_eq!(
                &seal.hash,
                blake3::hash(&out).as_bytes(),
                "a value read has its stated hash"
            );
            assert!(seal.parts >= 1, "a value has its seal at least");
        }
        Err(ValueError::Absent { .. }) => {
            assert!(
                out.is_empty(),
                "nothing of an absent value reaches the sink"
            );
        }
        Err(_) => {}
    }
    (r, out)
}

/// The pieces of `r`, each opened as a reader of its own bytes.
fn split<'f>(f: &'f [u8], r: &TapeReader<&[u8]>) -> Vec<TapeReader<&'f [u8]>> {
    r.pieces()
        .iter()
        .filter_map(|p| {
            let from = usize::try_from(p.start).ok()?;
            let to = usize::try_from(p.end).ok()?;
            TapeReader::open(f.get(from..to)?).ok()
        })
        .collect()
}

/// Read `value` from `r` as one reader and, when every piece opens on its own, from a reader per piece: values read
/// whole both ways are the same.
fn read_both(f: &[u8], r: &TapeReader<&[u8]>, track: TrackId, value: ValueId) -> Read {
    let one = read(&[r], track, value);
    let each = split(f, r);
    if each.len() == r.pieces().len() {
        let refs: Vec<&TapeReader<&[u8]>> = each.iter().collect();
        let apart = read(&refs, track, value);
        if let (Ok(a), Ok(b)) = (&one.0, &apart.0) {
            assert_eq!(a, b, "a value reads the same from a reader per piece");
            assert_eq!(one.1, apart.1);
        }
    }
    one
}

/// `tape-bytes`: the input as a file; values of the tracks its index entries name, at their first items, read
/// whole or refused, never partial.
pub fn bytes(data: &[u8]) {
    let Ok(r) = TapeReader::open(data) else {
        return;
    };
    let mut asked: BTreeSet<(u64, u64)> = BTreeSet::new();
    for i in 0..r.pieces().len() {
        let Ok(entries) = r.entries(i) else {
            continue;
        };
        for e in entries.iter().filter(|e| e.kind == BlockKind::Events) {
            let tracks = asked.iter().map(|a| a.0).collect::<BTreeSet<_>>();
            if tracks.len() >= TRACKS_MAX && !tracks.contains(&e.track_id.get()) {
                continue;
            }
            let of_track = asked.iter().filter(|a| a.0 == e.track_id.get()).count();
            if of_track < VALUES_MAX {
                asked.insert((e.track_id.get(), e.first_item.get()));
            }
        }
    }
    for (track, item) in asked {
        // what holds of every read is checked inside
        drop(read_both(
            data,
            &r,
            TrackId::new(track),
            ValueId(ItemNum::new(item)),
        ));
    }
}

/// `tape-bytes-hostile`: a chain of values whose parts, seals and index entries lie behind good CRCs, or which is
/// damaged or cut.
///
/// A value read whole is the template's value of that id, whatever was forged: a torn value or a part inside a
/// value never reads whole. A value absent sent nothing to the sink. Nothing done to the template, each value reads
/// as written and the torn one as torn.
pub fn bytes_hostile(data: &[u8]) {
    let a = hostile::bytes_attacked(data);
    let t = hostile::bytes_template();
    let Ok(r) = TapeReader::open(&a.bytes[..]) else {
        assert!(!a.whole, "the template opens");
        return;
    };
    for (id, want) in &t.values {
        let (got, bytes) = read_both(&a.bytes, &r, t.track, *id);
        match (&got, want) {
            (Ok(_), Some(v)) => assert_eq!(&bytes, v, "a value read whole is the one written"),
            (Ok(_), None) => unreachable!("torn value {id:?} read whole"),
            (Err(e), Some(_)) if a.whole => unreachable!("value {id:?} of the template: {e}"),
            (Err(ValueError::Absent { why, .. }), None) if a.whole => {
                assert!(
                    matches!(why, Absence::Torn { .. }),
                    "the torn value is torn: {why:?}"
                );
            }
            (Err(e), None) if a.whole => unreachable!("torn value {id:?}: {e}"),
            (Err(_), _) => {}
        }
        // the part after a value's first is inside it, or the next value's first
        let next = ValueId(ItemNum::new(id.0.get().saturating_add(1)));
        if !t.values.iter().any(|(v, _)| *v == next) {
            let (got, _) = read_both(&a.bytes, &r, t.track, next);
            assert!(got.is_err(), "a part inside a value never reads as a value");
        }
    }
}
