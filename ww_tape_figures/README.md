# ww_tape_figures

The figures of the book pages in `docs/` that show bytes, offsets or numbers. Each is drawn from blocks `ww_tape`
writes and reads back, and therefore a figure cannot drift from the format.

The figures show:

- a block's header and body;
- a closed piece on the 4 KiB grid;
- puts and copies of a state track;
- the grids of a series track;
- the drive's crash and its resume;
- the drive's five tracks over its hour and the blocks that hold them;
- the file of the drive's first minute, start and end to scale.
- the charts and tables of "Sizes and speeds", from the bench reports in `docs/bench/` (`just bench-book`), and a check
  that every number the page's prose writes comes from those reports; a report of a debug build or a dirty tree is
  refused.

The figures are SVG files in `docs/plots/`, kept in git so the WireWeaver book takes them with the pages. Their
classes (`ww-svg`, `line`, `thin`, `faint`, `accent`, ...) take the book's colours in light and dark.

```sh
just book-figures   # rewrite docs/plots/*.svg
```

A test (`tests/current.rs`) draws every figure again and fails when a file in `docs/plots/` differs. A format change
that moves a byte therefore also asks for the figures to be redrawn.

Concept drawings that hold no numbers are drawn by
hand in `docs/figures/`.
