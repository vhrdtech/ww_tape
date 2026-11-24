# ww_tape_testalloc

`Counting`, a global allocator over the system's for test binaries and tools. A heap test measures the heap a call
adds on the calling thread (`thread_heap_of`), so the harness on its other threads doesn't count. A tool measuring
alone reads the process-wide bytes live and their peak. `Counting` also keeps each thread's largest single allocation
(`take_largest`) and can refuse allocations past a size (`Counting::refusing_over`), as a fuzz case's limit needs.

The workspace forbids unsafe code; this crate's `GlobalAlloc` impl is the one place a test or tool needs it for
counting. Install it as a binary's `#[global_allocator]`.
