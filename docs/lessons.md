---
level: basic
---

# Lessons from tape and flash

Tape drives and flash memory met the problems of a [[raw medium]] long before tapes did. They met data of an earlier
life behind the current end, erased space in the middle and writes retried after an error. This appendix names where
each rule
of [Where the data ends on a raw card](raw_media.md) comes from, and what the format takes from it. Each point names its
source, and the sources are listed at the end.

## Sessions and the end of the data

Linear Tape-Open (LTO) marks every data set with a write pass number, which grows whenever data is overwritten. A reader
thus tells current data from residue of an earlier pass [ECMA-319, §13.5.16]. Unsorted Block Images (UBI) on flash does
the same with an image sequence number on every eraseblock [UBI].

The [[session tag]] does this job in a tape. It is random or derived from a piece id, never a counter. A wiped card or a
device without stored state has no counter to continue.

LTO declares data recorded after the end-of-data mark inaccessible and invalid [ECMA-319, §13.4.3]. A tape has no
end-of-data block, since a crash leaves none. Blank space and the session tag mark the end instead, and nothing after it
is data.

## Erased bytes and gaps

Erased is not always zero. NOT-AND (NAND) flash erases to 0xFF [Micron TN-29-19]. A Secure Digital (SD) card reads 0 or
1 after an erase, by vendor [SD 9.10, §4.3.5.1]. An eMMC device reports which value it uses [JESD84-A441]. A tape's
blank space is therefore either value.

A blank unit is not always the end. A NAND writer that hits a failed program moves on and leaves erased pages behind.
The UBI file system (UBIFS) looks past such units when it scans for the end of the data [UBIFS].

A tape's
[[layout]] says how far a reader looks, in units that follow the media. SD allocation units reach 64 MB [SD 9.10,
§4.13.1]. The Open NAND Flash Interface (ONFI) defines NAND erase blocks and pages [ONFI 6.0, §3.1].

A NAND page is programmed once. Programming the same part of a page twice without an erase gives undefined contents
[ONFI 6.0, §6.7.1.25]. Micron's multi-level cell (MLC) example allows one program per page [Micron TN-29-19]. A
writer on raw NAND therefore pads the rest of the page at each sync, and readers take the padding as a [[gap]].

## Rewrites, indexes, compression and correction

LTO rewrites an interrupted data set in its entirety, and the later write counts [ECMA-319, §15.4]. The same block twice
in a tape is a [[duplicate]], and the reader takes the later copy.

Each Linear Tape File System (LTFS) index points to itself and to the one before it [LTFS]. An index whose self pointer
does not match where it was found is data, not an index. A tape's index
blocks carry the same two offsets, and a reader checks the self offset against where it found the block.

Streaming Lossless Data Compression (SLDC) switches to a scheme that copies bytes when compression would grow the data
[ECMA-321, §7.2]. Incompressible data then grows by about 0.05 % instead of 12.5 %. A tape's writer likewise keeps a
block's plain body when coding does not shrink it.

ONFI leaves error correction to the host controller, per page [ONFI 6.0, §6.7.1.26]. Correction thus belongs to the
medium, and a tape only detects what that correction missed, as [When bytes go bad](damage.md) explains.

## Sources

- [ECMA-319] Ecma International, ECMA-319: the Ultrium-1 tape format of LTO.
- [ECMA-321] Ecma International, ECMA-321: Streaming Lossless Data Compression (SLDC).
- [JESD84-A441] JESD84-A441: the eMMC standard, version 4.41.
- [LTFS] The LTFS Format Specification, version 2.5.1, §5.4.2 and §5.4.3.
- [Micron TN-29-19] Micron, technical note TN-29-19: NAND Flash 101.
- [ONFI 6.0] The ONFI specification, revision 6.0.
- [SD 9.10] The SD Physical Layer Simplified Specification, version 9.10.
- [UBI] Linux kernel, `drivers/mtd/ubi/ubi-media.h`.
- [UBIFS] Linux kernel, `fs/ubifs/scan.c` and `fs/ubifs/recovery.c`.
