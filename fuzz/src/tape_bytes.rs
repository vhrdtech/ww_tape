//! ww_tape bytes tracks on any bytes: values of the tracks a file's index names, read whole or refused, never
//! partial (ww_tape_fuzz::targets::bytes).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::bytes(data));
