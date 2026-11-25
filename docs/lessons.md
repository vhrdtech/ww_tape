---
level: nomicon
---

# Lessons from tape and flash

Tape drives and flash memory met the problems of a [[raw medium]] long before this format did. They found data of an
earlier life behind the current end, erased space in the middle and writes retried after an error. Each rule of [Where
the data ends on a raw card](raw_media.md) follows one of their solutions, taken as far as it fits a tape. Each point
names its source, listed at the end.

<figure class="ww-figure" markdown="0">
--8<-- "docs/figures/lessons.svg"
<figcaption>The problems on one reused card, each marked with the stores that met it first. The data ends at the first
blank space that the next blocks do not continue in the same session.</figcaption>
</figure>

## Sessions and the end of the data

A reused medium holds valid data of an earlier write behind the current end. LTO marks every data set with a write pass
number, which grows whenever data is overwritten [ECMA-319, §13.5.16]. UBI on flash does the same with an image
sequence number on every eraseblock [UBI].

| Problem | Earlier solution | A tape's rule |
|---|---|---|
| Old data behind the new | a write pass number per data set (LTO), a sequence number per eraseblock (UBI) | a [[session tag]] in every [[block]] header |
| Where the data ends | data after the end-of-data mark is invalid [ECMA-319, §13.4.3] | blank space and a change of session end it |

A [[tape]]'s session tag is random, derived from a seed or derived from a piece id, never a counter. A wiped card or a
device without stored state has no counter to continue. A tape also has no end-of-data block, since a crash leaves
none.

## Erased bytes and gaps

Erased is not always zero, and a blank unit is not always the end. A tape's blank space is therefore either value, and
its [[layout]] says how far a reader looks past it, in units that follow the media.

| Medium | What it does | Source |
|---|---|---|
| NAND flash | erases to FFh | [Micron TN-29-19] |
| SD card | reads 0 or 1 after an erase, by vendor | [SD 9.10, §4.3.5.1] |
| eMMC | reports which value it uses | [JESD84-A441] |
| NAND under UBIFS | a failed program leaves erased pages behind, and the scan looks past them | [UBIFS] |
| SD card | allocation units reach 64 MB | [SD 9.10, §4.10.2.4, Table 4-48] |
| NAND | erase blocks and pages | [ONFI 6.0, §3.1] |

A NAND page is programmed once. Programming the same part of a page twice without an erase gives indeterminate contents
[ONFI 6.0, §6.7.1.25]. Micron's MLC example allows one program per page [Micron TN-29-19]. A [[writer]] on raw NAND
therefore pads the rest of the page at each sync, and a [[reader]] takes the padding as a [[gap]].

## Rewrites, indexes, compression and correction

Four more rules of a tape follow a store that solved the same problem:

| Problem | Earlier solution | A tape's rule |
|---|---|---|
| A write retried after an error | LTO rewrites the data set whole, and the later write counts [ECMA-319, §15.4] | of a [[duplicate]] block, the reader takes the later copy |
| An index found by a scan | each LTFS index points to itself and to the one before [LTFS] | an index block carries both offsets, and the reader checks the self offset |
| Data that does not compress | SLDC switches to copying bytes, so it grows by about 0.05 % instead of 12.5 % [ECMA-321, §7.2] | the writer keeps a block's plain body when coding does not shrink it |
| Bit errors | ONFI leaves correction to the host controller, per 512 bytes or a larger codeword [ONFI 6.0, §6.7.1.26] | a tape only detects what that correction missed |

Correction thus belongs to the medium, as [When bytes go bad](damage.md) explains.

## Sources

- [ECMA-319] Ecma International,
  [ECMA-319](https://ecma-international.org/publications-and-standards/standards/ecma-319/): the Ultrium-1 tape format
  of LTO.
- [ECMA-321] Ecma International,
  [ECMA-321](https://ecma-international.org/publications-and-standards/standards/ecma-321/): SLDC.
- [JESD84-A441] JEDEC, JESD84-A441: the eMMC standard, version 4.41. JEDEC
  hands it out free after a sign-in, so it has no link here.
- [LTFS] SNIA,
  [The LTFS Format Specification, version 2.5.1](https://www.snia.org/sites/default/files/technical-work/ltfs/release/SNIA-LTFS-Format-2-5-1-Standard.pdf),
  §5.4.2 and §5.4.3.
- [Micron TN-29-19] Micron, technical note TN-29-19: NAND Flash 101. Micron hands it out after a sign-in on its site, so
  it has no link here.
- [ONFI 6.0] ONFI, [the ONFI specification, revision 6.0](https://onfi.org/files/ONFI_6_0_Final.pdf).
- [SD 9.10] SD Association, the Physical Layer Simplified Specification, version 9.10, on
  [its simplified specifications page](https://www.sdcard.org/downloads/pls/).
- [UBI] Linux kernel,
  [`drivers/mtd/ubi/ubi-media.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/drivers/mtd/ubi/ubi-media.h).
- [UBIFS] Linux kernel,
  [`fs/ubifs/scan.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/ubifs/scan.c) and
  [`fs/ubifs/recovery.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/ubifs/recovery.c).
