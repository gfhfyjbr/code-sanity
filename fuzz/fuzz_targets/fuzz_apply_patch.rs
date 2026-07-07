//! No-panic over the pure applier: anchor math, CRLF handling, splice
//! offsets. Apply errors are expected outcomes, not findings.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&str, &str)| {
    let (content, patch) = input;
    code_sanity::patch::fuzz_api::parse_and_apply(content, patch);
});
