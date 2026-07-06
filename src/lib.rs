pub mod cli;
pub mod config;
pub mod db;
pub mod index;
pub mod journal;
pub mod map;
pub mod mcp;
pub mod patch;
pub mod sanitize;
pub mod search;
pub mod verify;

pub use config::{Config, Layout};
pub use index::{IndexReport, index_workspace, init_workspace};
pub use patch::{
    ApplyReport, RecoverReport, RenameReport, apply_patch_text, project_mirror_edit,
    recover_workspace, rename_alias, write_sanitized_content,
};
pub use search::{SearchMatch, read_sanitized_file, search_mirror};
pub use verify::{VerifyReport, verify_workspace};
