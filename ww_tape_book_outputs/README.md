# ww_tape_book_outputs

What the book shows of a program's output. A test calls `check("name.txt", &output)`: it fails when
`docs/generated/name.txt` holds something else, so the page never shows a stale output. `just book-outputs` sets
`WW_TAPE_BOOK_OUTPUTS=write` and runs the tests named `book_output`, which then write the files instead.

A page shows such a file through an include: `--8<-- "docs/generated/name.txt"`. `vz check` fails a file under
`docs/generated/` that no page includes.
