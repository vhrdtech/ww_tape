# ww_tape_codecs

The column codecs of [ww_tape](../ww_tape): bit packing with AVX-512, AVX2 and NEON kernels, CRC-32C, varints,
bounded reads of numbers from file bytes (`FromFile`), lz4 columns and the summary statistics kernels. Nothing here
knows a block, a chunk, a track or a piece; `ww_tape` builds the format on top and re-exports these modules under its
own paths.

Without default features the crate is `no_std` and picks vector kernels from compile-time target features only.
