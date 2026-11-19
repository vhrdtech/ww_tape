# Anatomy of a file

This page follows one finished file from its start to its end. It then explains how a reader reads by position, and
why the same input gives the same bytes.

## From piece head to tail

A complete file is a **piece**. It starts with a piece head, then the layout and the producer.

The data follows, with the definitions it needs close to it and index blocks between it. Three blocks close the
piece.

[Sessions, pieces and chains](sessions_and_pieces.md#one-file-start-to-end) shows one piece in a diagram.

### Checkpoints while the file grows

While the writer writes a file, nothing marks its end yet. The writer places a **checkpoint** index block about every
4 MiB.

Each checkpoint lists every block placed since the previous checkpoint, and points back at it. A reader of an
unfinished file finds the newest checkpoint from the end, and follows the checkpoints back.

The reader then scans only the stretch after the newest checkpoint.

### The end of a finished piece

At the end, the writer places a **final index**, a **finish** block and a **tail**. The final index lists every block of
the piece, sorted by track, with a directory of tracks in its head.

A reader of one track out of thousands reads that directory and that track's list. The finish block holds the piece's
content hash and per-track totals.

The finish block also holds the offsets of the final index and of the newest checkpoint. The content hash is a hash
over every byte before the finish, made with BLAKE3 (a cryptographic hash function).

The content hash takes in each chunk's payload by the payload's own hash. The per-track totals are each track's items,
ticks and running hash.

The tail is the last 164 bytes of the piece. It gives the piece's length and where its finish block is.

A reader of a finished file starts at the end, with the tail. All offsets count from the start of the piece, and an
index therefore stays valid wherever the piece sits.

The layouts are in the module docs of [index](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/index/mod.rs)
and [piece](https://github.com/vhrdtech/ww_tape/blob/main/ww_tape/src/piece/mod.rs).

A file that a writer started without a piece head, a plain stream of blocks, is still a valid tape. It is one piece
with no ids.

[Pieces and chains](index.md#pieces-and-chains) explains how pieces form a chain, and how a chain fits into one file.

## Reading by position

`TapeReader` reads a tape through any `ByteSource`, a source of bytes that reads at an offset. Three examples are:

- a slice in memory;
- a file, which `ww_tape_fs::FileSource` opens as one;
- an HTTP resource fetched by range requests.

The reader needs no file system and no threads, and it therefore runs in a browser. It opens a finished piece with a
few small reads: the tail, the finish and the final index's directory.

After that, it reads only the blocks a query needs.

## Same input, same bytes

A writer given the same input writes the same bytes, for these reasons:

- Blocks hold no wall time.
- Definitions go in the order the application makes them.
- Puts and copies follow from the bytes before them.
- The writer cuts blocks by size rather than by time.
- Every codec is integer-only, or uses arithmetic that every platform rounds the same way.

A producer gives up this property for the blocks it seals on a timer, and for the pieces it cuts by age. The lz4 codec
is one more exception.

An lz4-coded payload column is byte-identical only for one version of the lz4 library, on targets of the same pointer
width and byte order. The exception does not affect decoding.

Deterministic output makes files comparable by hash. A test can pin a file's exact bytes, and two machines that
recorded the same feed can confirm that they hold the same data.
