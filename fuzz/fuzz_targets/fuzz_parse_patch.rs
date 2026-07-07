//! No-panic + determinism + counted-hunk contract over the unified-diff
//! parser, which consumes input written by LLM agents.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    code_sanity::patch::fuzz_api::parse(&input);
});
