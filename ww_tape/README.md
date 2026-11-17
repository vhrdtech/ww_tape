# ww_tape

Tape blocks: tracks of ShrinkWrap records in self-checking, variable-length blocks, for live feeds, log files and raw
media. The crate docs give the byte layout of every block; the [docs](../docs/index.md) explain the format.

- `alloc`: the positional reader (`TapeReader` over a `ByteSource`) and the piece chain check, without std (WASM).
- `std` (default): owned describe records, `DescribeSet`, file reading and writing.

`golden/` holds the golden files: the exact bytes of one write of fixed input per block kind, checked by the tests and
rewritten by `just tape-golden-update` after a deliberate format change.
