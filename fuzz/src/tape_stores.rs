//! A chain on memory tapes driven by the input's operations: never a panic, and a reader finds what the model holds.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::stores(data));
