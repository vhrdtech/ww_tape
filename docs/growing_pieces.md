# Reading a growing piece

A piece is not always finished when a reader opens it: a writer may have closed it and opened it again. This page
explains what a reader sees then, and how it notices that the piece grew.

## The close in force

A writer may close a piece, open it again later and write on after the close. Each such session ends with a close
record and a tail of its own, so one piece can hold several closes ([Closing more than
once](format.md#closing-more-than-once)).

The reader takes the last close whose tail is whole and follows it directly: the **close in force**. `PieceInfo::close`
holds it, and `TapeReader::closes` lists it with every close before it, oldest first.

Bytes after the close in force form an **unclosed tail**: a writer still writing, or one that a crash stopped. The
reader lists their whole blocks as it lists those of an unclosed piece, and never reports them as damage.

- `PieceInfo::closed_end` gives the offset where the close in force ends, at the end of its tail.
- `PieceInfo::is_closed` is true when nothing follows that tail.
- `TapeReader::unclosed` gives the range of the unclosed tail.
- `TapeReader::closed_entries` gives the entries that the close in force covers.
- `TapeReader::close_summary` reads the summary block that the close in force names, when it names one.

For example, a recorder closes its file every minute, and a viewer opens the file in between. The viewer reads every
closed minute and the whole blocks of the minute being written.

## Noticing that a tape grew

A reader sees the bytes that its source held when it opened. A writer that appends later changes nothing the reader
holds, so the reader goes on reading a consistent prefix.

To notice growth, compare the source's size (`ByteSource::size`, through `TapeReader::source`) with
`TapeReader::len`. When the size is larger, open the source again to read the new bytes.

## Checking each close

`TapeReader::verify` checks each stretch of the piece against the close that ends it, oldest first. It returns a
`Verification`: `holds` says whether the check passed, and `unchecked` names the bytes that no close covers.

## Where to go next

- [Reading a tape](reading.md) covers opening, the index and checking a piece.
- [Closing more than once](format.md#closing-more-than-once) gives the bytes of an append.
