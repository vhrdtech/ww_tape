# ww_tape_codecs_benchmarks

Benchmarks of `ww_tape_codecs`. Its binary runs every float sample codec on simulated, decoded and sensor data. It runs
the same quantities stored as fixed-point integers with `DELTA` next to them.

The binary checks that every value comes back bit for bit. It prints a Markdown table of ratio, bits per value, and
encode and decode time per value.

```sh
just bench-sample-codecs              # fetches the speech file once, then runs
just bench-sample-codecs --chunk 1024 --repeats 9
```

The data sets, how they are made and the measured table are in the docs: [Sample codecs](../docs/format.md#sample-codecs).
