use std::path::Path;

use crate::driver::VmError;

pub(super) fn clone_disk(base: &Path, target: &Path) -> Result<(), VmError> {
    if target.exists() {
        if let (Ok(base_meta), Ok(target_meta)) =
            (std::fs::metadata(base), std::fs::metadata(target))
        {
            if base_meta.len() == target_meta.len() {
                tracing::debug!(
                    target = %target.display(),
                    "disk clone target already exists with matching size, skipping"
                );
                return Ok(());
            }
            tracing::warn!(
                target = %target.display(),
                base_size = base_meta.len(),
                target_size = target_meta.len(),
                "disk clone target exists but size differs, re-cloning"
            );
            std::fs::remove_file(target).map_err(VmError::Io)?;
        }
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(VmError::Io)?;
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("cp")
            .args(["-c"])
            .arg(base)
            .arg(target)
            .status()
            .map_err(VmError::Io)?;
        if !status.success() {
            tracing::warn!(
                base = %base.display(),
                target = %target.display(),
                status = %status,
                "APFS clone failed; falling back to a full file copy"
            );
            std::fs::copy(base, target).map_err(VmError::Io)?;
        }
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("cp")
            .args(["--reflink=auto"])
            .arg(base)
            .arg(target)
            .status()
            .map_err(VmError::Io)?;
        if !status.success() {
            tracing::warn!(
                base = %base.display(),
                target = %target.display(),
                status = %status,
                "reflink clone failed; falling back to a full file copy"
            );
            std::fs::copy(base, target).map_err(VmError::Io)?;
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        std::fs::copy(base, target).map_err(VmError::Io)?;
    }

    Ok(())
}
