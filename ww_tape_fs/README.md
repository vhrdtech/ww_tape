# ww_tape_fs

The files of [ww_tape](../ww_tape):

- `FileSink`, a sink that writes a tape into a file, as `NAME.partial` until its finish, or appended in place;
- `Files`, a store that keeps each piece of a chain in a file of a directory, or every piece in one file;
- `FileSource`, which reads a finished piece back.

A file sink holds an exclusive lock on
`NAME.lock` beside its file, so a second writer of the same file is refused.

`ww_tape_fs::writer` opens a `TapeWriter` by path (`create`, `append`, `resume`, `cut`) and `ww_tape_fs::chain` a
`PieceChain` in a directory (`create`, `resume`). Everything that names no path, the memory sinks included, is in
`ww_tape`.
