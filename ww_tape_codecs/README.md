# ww_tape_codecs

[![crates.io](https://img.shields.io/crates/v/ww_tape_codecs)](https://crates.io/crates/ww_tape_codecs)
[![docs.rs](https://img.shields.io/docsrs/ww_tape_codecs)](https://docs.rs/ww_tape_codecs)

The column codecs of [ww_tape](../ww_tape):

- bit packing with vector kernels: AVX-512 and Advanced Vector Extensions 2 (AVX2) on x86, NEON (the Arm vector
  extension) on Arm;
- the 32-bit cyclic redundancy check with the Castagnoli polynomial (CRC-32C);
- varints and bounded reads of numbers from file bytes (`FromFile`);
- lz4 columns and the summary statistics kernels.

Nothing here knows a block, a chunk, a track or a piece. `ww_tape` builds the format on top and re-exports these
modules under its own paths.

Without default features the crate is `no_std` and picks vector kernels from compile-time target features only.
