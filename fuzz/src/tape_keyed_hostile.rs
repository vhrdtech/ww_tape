//! A ww_tape piece of keyed puts whose keyed blocks and index entries lie behind good CRCs
//! (ww_tape_fuzz::hostile::keyed): key lookups hand out only records of the key asked, and a resume closes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| ww_tape_fuzz::targets::keyed_hostile(data));
