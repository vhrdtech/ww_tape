//! Bytes tracks: values written streamed as parts and read whole with their hash checked; torn values, damage,
//! values across cuts and removed pieces (TRACK-10).

use ww_tape::bytes_track::{
    Absence, BytesOptions, BytesPart, BytesWriter, PART_FIRST, PART_SEAL, PartDamage, ValueError,
    ValueId, ValueSeal, WriteValueError, read_value,
};
use ww_tape::payloads::PayloadCodec;
use ww_tape::{
    ByteSource, ChainOptions, CutPolicy, Definitions, Describe, DescribeError, Integrity, ItemNum,
    Items, MemPieces, OwnerId, PieceChain, PieceStore, PieceTapes, SyncPolicy, TapeReader,
    TrackDef, TrackId, TrackKind, TrackNum, TrackRepeat, WriteError, WriterOptions,
};

use crate::definitions::{CLOCK, base_defs, hold, track};

pub(crate) const BYTES: TrackNum = TrackNum::new(10);
const OWNER: OwnerId = OwnerId([0xb7; 32]);
/// Bytes of a part in these tests: small, so a value of a few KiB takes several.
const PART: usize = 1000;

/// Bytes track `number` on [CLOCK], its items raw bytes.
pub(crate) fn bytes_def(number: u32) -> Describe<'static> {
    let Describe::Track(t) = track(number, CLOCK) else {
        unreachable!("track() gives a track")
    };
    Describe::Track(TrackDef {
        items: Items::RawBytes,
        kind: TrackKind::Bytes,
        ..t
    })
}

pub(crate) fn bytes_id() -> TrackId {
    let Describe::Track(t) = bytes_def(BYTES.get()) else {
        unreachable!("bytes_def() gives a track")
    };
    t.id()
}

pub(crate) fn defs() -> Definitions {
    let mut defs = base_defs().clone();
    hold(&mut defs, &[bytes_def(BYTES.get())]).unwrap();
    defs
}

fn options(tapes: PieceTapes, integrity: Integrity, chunks: u64) -> ChainOptions {
    ChainOptions {
        tapes,
        writer: WriterOptions {
            sync: SyncPolicy::None,
            integrity,
            ..Default::default()
        },
        cut: CutPolicy {
            chunks: exact_units::Count::new(chunks),
            ..Default::default()
        },
    }
}

fn chain(options: ChainOptions) -> (MemPieces, PieceChain<MemPieces>) {
    let mem = MemPieces::new();
    let c = PieceChain::create_on(mem.clone(), OWNER, 0, &[], options).unwrap();
    (mem, c)
}

/// `len` bytes of value `seed`, none alike.
fn value(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) ^ (i >> 8) as u8)
        .collect()
}

fn writer(defs: &Definitions, next: u64) -> BytesWriter {
    BytesWriter::new(
        defs,
        BYTES,
        ItemNum::new(next),
        BytesOptions {
            part_len: PART,
            ..Default::default()
        },
    )
    .unwrap()
}

/// Write `bytes` as one value at `ticks` through `c`, in writes of 333 bytes.
fn put<T: PieceStore>(
    c: &mut PieceChain<T>,
    defs: &Definitions,
    w: &mut BytesWriter,
    ticks: i64,
    bytes: &[u8],
) -> ValueSeal {
    w.write_from(ticks, Chunks(bytes), |p: BytesPart<'_>| {
        c.write_chunk(defs, &p.head, p.columns()).map(drop)
    })
    .unwrap()
}

/// A reader giving its bytes 333 at a time.
struct Chunks<'a>(&'a [u8]);

impl std::io::Read for Chunks<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(333).min(self.0.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
    }
}

/// The pieces of `mem` in order, each as its bytes.
fn pieces(mem: &MemPieces) -> Vec<Vec<u8>> {
    mem.keys()
        .unwrap()
        .iter()
        .map(|k| mem.get(k).unwrap().unwrap().to_vec().unwrap())
        .collect()
}

type Read = Result<
    (Vec<u8>, ValueSeal),
    ValueError<<&'static [u8] as ByteSource>::Error, std::convert::Infallible>,
>;

fn read_from(files: &[Vec<u8>], id: ValueId) -> Read {
    let readers: Vec<TapeReader<&[u8]>> = files
        .iter()
        .map(|b| TapeReader::open(&b[..]).unwrap())
        .collect();
    let refs: Vec<&TapeReader<&[u8]>> = readers.iter().collect();
    let mut out = Vec::new();
    let seal = read_value(&refs, bytes_id(), id, |b| {
        out.extend_from_slice(b);
        Ok(())
    })?;
    Ok((out, seal))
}

/// Values of several parts, of exactly a part, of less than a part and of no bytes read back whole, each with its
/// length, hash and parts; the piece verifies.
#[test]
fn values_read_whole_with_their_hash() {
    let defs = defs();
    let (mem, mut c) = chain(options(PieceTapes::One, Integrity::Full, 1 << 20));
    let mut w = writer(&defs, 0);
    let values = [
        value(1, 3500),
        value(2, PART),
        value(3, 10),
        Vec::new(),
        value(4, 2 * PART),
    ];
    let seals: Vec<ValueSeal> = values
        .iter()
        .zip(0i64..)
        .map(|(v, t)| put(&mut c, &defs, &mut w, t, v))
        .collect();
    let parts: Vec<u64> = seals.iter().map(|s| s.parts).collect();
    assert_eq!(parts, [5, 2, 2, 1, 3], "data parts and a seal each");
    assert_eq!(seals[0].id, ValueId(ItemNum::new(0)));
    assert_eq!(seals[1].id, ValueId(ItemNum::new(5)));
    assert_eq!(w.next_part(), ItemNum::new(13));
    c.close().unwrap();
    let files = pieces(&mem);
    assert!(
        TapeReader::open(&files[0][..])
            .unwrap()
            .verify(0)
            .unwrap()
            .holds
    );
    for (v, s) in values.iter().zip(&seals) {
        let (bytes, read) = read_from(&files, s.id).unwrap();
        assert_eq!(&bytes, v);
        assert_eq!(read, *s);
        assert_eq!(read.hash, *blake3::hash(v).as_bytes());
        assert_eq!(read.len, v.len() as u64);
    }
}

/// A part's chunk is an events chunk of one instant, its bytes first, its flags marking the first part and the seal.
#[test]
fn parts_are_events_chunks_of_one_item() {
    let defs = defs();
    let mut w = writer(&defs, 40);
    let mut parts = Vec::new();
    w.begin(9).unwrap();
    w.write::<ww_tape::bytes_track::BytesError>(&value(5, 1500), |p| {
        parts.push((
            p.head,
            p.columns()
                .iter()
                .map(|(s, b)| (s.kind, b.to_vec()))
                .collect::<Vec<_>>(),
        ));
        Ok(())
    })
    .unwrap();
    let seal = w
        .finish::<ww_tape::bytes_track::BytesError>(|p| {
            parts.push((
                p.head,
                p.columns()
                    .iter()
                    .map(|(s, b)| (s.kind, b.to_vec()))
                    .collect(),
            ));
            Ok(())
        })
        .unwrap();
    assert_eq!(seal.parts, 3);
    use ww_tape::ColumnKind::{Flags, PayloadLengths, Payloads, Starts};
    let kinds: Vec<Vec<_>> = parts
        .iter()
        .map(|(_, c)| c.iter().map(|(k, _)| *k).collect())
        .collect();
    assert_eq!(
        kinds,
        [
            vec![Payloads, PayloadLengths, Starts, Flags],
            vec![Payloads, PayloadLengths, Starts],
            vec![Payloads, PayloadLengths, Starts, Flags],
        ],
        "a middle part's flags are 0: no column"
    );
    for (k, (h, c)) in parts.iter().enumerate() {
        assert_eq!(h.first_item, ItemNum::new(40 + k as u64));
        assert_eq!((h.count, h.first_tick, h.last_tick), (1, 9, 9));
        assert_eq!(h.track_number, BYTES);
        let _ = c;
    }
    assert_eq!(parts[0].1[0].1.len(), PART);
    assert_eq!(parts[1].1[0].1.len(), 500);
    let sealed = &parts[2].1[0].1;
    assert_eq!(sealed.len(), 48);
    assert_eq!(&sealed[..8], &40u64.to_le_bytes());
    assert_eq!(&sealed[8..16], &1500u64.to_le_bytes());
    assert_eq!(&sealed[16..], blake3::hash(&value(5, 1500)).as_bytes());
    assert_eq!(PART_FIRST | PART_SEAL, 3);
}

/// A value given up part way, and a value whose seal never came (the track ends), read as absent, with nothing
/// handed to the sink; the value after the torn one reads whole.
#[test]
fn a_torn_value_reads_as_absent() {
    let defs = defs();
    let (mem, mut c) = chain(options(PieceTapes::One, Integrity::Full, 1 << 20));
    let mut w = writer(&defs, 0);
    let torn = w.begin(1).unwrap();
    w.write(&value(1, 2500), |p: BytesPart<'_>| {
        c.write_chunk(&defs, &p.head, p.columns())
            .map(drop)
            .map_err(WriteValueError::Write)
    })
    .unwrap();
    w.abandon();
    let whole = put(&mut c, &defs, &mut w, 2, &value(2, 1200));
    let tail = w.begin(3).unwrap();
    w.write(&value(3, 1500), |p: BytesPart<'_>| {
        c.write_chunk(&defs, &p.head, p.columns())
            .map(drop)
            .map_err(WriteValueError::Write)
    })
    .unwrap();
    c.close().unwrap();
    let files = pieces(&mem);
    let mut handed = 0usize;
    let readers = [TapeReader::open(&files[0][..]).unwrap()];
    let r = read_value(&[&readers[0]], bytes_id(), torn, |b| {
        handed += b.len();
        Ok::<(), ()>(())
    });
    assert!(
        matches!(
            r,
            Err(ValueError::Absent {
                why: Absence::Torn { parts: 2 },
                ..
            })
        ),
        "{r:?}"
    );
    assert_eq!(handed, 0, "absent, never partial");
    assert_eq!(read_from(&files, whole.id).unwrap().0, value(2, 1200));
    assert!(matches!(
        read_from(&files, tail),
        Err(ValueError::Absent {
            why: Absence::Torn { parts: 1 },
            ..
        })
    ));
    assert!(matches!(
        read_from(&files, ValueId(ItemNum::new(1))),
        Err(ValueError::Absent {
            why: Absence::NotFirst,
            ..
        })
    ));
    assert!(matches!(
        read_from(&files, ValueId(ItemNum::new(99))),
        Err(ValueError::Absent {
            why: Absence::NotFound,
            ..
        })
    ));
}

/// A crash at any block of a value: the piece cut there reads the value as absent until the seal is in, whole after.
#[test]
fn a_value_cut_by_a_crash_at_any_block_is_absent_until_sealed() {
    let defs = defs();
    let (mem, mut c) = chain(options(PieceTapes::One, Integrity::Full, 1 << 20));
    let mut w = writer(&defs, 0);
    let v = value(7, 4200);
    let seal = put(&mut c, &defs, &mut w, 5, &v);
    c.close().unwrap();
    let whole = pieces(&mem).remove(0);
    let r = TapeReader::open(&whole[..]).unwrap();
    let ends: Vec<usize> = r
        .entries(0)
        .unwrap()
        .iter()
        .map(|e| (e.offset + u64::from(e.len)) as usize)
        .collect();
    let seal_end = r
        .track_entries(0, bytes_id())
        .unwrap()
        .iter()
        .map(|e| (e.offset + u64::from(e.len)) as usize)
        .max()
        .unwrap();
    let mut absent = 0;
    for end in ends {
        let cut = whole[..end].to_vec();
        let got = read_from(&[cut], seal.id);
        if end < seal_end {
            assert!(
                matches!(
                    got,
                    Err(ValueError::Absent {
                        why: Absence::Torn { .. } | Absence::NotFound,
                        ..
                    })
                ),
                "cut at {end}: {got:?}"
            );
            absent += 1;
        } else {
            assert_eq!(got.unwrap().0, v, "cut at {end}");
        }
    }
    assert!(absent >= 5, "a cut after each part but the seal");
}

/// A changed byte in a part's bytes fails its CRC: the error names that part's chunk and where its bytes start in
/// the value. Under Integrity::None no CRC catches it: the value's hash does, and names the value.
#[test]
fn a_changed_byte_names_the_chunk() {
    for integrity in [Integrity::Full, Integrity::CrcOnly, Integrity::None] {
        let defs = defs();
        let (mem, mut c) = chain(options(PieceTapes::One, integrity, 1 << 20));
        let mut w = writer(&defs, 0);
        let v = value(9, 3500);
        let seal = put(&mut c, &defs, &mut w, 1, &v);
        c.close().unwrap();
        let mut file = pieces(&mem).remove(0);
        let r = TapeReader::open(&file[..]).unwrap();
        let third = r
            .track_entries(0, bytes_id())
            .unwrap()
            .into_iter()
            .find(|e| e.first_item == ItemNum::new(2))
            .unwrap();
        let start = r.pieces()[0].start as usize + third.offset as usize;
        drop(r);
        // the part's bytes are its first column, at its payload offset
        let payload_at =
            u32::from_le_bytes(file[start + 32..start + 36].try_into().unwrap()) as usize;
        file[start + payload_at + 17] ^= 0x40;
        let got = read_from(&[file], seal.id);
        match integrity {
            Integrity::None => assert!(
                matches!(
                    got,
                    Err(ValueError::Mismatch {
                        len: 3500,
                        stated_len: 3500,
                        hash_matches: false,
                        ..
                    })
                ),
                "{got:?}"
            ),
            _ => match got {
                Err(ValueError::Damaged {
                    at,
                    cause: PartDamage::Read(_),
                }) => {
                    assert_eq!(at.part, ItemNum::new(2));
                    assert_eq!(at.offset, third.offset);
                    assert_eq!(at.value_offset, Some(2 * PART as u64));
                }
                other => panic!("{integrity:?}: {other:?}"),
            },
        }
    }
}

/// Cuts between parts put a value's parts into several pieces: in one tape, and in a piece file each.
#[test]
fn a_value_spans_cuts() {
    for tapes in [PieceTapes::One, PieceTapes::Each] {
        let defs = defs();
        let (mem, mut c) = chain(options(tapes, Integrity::Full, 2));
        let mut w = writer(&defs, 0);
        let v = value(11, 6500);
        let seal = put(&mut c, &defs, &mut w, 1, &v);
        let after = put(&mut c, &defs, &mut w, 2, &value(12, 900));
        c.close().unwrap();
        let files = pieces(&mem);
        let pieces_n: usize = files
            .iter()
            .map(|f| TapeReader::open(&f[..]).unwrap().pieces().len())
            .sum();
        assert!(pieces_n >= 4, "{tapes:?}: {pieces_n} pieces");
        assert_eq!(read_from(&files, seal.id).unwrap().0, v, "{tapes:?}");
        assert_eq!(read_from(&files, after.id).unwrap().0, value(12, 900));
    }
}

/// A value whose middle parts were in a piece removed on purpose, or in a piece file lost, reads as absent naming the
/// missing parts; a value wholly in the pieces kept still reads.
#[test]
fn a_value_over_a_removed_or_lost_piece_reads_as_a_gap() {
    let defs = defs();
    let (mem, mut c) = chain(options(PieceTapes::Each, Integrity::Full, 2));
    let mut w = writer(&defs, 0);
    let v = value(13, 5500);
    let seal = put(&mut c, &defs, &mut w, 1, &v);
    let files = pieces(&mem);
    assert!(files.len() >= 3);
    // the lost piece: the second file missing from those given
    let lost: Vec<Vec<u8>> = files
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, f)| f.clone())
        .collect();
    assert!(matches!(
        read_from(&lost, seal.id),
        Err(ValueError::Absent { why: Absence::Gap { missing }, .. }) if missing == (2..4)
    ));
    c.remove(&[1]).unwrap();
    let later = put(&mut c, &defs, &mut w, 2, &value(14, 300));
    c.close().unwrap();
    let kept = pieces(&mem);
    match read_from(&kept, seal.id) {
        Err(ValueError::Absent {
            why: Absence::Gap { missing },
            ..
        }) => assert_eq!(missing, 2..4),
        other => panic!("{other:?}"),
    }
    assert_eq!(read_from(&kept, later.id).unwrap().0, value(14, 300));
}

/// A part's bytes coded in lz4 read back the same.
#[test]
fn lz4_parts_read_back() {
    let defs = defs();
    let (mem, mut c) = chain(options(PieceTapes::One, Integrity::Full, 1 << 20));
    let mut w = BytesWriter::new(
        &defs,
        BYTES,
        ItemNum::new(0),
        BytesOptions {
            part_len: PART,
            codec: PayloadCodec::LZ4,
        },
    )
    .unwrap();
    let v: Vec<u8> = b"abcd".iter().copied().cycle().take(5000).collect();
    let seal = put(&mut c, &defs, &mut w, 1, &v);
    c.close().unwrap();
    let files = pieces(&mem);
    let r = TapeReader::open(&files[0][..]).unwrap();
    let parts = r.track_entries(0, bytes_id()).unwrap();
    assert_eq!(parts.len(), 6);
    let data: u64 = parts[..5].iter().map(|e| u64::from(e.len)).sum();
    assert!(data < 5000, "{data} bytes of data parts for 5000 bytes");
    assert_eq!(read_from(&files, seal.id).unwrap().0, v);
}

/// A bytes track takes the plain policy and no relations, no record, and only chunks shaped as parts.
#[test]
fn a_bytes_track_takes_only_parts() {
    let Describe::Track(t) = bytes_def(BYTES.get()) else {
        unreachable!()
    };
    let mut d = base_defs().clone();
    let state = Describe::Track(TrackDef {
        repeat: TrackRepeat::STATE,
        ..t.clone()
    });
    assert!(matches!(
        hold(&mut d, &[state]),
        Err(DescribeError::BytesTrack { .. })
    ));
    assert!(
        BytesWriter::new(
            &base_defs().clone(),
            TrackNum::new(3),
            ItemNum::new(0),
            BytesOptions::default()
        )
        .is_err()
    );
    let defs = defs();
    let (_, mut c) = chain(options(PieceTapes::One, Integrity::Full, 1 << 20));
    // an events chunk of two items on the bytes track
    let times = ww_tape::times::EncodedTimes::encode(&[1, 2], None, None).unwrap();
    let payloads = ww_tape::payloads::EncodedPayloads::encode([&b"a"[..], b"b"]).unwrap();
    let head = times.head.chunk_head(bytes_id(), BYTES, ItemNum::new(0));
    let columns: Vec<_> = payloads
        .columns()
        .into_iter()
        .chain(times.columns())
        .collect();
    assert!(matches!(
        c.write_chunk(&defs, &head, &columns),
        Err(WriteError::Describe(DescribeError::BytesPart { .. }))
    ));
    // a record on the bytes track
    let mut buf = [0u8; 256];
    let clock = ww_tape::ClockNum::new(CLOCK);
    let mut b = ww_tape::BlockWriter::new(
        &mut buf,
        ww_tape::BlockKind::Records,
        crate::id(0),
        &[clock],
    )
    .unwrap();
    b.push(BYTES, ww_tape::ClockTime::new(clock, 1), &[b"x"])
        .unwrap();
    let len = b.finish();
    assert!(matches!(
        c.write_block(&defs, &buf[..len]),
        Err(WriteError::Describe(DescribeError::WrongKind { .. }))
    ));
}

/// A piece holding a value of three parts, a value of no bytes and a torn value, pinned byte for byte.
#[test]
fn bytes_golden() {
    let defs = defs();
    let (mem, mut c) = chain(options(PieceTapes::One, Integrity::Full, 1 << 20));
    let mut w = BytesWriter::new(
        &defs,
        BYTES,
        ItemNum::new(0),
        BytesOptions {
            part_len: 10,
            ..Default::default()
        },
    )
    .unwrap();
    let three = put(&mut c, &defs, &mut w, 1, b"twenty-five bytes of data");
    assert_eq!(three.parts, 4);
    let empty = put(&mut c, &defs, &mut w, 2, b"");
    assert_eq!(empty.parts, 1);
    w.begin(3).unwrap();
    w.write(b"torn value, no seal", |p: BytesPart<'_>| {
        c.write_chunk(&defs, &p.head, p.columns())
            .map(drop)
            .map_err(WriteValueError::Write)
    })
    .unwrap();
    w.abandon();
    c.close().unwrap();
    let file = pieces(&mem).remove(0);
    assert_eq!(
        read_from(&[file.clone()], three.id).unwrap().0,
        b"twenty-five bytes of data"
    );
    assert_eq!(read_from(&[file.clone()], empty.id).unwrap().0, b"");
    crate::quiet::golden(&file, "v1-bytes.hex");
}

/// Through a mux: a value's parts go through the queue as chunks, a cut falls inside the value, another handle's
/// events interleave, and the value reads back whole across the pieces.
#[test]
fn a_value_through_the_mux() {
    let defs = defs();
    let (mem, c) = chain(options(PieceTapes::Each, Integrity::Full, 3));
    let mux = ww_tape_mux::TapeMux::start(c, defs, ww_tape_mux::MuxOptions::default()).unwrap();
    let mut h = mux
        .bytes(
            BYTES,
            ItemNum::new(0),
            BytesOptions {
                part_len: PART,
                ..Default::default()
            },
        )
        .unwrap();
    let v = value(21, 7300);
    let id = h.begin(4).unwrap();
    for (k, piece) in v.chunks(1100).enumerate() {
        h.write(piece).unwrap();
        if k == 3 {
            h.flush().unwrap();
            mux.cut().unwrap();
        }
    }
    let seal = h.finish().unwrap();
    assert_eq!(seal.id, id);
    assert_eq!(seal.parts, 9);
    assert_eq!(h.next_part(), ItemNum::new(9));
    let refused = mux.bytes(TrackNum::new(3), ItemNum::new(0), BytesOptions::default());
    assert!(matches!(refused, Err(ww_tape_mux::MuxError::Bytes(_))));
    drop(h);
    mux.close().unwrap();
    let files = pieces(&mem);
    assert!(files.len() >= 3, "{} pieces", files.len());
    let (bytes, read) = read_from(&files, id).unwrap();
    assert_eq!(bytes, v);
    assert_eq!(read, seal);
}
