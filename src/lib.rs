// Workspace locking is flock-based and file durability relies on unix fd
// semantics; other platforms would compile into silently unsafe binaries.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("code-sanity supports Linux and macOS only (flock-based locking and unix fd APIs)");

pub mod cli;
pub mod config;
pub mod db;
pub mod embed;
pub mod fsutil;
pub mod index;
pub mod journal;
pub mod llm;
pub mod lock;
pub mod logging;
pub mod map;
pub mod mcp;
pub mod patch;
pub mod proposal;
pub mod redact;
pub mod sanitize;
pub mod search;
pub mod strict;
pub mod verify;

pub use config::{Config, Layout};
pub use embed::{EmbedReport, SemanticMatch, embed_index, semantic_search};
pub use index::{IndexReport, index_workspace, init_workspace};
pub use patch::{
    ApplyReport, RecoverReport, RenameReport, apply_patch_text, project_mirror_edit,
    recover_workspace, rename_alias, write_sanitized_content,
};
pub use search::{SearchMatch, read_sanitized_file, search_mirror};
pub use verify::{VerifyFailed, VerifyReport, verify_workspace};
