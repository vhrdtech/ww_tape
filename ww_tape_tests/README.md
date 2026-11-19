# ww_tape_tests

The integration tests of [ww_tape](../ww_tape), through its public API: writing and reading tapes, pieces and chains,
resumes, locks, golden files and the book pages against the code. They sit in a crate of their own so they can use
the crates built on `ww_tape` without a dependency cycle. The crate has no library code; run the tests with
`cargo nextest run -p ww_tape_tests`.
