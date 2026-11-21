# ww_tape_fs

The files of [ww_tape](../ww_tape):

- `FileSink`, a sink that writes a tape into a file under its final name, new or appended in place;
- `Files`, a store that keeps each piece of a chain in a file of a directory, or every piece in one file;
- `FileSource`, which reads a closed piece back.

A file sink holds the kernel's exclusive lock on its open file, so a second writer of the same file is refused. A
crash leaves nothing behind. On a network or user-space file system it returns a warning (`FsWarning`).

`ww_tape_fs::writer` opens a `TapeWriter` by path (`create`, `append`, `resume`, `cut`) and `ww_tape_fs::chain` a
`PieceChain` in a directory (`create`, `resume`). Everything that names no path, the memory sinks included, is in
`ww_tape`.
