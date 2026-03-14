//! VmDriver trait — platform-agnostic VM lifecycle.
//!
//! Two implementations:
//! - `AppleVzDriver` — macOS via Apple Virtualization.framework
//! - `CloudHvDriver` — Linux via Cloud Hypervisor REST API

#[cfg(target_os = "macos")]
pub mod apple_vz;

#[cfg(target_os = "linux")]
pub mod cloud_hv;

#[cfg(target_os = "windows")]
pub mod whp;

use crate::config::{VmConfig, VmHandle, VmState};

/// Platform-agnostic VM lifecycle.
///
/// Apple VZ on macOS, Cloud Hypervisor on Linux.
/// Each driver manages the hypervisor-specific details of booting,
/// stopping, and querying VMs.
pub trait VmDriver: Send + Sync {
    /// Boot a VM with the given configuration.
    ///
    /// Returns a handle that can be used to query state, stop, or kill the VM.
    /// The VM may still be in `Starting` state when this returns — use
    /// `state()` to poll for `Running`.
    fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError>;

    /// Stop a running VM gracefully.
    ///
    /// Sends a shutdown signal and waits for the guest to power off.
    fn stop(&self, handle: &VmHandle) -> Result<(), VmError>;

    /// Force-kill a VM immediately.
    ///
    /// Does not wait for graceful shutdown. Use as a last resort.
    fn kill(&self, handle: &VmHandle) -> Result<(), VmError>;

    /// Query current VM state.
    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError>;

    /// Pause a running VM (suspend execution, preserve state).
    ///
    /// Not all drivers support pause. The default returns an error.
    fn pause(&self, handle: &VmHandle) -> Result<(), VmError> {
        Err(VmError::Hypervisor(format!(
            "pause is not supported by this driver for VM '{}'",
            handle.name
        )))
    }

    /// Resume a paused VM.
    ///
    /// Not all drivers support resume. The default returns an error.
    fn resume(&self, handle: &VmHandle) -> Result<(), VmError> {
        Err(VmError::Hypervisor(format!(
            "resume is not supported by this driver for VM '{}'",
            handle.name
        )))
    }
}

/// VM operation errors.
#[derive(Debug, thiserror::Error)]
pub enum VmError {
    /// VM boot failed.
    #[error("boot failed for '{name}': {detail}")]
    BootFailed { name: String, detail: String },

    /// VM not found (already stopped/destroyed, or never existed).
    #[error("VM '{name}' not found")]
    NotFound { name: String },

    /// Stop/kill failed.
    #[error("failed to stop '{name}': {detail}")]
    StopFailed { name: String, detail: String },

    /// State query failed.
    #[error("failed to query state for '{name}': {detail}")]
    StateFailed { name: String, detail: String },

    /// Hypervisor-specific error (e.g., Apple VZ framework error, Cloud Hypervisor API error).
    #[error("hypervisor error: {0}")]
    Hypervisor(String),

    /// I/O error (disk, network, file operations).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration error.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
}

impl From<crate::oci::registry::OciError> for VmError {
    fn from(e: crate::oci::registry::OciError) -> Self {
        VmError::Hypervisor(format!("OCI error: {}", e))
    }
}

impl From<crate::setup::SetupError> for VmError {
    fn from(e: crate::setup::SetupError) -> Self {
        VmError::Hypervisor(format!("setup error: {}", e))
    }
}

/// Check a serial console log for the VM readiness marker.
///
/// The guest writes `VMRS_READY <ip>` when boot completes.
/// Returns `Some(ip)` if found, `None` otherwise.
pub(crate) fn check_ready_marker(log_path: &std::path::Path) -> Option<String> {
    let content = match std::fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %log_path.display(), "failed to read serial log: {}", e);
            return None;
        }
    };
    let pos = content.find(crate::config::READY_MARKER)?;
    let after = &content[pos + crate::config::READY_MARKER.len()..];
    let ip = after.split_whitespace().next()?.trim().to_string();
    if ip.is_empty() {
        None
    } else {
        Some(ip)
    }
}
