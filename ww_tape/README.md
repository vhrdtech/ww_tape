# ww_tape

Tape blocks: tracks of ShrinkWrap records in self-checking, variable-length blocks, for live feeds, log files and raw
media. The crate docs give the byte layout of every block; the [docs](../docs/index.md) explain the format.

- `alloc`: the positional reader (`TapeReader` over a `ByteSource`) and the piece chain check, without std (WASM).
- `std` (default): owned describe records, file reading and writing.

`golden/` holds the golden files: the exact bytes of one write of fixed input per block kind, checked by the tests and
rewritten by `just tape-golden-update` after a deliberate format change.

`api_snapshots/` holds the snapshot of the crate's own API (`Describe` and the types it names) that WireWeaver's
`ww api save` writes: WireWeaver finds a dependency's bundle there, so a reader without this crate can decode track 0.
After a change to those types, `ww api save ww_tape` in this repository saves it again.
