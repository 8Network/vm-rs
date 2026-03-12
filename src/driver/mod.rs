//! VmDriver trait — platform-agnostic VM lifecycle.
//!
//! Two implementations:
//! - `AppleVzDriver` — macOS via Apple Virtualization.framework
//! - `CloudHvDriver` — Linux via Cloud Hypervisor REST API

#[cfg(target_os = "macos")]
pub mod apple_vz;

#[cfg(target_os = "linux")]
pub mod cloud_hv;

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
