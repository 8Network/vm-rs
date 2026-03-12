//! VM setup — image preparation, cloud-init seed ISO generation, SSH keys.
//!
//! The user specifies what OS image they want. vm-rs provides:
//! - A catalog of known cloud images (Ubuntu, Alpine) with verified URLs
//! - `prepare_image()` to download and cache images by spec
//! - `create_seed_iso()` to generate per-VM cloud-init configuration
//! - `generate_ssh_key()` to create SSH key pairs for VM access
//!
//! Users can also bring their own kernel + disk image — just point VmConfig at the files.

mod image;
mod seed;
mod ssh;

use std::path::PathBuf;

pub use image::{prepare_image, Arch, ImageSpec, PreparedImage};
pub use seed::{
    create_seed_iso, HealthCheckConfig, NicConfig, ProcessConfig, SeedConfig, VolumeMountConfig,
};
pub use ssh::generate_ssh_key;

/// Default data directory for vm-rs assets.
///
/// Reads `VMRS_DATA_DIR` env var, falls back to `~/.vm-rs/`.
pub fn data_dir() -> Result<PathBuf, SetupError> {
    if let Ok(dir) = std::env::var("VMRS_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").map_err(|_| {
        SetupError::Config(
            "HOME environment variable not set. Set VMRS_DATA_DIR explicitly.".into(),
        )
    })?;
    Ok(PathBuf::from(home).join(".vm-rs"))
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Setup errors.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("ISO creation failed: {0}")]
    IsoCreation(String),

    #[error("asset download failed: {0}")]
    AssetDownload(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("unsupported image: {0}")]
    UnsupportedImage(String),
}
