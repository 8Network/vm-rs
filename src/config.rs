//! VM configuration types — everything needed to boot and manage a VM.

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;

/// Readiness marker written to the serial console when the VM is ready.
/// The full output is `VMRS_READY <ip_address>`.
pub const READY_MARKER: &str = "VMRS_READY";

/// Owned VM network endpoint backed by a Unix datagram socket file descriptor.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct VmSocketEndpoint(Arc<OwnedFd>);

#[cfg(unix)]
impl VmSocketEndpoint {
    pub fn new(fd: OwnedFd) -> Self {
        Self(Arc::new(fd))
    }

    pub fn try_clone_owned(&self) -> std::io::Result<OwnedFd> {
        // SAFETY: `dup` duplicates a valid file descriptor we own through `OwnedFd`.
        let duplicated = unsafe { libc::dup(self.as_raw_fd()) };
        if duplicated < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `dup` returned a new owned file descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
    }
}

#[cfg(unix)]
impl AsRawFd for VmSocketEndpoint {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Stable identity for a VM monitor process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmmProcess {
    pid: u32,
    start_time_ticks: Option<u64>,
}

impl VmmProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn start_time_ticks(&self) -> Option<u64> {
        self.start_time_ticks
    }

    /// Create a stable VM monitor process identity.
    ///
    /// `start_time_ticks` should come from the process start-time field in
    /// `/proc/<pid>/stat` when available so the identity can detect PID reuse.
    /// Pass `None` only when the platform cannot provide that information.
    pub fn new(pid: u32, start_time_ticks: Option<u64>) -> Self {
        Self {
            pid,
            start_time_ticks,
        }
    }
}

/// Everything needed to boot a VM.
///
/// Two boot modes are supported:
///
/// **Initramfs boot** (fast, stateless):
///   Set `kernel` + `initramfs` + `cmdline` + `shared_dirs`. Leave `root_disk`
///   and `seed_iso` as `None`. Config delivered via VirtioFS shared directories.
///   The initramfs IS the root filesystem (unpacked into RAM by the kernel).
///
/// **Cloud-init boot** (traditional, disk-based):
///   Set `kernel` + `root_disk` + `seed_iso`. Cloud-init reads its config from
///   the seed ISO (NoCloud datasource). Requires a base disk image.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Unique name for this VM.
    pub name: String,
    /// Namespace (logical grouping, e.g., stack name).
    pub namespace: String,
    /// Path to the kernel image.
    pub kernel: PathBuf,
    /// Path to initramfs (required for initramfs boot, optional for cloud-init boot).
    pub initramfs: Option<PathBuf>,
    /// Path to the root disk image (None for stateless initramfs boot).
    pub root_disk: Option<PathBuf>,
    /// Path to additional data disk (optional).
    pub data_disk: Option<PathBuf>,
    /// Path to cloud-init seed ISO (None for initramfs boot with VirtioFS config).
    pub seed_iso: Option<PathBuf>,
    /// Number of vCPUs.
    pub cpus: usize,
    /// Memory in megabytes.
    pub memory_mb: usize,
    /// Network attachments (L2 switch ports or TAP devices).
    pub networks: Vec<NetworkAttachment>,
    /// Shared directories (host → guest via VirtioFS).
    pub shared_dirs: Vec<SharedDir>,
    /// Path to serial console log file.
    pub serial_log: PathBuf,
    /// Kernel command line arguments (optional — platform-specific defaults used if None).
    pub cmdline: Option<String>,
    /// Linux network namespace to run the VM in (optional).
    /// When set, the VMM process is spawned inside `ip netns exec <netns>`.
    pub netns: Option<String>,
    /// Enable vsock device for host-guest communication.
    pub vsock: bool,
    /// Persistent machine identifier (opaque bytes, driver-specific).
    pub machine_id: Option<Vec<u8>>,
    /// Path to EFI variable store for UEFI boot (optional).
    pub efi_variable_store: Option<PathBuf>,
    /// Enable Rosetta translation layer (macOS only, Apple Silicon).
    pub rosetta: bool,
}

impl VmConfig {
    /// Validate configuration invariants.
    pub fn validate(&self) -> Result<(), crate::driver::VmError> {
        use crate::driver::VmError;
        if self.name.is_empty() {
            return Err(VmError::InvalidConfig("VM name must not be empty".into()));
        }
        if self.name.len() > 128 {
            return Err(VmError::InvalidConfig("VM name must be 128 characters or fewer".into()));
        }
        if !self.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
            return Err(VmError::InvalidConfig(
                "VM name must contain only alphanumeric characters, hyphens, underscores, and dots".into(),
            ));
        }
        if self.name.starts_with('.') || self.name.starts_with('-') {
            return Err(VmError::InvalidConfig("VM name must not start with '.' or '-'".into()));
        }
        if self.cpus == 0 {
            return Err(VmError::InvalidConfig("cpus must be at least 1".into()));
        }
        if self.memory_mb == 0 {
            return Err(VmError::InvalidConfig("memory_mb must be at least 1".into()));
        }
        if self.kernel.as_os_str().is_empty() {
            return Err(VmError::InvalidConfig("kernel path must not be empty".into()));
        }
        Ok(())
    }
}

/// Network attachment for a VM.
#[derive(Debug, Clone)]
pub enum NetworkAttachment {
    /// Owned socket endpoint for an L2 switch port (macOS).
    /// The endpoint is the VM's end of a socketpair — the switch holds the other end.
    #[cfg(unix)]
    SocketPairFd(VmSocketEndpoint),
    /// TAP device name (Linux).
    Tap { name: String, mac: Option<String> },
}

/// Host directory shared with guest via VirtioFS.
#[derive(Debug, Clone)]
pub struct SharedDir {
    /// Path on the host.
    pub host_path: PathBuf,
    /// Mount tag inside the guest.
    pub tag: String,
    /// Read-only mount.
    pub read_only: bool,
}

/// Handle to a running (or stopped) VM.
#[derive(Debug, Clone)]
pub struct VmHandle {
    /// VM name.
    pub name: String,
    /// Namespace.
    pub namespace: String,
    /// Current state.
    pub state: VmState,
    /// Process identity of the VMM process (Linux: cloud-hypervisor, macOS: not applicable).
    pub process: Option<VmmProcess>,
    /// Serial console log path.
    pub serial_log: PathBuf,
    /// Persistent machine identifier (opaque bytes, driver-specific).
    pub machine_id: Option<Vec<u8>>,
}

/// VM lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum VmState {
    /// VM is being created / booting.
    Starting,
    /// VM is running and reachable.
    Running {
        /// IP address assigned to the VM.
        ip: String,
    },
    /// VM is paused (execution suspended, state preserved).
    Paused,
    /// VM was stopped gracefully.
    Stopped,
    /// VM failed to boot or crashed.
    Failed {
        /// Human-readable failure reason.
        reason: String,
    },
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmState::Starting => write!(f, "starting"),
            VmState::Running { ip } => write!(f, "running ({})", ip),
            VmState::Paused => write!(f, "paused"),
            VmState::Stopped => write!(f, "stopped"),
            VmState::Failed { reason } => write!(f, "failed: {}", reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_state_display_starting() {
        assert_eq!(VmState::Starting.to_string(), "starting");
    }

    #[test]
    fn vm_state_display_running() {
        let state = VmState::Running {
            ip: "10.0.1.2".into(),
        };
        assert_eq!(state.to_string(), "running (10.0.1.2)");
    }

    #[test]
    fn vm_state_display_stopped() {
        assert_eq!(VmState::Stopped.to_string(), "stopped");
    }

    #[test]
    fn vm_state_display_failed() {
        let state = VmState::Failed {
            reason: "timeout".into(),
        };
        assert_eq!(state.to_string(), "failed: timeout");
    }

    #[test]
    fn vm_state_equality() {
        assert_eq!(VmState::Starting, VmState::Starting);
        assert_eq!(VmState::Stopped, VmState::Stopped);
        assert_ne!(VmState::Starting, VmState::Stopped);
    }

    #[test]
    fn ready_marker_value() {
        assert_eq!(READY_MARKER, "VMRS_READY");
    }

    #[test]
    fn vm_state_display_paused() {
        assert_eq!(VmState::Paused.to_string(), "paused");
    }

    fn test_vm_config(name: &str) -> VmConfig {
        VmConfig {
            name: name.into(),
            namespace: "test".into(),
            kernel: std::path::PathBuf::from("/tmp/kernel"),
            initramfs: None,
            root_disk: None,
            data_disk: None,
            seed_iso: None,
            cpus: 1,
            memory_mb: 256,
            networks: vec![],
            shared_dirs: vec![],
            serial_log: std::path::PathBuf::from("/tmp/serial.log"),
            cmdline: None,
            netns: None,
            vsock: false,
            machine_id: None,
            efi_variable_store: None,
            rosetta: false,
        }
    }

    #[test]
    fn validate_rejects_empty_name() {
        let config = test_vm_config("");
        let err = config
            .validate()
            .expect_err("empty VM name should fail validation")
            .to_string();
        assert!(err.contains("empty"), "expected 'empty' in error: {}", err);
    }

    #[test]
    fn validate_rejects_path_traversal() {
        let config = test_vm_config("../etc");
        let err = config
            .validate()
            .expect_err("path traversal characters should fail validation")
            .to_string();
        assert!(err.contains("alphanumeric") || err.contains("characters"),
            "expected name validation error: {}", err);
    }

    #[test]
    fn validate_rejects_zero_cpus() {
        let mut config = test_vm_config("good-name");
        config.cpus = 0;
        let err = config
            .validate()
            .expect_err("zero CPUs should fail validation")
            .to_string();
        assert!(err.contains("cpus"), "expected 'cpus' in error: {}", err);
    }

    #[test]
    fn validate_accepts_valid_config() {
        let config = test_vm_config("my-vm.01");
        config.validate().expect("valid config should pass validation");
    }
}
