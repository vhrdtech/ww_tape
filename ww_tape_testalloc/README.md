# ww_tape_testalloc

`Counting`, a global allocator over the system's for test binaries and tools. It counts the bytes live and their
peak, so a heap test measures what a call adds (`peak_of`). It also keeps each thread's largest single allocation
(`take_largest`) and can refuse allocations past a size (`Counting::refusing_over`), as a fuzz case's limit needs.

The workspace forbids unsafe code; this crate's `GlobalAlloc` impl is the one place a test or tool needs it for
counting. Install it as a binary's `#[global_allocator]`.
