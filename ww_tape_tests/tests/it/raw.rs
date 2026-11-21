//! Tracks whose items are not a WireWeaver stream's values ([Items], FMT-12): plain bytes, a media type, a format
//! the user names with a [Schema] the tape carries, numbers of a described element. The items are stored as given,
//! in records and in events chunks, and the definitions say what they are.

use shrink_wrap::prelude::*;
use ww_tape::payloads::{EncodedPayloads, Payloads};
use ww_tape::state::DefinitionKind;
use ww_tape::times::EncodedTimes;
use ww_tape::{
    Block, BlockId, BlockKind, Definitions, Describe, DescribeError, Dimension, ENCODING_NAME_MAX,
    ElementDesc, EventsDef, FileBlocks, Foreign, FullVersion, Items, MEDIA_TYPE_MAX, MemSink,
    MemTapes, OwnerId, PieceChain, RecordRef, Recovery, RelationDef, Schema, Segment, SessionTag,
    Slot, SyncPolicy, TapeReader, TapeWriter, TrackDef, TrackKind, TrackRepeat, TypeRef, Version,
    track_key,
};

use crate::definitions::{CLOCK, begin, clock, on_tracks, options};

/// The schema of [named]'s format: a protobuf message.
const PROTO: &[u8] = b"syntax = \"proto3\"; message Frame { uint32 id = 1; bytes data = 2; }";

/// BLAKE3 of [PROTO].
fn proto_hash() -> &'static [u8; 32] {
    static H: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    H.get_or_init(|| *blake3::hash(PROTO).as_bytes())
}

/// The schema of [PROTO] as a definition.
fn proto_schema() -> Describe<'static> {
    Describe::Schema(Schema {
        hash: RefVec::Slice {
            slice: proto_hash(),
        },
        media_type: "text/x-protobuf",
        bytes: RefVec::Slice { slice: PROTO },
    })
}

/// A format named by the user, version 3, described by [PROTO].
fn named() -> Foreign<'static> {
    Foreign {
        name: "com.example.frame",
        media_type: "",
        version: "3",
        schema: RefVec::Slice {
            slice: proto_hash(),
        },
    }
}

/// A format named by a media type.
fn media(m: &str) -> Foreign<'_> {
    Foreign {
        name: "",
        media_type: m,
        version: "",
        schema: RefVec::Slice { slice: &[] },
    }
}

/// 8-bit IQ as an RTL-SDR sends it: two unsigned bytes, zero at 127.5.
fn iq_u8() -> ElementDesc {
    ElementDesc {
        bits: 8,
        exponent_bits: 0,
        signed: false,
        msb_first: false,
        parts: UNib32(2),
        zero: ww_tape::Ratio64::new(255, 2),
    }
}

/// An events track of instants: its items are what `items` says.
fn raw_track(number: u32, items: Items<'static>) -> TrackDef<'static> {
    TrackDef {
        number: UNib32(number),
        key: track_key(0x77, &[Segment::Index(number)]),
        clock: UNib32(CLOCK),
        dimensions: RefVec::Slice { slice: &[] },
        items,
        label: "raw",
        repeat: TrackRepeat::Stream,
        kind: TrackKind::Events(EventsDef::INSTANTS),
        relations: RefVec::Slice { slice: &[] },
        owner: [0; 32],
    }
}

/// Items of every length a group of lengths may hold: none, a few, and a frame.
fn items() -> Vec<Vec<u8>> {
    vec![
        vec![],
        vec![0xff; 3],
        b"\x0a\x08\x96\x01".to_vec(),
        (0..1500u32).map(|i| (i * 7 % 251) as u8).collect(),
    ]
}

/// The clock, [proto_schema] and `def`, held.
fn definitions(def: &TrackDef<'static>) -> Definitions {
    let mut defs = Definitions::new();
    defs.define_all(&[clock(CLOCK), proto_schema(), Describe::Track(def.clone())])
        .unwrap();
    defs
}

/// A tape of `def`'s [items] as a records block and as an events chunk; the records block's length.
fn write(def: &TrackDef<'static>) -> (Vec<u8>, usize) {
    let defs = definitions(def);
    let mut w = TapeWriter::create_on(MemSink::new(), SyncPolicy::None);
    let tape = w.sink().tape();
    let items = items();
    let records: Vec<(u32, i64, &[u8])> = items
        .iter()
        .enumerate()
        .map(|(i, b)| (def.number.0, i as i64, b.as_slice()))
        .collect();
    let block = on_tracks(w.next_id(), &records);
    w.write_block(&defs, &block).unwrap();
    let starts: Vec<i64> = (10..10 + items.len() as i64).collect();
    let times = EncodedTimes::encode(&starts, None, None).unwrap();
    let payloads = EncodedPayloads::encode(items.iter().map(Vec::as_slice)).unwrap();
    let head = times.head.chunk_head(def.id(), def.number.0, 0);
    let columns: Vec<_> = times.columns().chain(payloads.columns()).collect();
    w.write_chunk(&defs, &head, &columns).unwrap();
    w.close().unwrap();
    (tape.to_vec().unwrap(), block.len())
}

/// The definition of track `number` in force at the end of piece 0 of `bytes`.
fn track_read(bytes: &[u8], number: u32) -> TrackDef<'_> {
    let r = TapeReader::open(bytes).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    let item = at.items[&Slot::Track { number }].item.clone();
    let item: &'static [u8] = Box::leak(item.into_boxed_slice());
    let Ok(Describe::Track(t)) = Describe::from_ww_bytes(item) else {
        panic!("track {number} reads");
    };
    t
}

/// A track of plain bytes keeps each item's bytes as given, in records and in an events chunk: a record adds only
/// its head, and the definition read back says plain bytes.
#[test]
fn plain_bytes_are_stored_as_given() {
    let def = raw_track(1, Items::RawBytes);
    let (bytes, block_len) = write(&def);
    let (mut in_records, mut in_chunk) = (vec![], vec![]);
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records && !b.is_repeat() {
            for r in b.records() {
                let r = r.unwrap();
                if r.track == 1 {
                    in_records.push(r.item.to_vec());
                }
            }
        }
        if let Some(c) = b.chunk() {
            let p = Payloads::read(&c).unwrap().unwrap();
            in_chunk = p.iter().map(|i| i.bytes.to_vec()).collect();
        }
    }
    assert_eq!(in_records, items(), "records keep the bytes");
    assert_eq!(in_chunk, items(), "the chunk keeps the bytes");
    // the block's header (32), clock table (2), CRC (4), and per record its head, time delta and length
    let payload: usize = items().iter().map(Vec::len).sum();
    assert_eq!(block_len, payload + 32 + 2 + 4 + 4 * 3 + 1);

    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    let read = track_read(&bytes, 1);
    assert_eq!(read.items, Items::RawBytes);
    assert_eq!(read, def);
}

/// A track of a format named by a media type reads back with it.
#[test]
fn a_media_type_reads_back() {
    let def = raw_track(2, Items::Foreign(media("application/vnd.tcpdump.pcap")));
    let (bytes, _) = write(&def);
    assert_eq!(track_read(&bytes, 2), def);
}

/// A format the user names carries its schema: the writer puts the schema before the track's definition, a reader
/// finds it by the hash the encoding names, and a new piece carries it again with the track.
#[test]
fn a_named_format_carries_its_schema() {
    let def = raw_track(3, Items::Foreign(named()));
    let defs = definitions(&def);
    assert_eq!(
        defs.needs(3)
            .iter()
            .map(|s| match s {
                Slot::Own { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [None, Some(DefinitionKind::Schema), None],
        "the clock, the schema, then the track"
    );
    assert_eq!(defs.schema(proto_hash()).unwrap().1, {
        let mut buf = vec![0u8; 4096];
        proto_schema().to_ww_bytes(&mut buf).unwrap().to_vec()
    });

    let (bytes, _) = write(&def);
    let read = track_read(&bytes, 3);
    assert_eq!(read.items, Items::Foreign(named()));
    let r = TapeReader::open(&bytes[..]).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    let schema = at
        .items
        .iter()
        .find(|(s, _)| {
            matches!(
                s,
                Slot::Own {
                    kind: DefinitionKind::Schema,
                    ..
                }
            )
        })
        .map(|(_, i)| i.item.clone())
        .expect("the schema is in the piece");
    let Ok(Describe::Schema(s)) = Describe::from_ww_bytes(&schema) else {
        panic!("a schema");
    };
    assert_eq!(s.bytes.as_slice(), PROTO);
    assert_eq!(s.hash.as_slice(), proto_hash());

    // a cut carries the schema with the quiet track
    let mem = MemTapes::new();
    let mut chain = PieceChain::create_on(
        mem.clone(),
        OwnerId([0x52; 32]),
        0,
        &[],
        ww_tape::ChainOptions {
            writer: ww_tape::WriterOptions {
                sync: SyncPolicy::None,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    let block = on_tracks(chain.writer().unwrap().next_id(), &[(3, 0, b"frame")]);
    chain.write_block(&defs, &block).unwrap();
    chain.cut_now(&defs).unwrap();
    let pieces = chain.close().unwrap();
    let last = mem.get(&pieces[1].tape).unwrap().unwrap().to_vec().unwrap();
    let r = TapeReader::open(&last[..]).unwrap();
    let at = r.definitions_at(0, u64::MAX).unwrap();
    assert!(at.items.contains_key(&Slot::Track { number: 3 }));
    assert!(
        at.items.keys().any(|s| matches!(
            s,
            Slot::Own {
                kind: DefinitionKind::Schema,
                ..
            }
        )),
        "the second piece carries the schema"
    );
}

/// Events of 8-bit IQ: each item is one element, its two bytes; the definition reads back with the description.
#[test]
fn numbers_of_a_described_element_read_back() {
    let def = raw_track(
        6,
        Items::Numbers {
            element: iq_u8(),
            unit: None,
        },
    );
    let (bytes, _) = write(&def);
    assert_eq!(track_read(&bytes, 6), def);
    assert_eq!(iq_u8().container_bits(), 8);
    assert_eq!(iq_u8().element(), None, "no predefined element is 8-bit IQ");
    assert_eq!(
        ElementDesc::of(ww_tape::Element::ComplexI16).element(),
        Some(ww_tape::Element::ComplexI16)
    );
}

/// Items that don't fit are refused: a foreign format with neither a name nor a media type, a bad media type, a name
/// or version too long or not printable, a schema hash of another length; numbers of a bad element; a kind this
/// crate doesn't know. So is a schema whose hash isn't BLAKE3 of its bytes, or whose media type isn't valid.
#[test]
fn items_or_a_schema_that_do_not_fit_are_refused() {
    let refused = |items: Items<'_>| {
        let t = TrackDef {
            items,
            ..raw_track(4, Items::RawBytes)
        };
        let mut defs = Definitions::new();
        defs.define_all(&[clock(CLOCK), Describe::Track(t)])
    };
    let bad = Err(DescribeError::Items { track: 4 });
    let long_media = format!("application/{}", "x".repeat(MEDIA_TYPE_MAX));
    for m in [
        "",
        "pcap",
        "/pcap",
        "application/",
        "text/plain\n",
        "text/plaïn",
        long_media.as_str(),
    ] {
        assert_eq!(refused(Items::Foreign(media(m))), bad, "{m:?}");
    }
    let long_name = "n".repeat(ENCODING_NAME_MAX + 1);
    let short_hash = [7u8; 16];
    for f in [
        Foreign {
            name: &long_name,
            ..media("")
        },
        Foreign {
            name: "tab\there",
            ..media("")
        },
        Foreign {
            version: "3",
            ..media("")
        },
        Foreign {
            name: "x",
            version: &long_name,
            ..media("")
        },
        Foreign {
            name: "x",
            schema: RefVec::Slice { slice: &short_hash },
            ..media("")
        },
    ] {
        assert_eq!(refused(Items::Foreign(f.clone())), bad, "{f:?}");
    }
    for element in [
        ElementDesc { bits: 0, ..iq_u8() },
        ElementDesc {
            bits: 65,
            ..iq_u8()
        },
        ElementDesc {
            parts: UNib32(17),
            ..iq_u8()
        },
        ElementDesc {
            parts: UNib32(0),
            ..iq_u8()
        },
        // a float of 8 bits with 7 exponent bits has no mantissa
        ElementDesc {
            exponent_bits: 7,
            signed: true,
            ..iq_u8()
        },
        ElementDesc {
            bits: 16,
            exponent_bits: 8,
            signed: false,
            ..iq_u8()
        },
    ] {
        assert_eq!(
            refused(Items::Numbers {
                element,
                unit: None
            }),
            bad,
            "{element:?}"
        );
    }
    assert_eq!(refused(Items::Unknown(9)), bad);

    let longest = format!("a/{}", "x".repeat(MEDIA_TYPE_MAX - 2));
    let name = "n".repeat(ENCODING_NAME_MAX);
    let bfloat16 = ElementDesc {
        bits: 16,
        exponent_bits: 8,
        signed: true,
        msb_first: false,
        parts: UNib32(1),
        zero: None,
    };
    for items in [
        Items::RawBytes,
        Items::Foreign(media(&longest)),
        Items::Foreign(media("application/x-protobuf; messageType=pkg.Msg")),
        Items::Foreign(Foreign {
            name: &name,
            ..media("")
        }),
        Items::Foreign(named()),
        Items::Numbers {
            element: iq_u8(),
            unit: None,
        },
        Items::Numbers {
            element: bfloat16,
            unit: None,
        },
        Items::ShrinkWrap(TypeRef {
            bundle: RefVec::Slice { slice: &[] },
            krate: FullVersion::new("drive_api", Version::new(0, 1, 0)),
            path: "Frame",
        }),
    ] {
        assert_eq!(refused(items.clone()), Ok(()), "{items:?}");
    }

    let wrong = [0u8; 32];
    for schema in [
        Schema {
            hash: RefVec::Slice { slice: &wrong },
            media_type: "text/x-protobuf",
            bytes: RefVec::Slice { slice: PROTO },
        },
        Schema {
            hash: RefVec::Slice {
                slice: proto_hash(),
            },
            media_type: "protobuf",
            bytes: RefVec::Slice { slice: PROTO },
        },
    ] {
        let mut defs = Definitions::new();
        assert_eq!(
            defs.define(&Describe::Schema(schema)),
            Err(DescribeError::Schema)
        );
    }
}

/// A track whose items are of a kind a newer ww_tape added reads with `Items::Unknown` and the rest of its definition
/// intact. A relay holds such an events track from a producer's block and stores its items as given; a program's own
/// definition of one is refused.
#[test]
fn an_items_variant_this_crate_does_not_know_reads_as_unknown() {
    #[derive_shrink_wrap(borrowed, derive(Debug), ww_repr = u4)]
    enum NewerItems<'i> {
        WireWeaver,
        ShrinkWrap,
        RawBytes,
        Numbers,
        Foreign,
        Packed { codec: UNib32, name: &'i str },
    }
    #[derive_shrink_wrap(borrowed, derive(Debug))]
    struct NewerTrackDef<'i> {
        number: UNib32,
        key: u64,
        clock: UNib32,
        dimensions: RefVec<'i, Dimension<'i>>,
        items: NewerItems<'i>,
        label: &'i str,
        repeat: TrackRepeat,
        kind: TrackKind<'i>,
        #[default = [0; 32]]
        owner: [u8; 32],
        #[default = Default::default()]
        relations: RefVec<'i, RelationDef<'i>>,
    }
    #[derive_shrink_wrap(borrowed, derive(Debug), ww_repr = u4)]
    enum NewerDescribe<'i> {
        Producer,
        Track(NewerTrackDef<'i>),
    }
    let today = raw_track(7, Items::RawBytes);
    let newer = NewerDescribe::Track(NewerTrackDef {
        number: today.number,
        key: today.key,
        clock: today.clock,
        dimensions: RefVec::Slice { slice: &[] },
        items: NewerItems::Packed {
            codec: UNib32(3),
            name: "iq8",
        },
        label: today.label,
        repeat: today.repeat,
        kind: today.kind.clone(),
        owner: today.owner,
        relations: RefVec::Slice { slice: &[] },
    });
    let mut buf = vec![0u8; 4096];
    let newer = newer.to_ww_bytes(&mut buf).unwrap().to_vec();
    let Ok(Describe::Track(read)) = Describe::from_ww_bytes(&newer) else {
        panic!("a track");
    };
    assert_eq!(
        read,
        TrackDef {
            items: Items::Unknown(5),
            ..today.clone()
        },
        "the rest of the definition is intact"
    );
    let mut defs = Definitions::new();
    assert_eq!(
        defs.define_all(&[clock(CLOCK), Describe::Track(read.clone())]),
        Err(DescribeError::Items { track: 7 }),
        "a program builds no unknown variant"
    );

    // a relay holds the producer's definition and puts its bytes before the track's data, stored as given
    let mut w = begin(options(u64::MAX));
    let tape = w.sink().tape();
    let producer = BlockId::new(SessionTag::random(), 7);
    let mut out = Vec::new();
    ww_tape::open::write_records(
        &[RecordRef {
            track: 0,
            at: None,
            item: &newer,
        }],
        4096,
        0,
        || producer,
        |b| {
            out.push(b.to_vec());
            Ok::<(), DescribeError>(())
        },
    )
    .unwrap();
    assert_eq!(
        w.defs.take_block(&Block::read(&out[0]).unwrap()).unwrap(),
        1
    );
    let data = on_tracks(w.next_id(), &[(7, 5, b"\x01\x02")]);
    w.write_block(&data).unwrap();
    w.close_piece().unwrap();
    let bytes = tape.to_vec().unwrap();
    let r = TapeReader::open(&bytes[..]).unwrap();
    assert!(r.verify(0).unwrap().holds);
    assert_eq!(track_read(&bytes, 7).items, Items::Unknown(5));
    let mut stored = vec![];
    let mut rd = FileBlocks::with_recovery(&bytes[..], Recovery::Prefix);
    while let Some(Ok(b)) = rd.next_block() {
        if b.kind == BlockKind::Records && !b.is_repeat() {
            for r in b.records() {
                let r = r.unwrap();
                if r.track == 7 {
                    stored.push(r.item.to_vec());
                }
            }
        }
    }
    assert_eq!(stored, [b"\x01\x02".to_vec()]);
}

/// What a track definition takes for each kind of items, and a schema over its bytes.
#[test]
fn items_cost_their_fields() {
    let len = |d: &Describe<'_>| {
        let mut buf = vec![0u8; 4096];
        d.to_ww_bytes(&mut buf).unwrap().len()
    };
    let track = |items: Items<'static>| len(&Describe::Track(raw_track(1, items)));
    let stream = Items::WireWeaver {
        api: FullVersion::new("drive_api", Version::new(0, 1, 0)),
        bundle: RefVec::Slice { slice: &[] },
        path: RefVec::Slice {
            slice: &[UNib32(3)],
        },
        keys: RefVec::Slice { slice: &[] },
        item: None,
    };
    assert_eq!(track(stream), 73, "a WireWeaver stream");
    assert_eq!(track(Items::RawBytes), 56, "plain bytes");
    assert_eq!(
        track(Items::Foreign(media("application/vnd.tcpdump.pcap"))),
        90,
        "a media type"
    );
    assert_eq!(
        track(Items::Foreign(named())),
        112,
        "a name, a version and a schema hash"
    );
    assert_eq!(
        track(Items::Numbers {
            element: iq_u8(),
            unit: None
        }),
        65,
        "8-bit IQ"
    );
    assert_eq!(
        len(&proto_schema()) - PROTO.len(),
        54,
        "a schema over its bytes"
    );
}
