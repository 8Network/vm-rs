//! SSH key pair generation for VM access.

use std::path::{Path, PathBuf};

use super::SetupError;

/// Generate an SSH key pair for VM access. Idempotent — skips if key exists.
///
/// Returns the path to the public key.
pub fn generate_ssh_key(data_dir: &Path) -> Result<PathBuf, SetupError> {
    std::fs::create_dir_all(data_dir).map_err(SetupError::Io)?;

    let key_path = data_dir.join("id_ed25519");
    let pub_path = data_dir.join("id_ed25519.pub");

    if key_path.exists() && pub_path.exists() {
        return Ok(pub_path);
    }

    let output = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&key_path)
        .args(["-N", "", "-q"])
        .output()
        .map_err(SetupError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SetupError::Config(format!(
            "ssh-keygen failed (exit {}): {}",
            output.status,
            stderr.trim()
        )));
    }

    tracing::info!(path = %key_path.display(), "generated SSH key pair");
    Ok(pub_path)
}
