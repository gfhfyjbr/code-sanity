//! Workspace-local dotenv loading for provider credentials.

use anyhow::{Context, Result, anyhow};
use std::path::Path;

pub(crate) fn load_workspace_env(root: &Path) -> Result<()> {
    let path = root.join(".env");
    match path.try_exists() {
        Ok(false) => return Ok(()),
        Ok(true) => {}
        Err(err) => return Err(err).with_context(|| format!("inspect {}", path.display())),
    }
    dotenvy::from_path(&path).map_err(|_| {
        anyhow!(
            "failed to load {}: check dotenv syntax (file contents withheld)",
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    const CHILD_MARKER: &str = "CODE_SANITY_DOTENV_CHILD_ROOT";
    const TEST_KEY: &str = "CODE_SANITY_DOTENV_PRECEDENCE_TEST_7F91";

    #[test]
    fn dotenv_child() {
        let Some(root) = std::env::var_os(CHILD_MARKER) else {
            return;
        };
        load_workspace_env(Path::new(&root)).unwrap();
        assert_eq!(
            std::env::var(TEST_KEY).unwrap(),
            std::env::var("CODE_SANITY_DOTENV_EXPECTED").unwrap()
        );
    }

    #[test]
    fn workspace_dotenv_loads_without_overriding_exported_values() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".env"), format!("{TEST_KEY}=from-file\n")).unwrap();
        let current = std::env::current_exe().unwrap();

        let loaded = Command::new(&current)
            .args(["--exact", "envfile::tests::dotenv_child"])
            .env(CHILD_MARKER, root.path())
            .env("CODE_SANITY_DOTENV_EXPECTED", "from-file")
            .env_remove(TEST_KEY)
            .output()
            .unwrap();
        assert!(
            loaded.status.success(),
            "dotenv child failed: {}",
            String::from_utf8_lossy(&loaded.stderr)
        );

        let preserved = Command::new(current)
            .args(["--exact", "envfile::tests::dotenv_child"])
            .env(CHILD_MARKER, root.path())
            .env("CODE_SANITY_DOTENV_EXPECTED", "exported")
            .env(TEST_KEY, "exported")
            .output()
            .unwrap();
        assert!(
            preserved.status.success(),
            "dotenv precedence child failed: {}",
            String::from_utf8_lossy(&preserved.stderr)
        );
    }

    #[test]
    fn malformed_dotenv_error_withholds_contents() {
        let root = tempfile::tempdir().unwrap();
        let secret = "do-not-print-this-secret";
        fs::write(root.path().join(".env"), format!("BROKEN='{secret}\n")).unwrap();
        let error = load_workspace_env(root.path()).unwrap_err().to_string();
        assert!(error.contains("check dotenv syntax"));
        assert!(!error.contains(secret));
    }
}
