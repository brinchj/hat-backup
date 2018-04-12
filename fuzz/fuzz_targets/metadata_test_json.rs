#![no_main]
#[macro_use]
extern crate libfuzzer_sys;
extern crate hat_fuzz;

fuzz_target!(|data: &[u8]| { hat_fuzz::metadata_test_json(data) });
