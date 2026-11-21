# ww_tape_fuzz

Fuzzing of ww_tape's decoders, as a library the `fuzz/` targets and the tests share:

- `targets::<name>(data)`: the body of the fuzz target `tape-<name>`.
- `hostile::<decoder>(data)`: a block or file with a correct structure and hostile values from `data`. These values
  go into counts, lengths, offsets, widths and layout fields. Each CRC is computed again.
- `targets::<group>_hostile(data)`: the body of `tape-<group>-hostile`, a hostile block through its decoders.
- `seeds/<target>/`: the seed files of each target, which `ww_tape_fuzz_seeds write` writes. They hold valid blocks
  from the golden files, the inputs behind past fixes, hostile stream inputs and the known failures.
- `ww_tape_fuzz_seeds minimize TARGET HEX` shrinks a failing input.
- `case::run`: one case with its panic and any allocation over 128 MiB as its outcome.

Not published.
