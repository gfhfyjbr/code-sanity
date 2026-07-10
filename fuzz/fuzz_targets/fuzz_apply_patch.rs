//! No-panic over the pure applier: anchor math, CRLF handling, splice
//! offsets. Apply errors are expected outcomes, not findings.
//!
//! Seed format (hand-writable): file content, a line holding exactly `%%%`,
//! then the unified diff. Without the marker the whole input is the patch,
//! applied against empty content.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (content, patch) = code_sanity::patch::fuzz_api::split_apply_seed(data);
    code_sanity::patch::fuzz_api::parse_and_apply(&content, &patch);
});
