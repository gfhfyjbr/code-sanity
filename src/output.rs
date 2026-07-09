//! Machine-readable CLI output (`--json`).
//!
//! In JSON mode a command writes exactly one compact JSON document to stdout —
//! success or failure — and stderr stays free-form human diagnostics outside
//! the contract. Consumers must ignore unknown fields; fields are added over
//! time but never renamed or retyped. Exit codes are unchanged by this mode.

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Human,
    Json,
}

impl Output {
    pub fn is_json(self) -> bool {
        matches!(self, Output::Json)
    }

    /// Print the one-line success envelope to stdout. No-op in human mode so
    /// a stray call can never pollute the human output.
    pub fn emit(self, command: &'static str, data: Value, elapsed_ms: Option<u128>) {
        if !self.is_json() {
            return;
        }
        let mut envelope = json!({ "ok": true, "command": command, "data": data });
        if let Some(ms) = elapsed_ms {
            envelope["elapsed_ms"] = json!(ms as u64);
        }
        println!("{envelope}");
    }
}

/// Print the one-line error envelope to stdout. `extra` merges additional
/// fields (e.g. `journal_path`, `failures`) into the error object.
pub fn emit_error(command: &str, kind: &str, message: &str, extra: Value) {
    let mut error = json!({ "kind": kind, "message": message });
    if let Some(map) = extra.as_object() {
        for (key, value) in map {
            error[key] = value.clone();
        }
    }
    println!(
        "{}",
        json!({ "ok": false, "command": command, "error": error })
    );
}
