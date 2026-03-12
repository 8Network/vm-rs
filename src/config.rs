//! VM configuration types — everything needed to boot and manage a VM.

use std::os::unix::io::RawFd;
use std::path::PathBuf;

/// Readiness marker written to the serial console when the VM is ready.
/// The full output is `VMRS_READY <ip_address>`.
pub const READY_MARKER: &str = "VMRS_READY";

/// Everything needed to boot a VM.
///
/// Three boot modes are supported:
///
/// **Initramfs boot** (fast, stateless):
///   Set `kernel` + `initramfs` + `cmdline` + `shared_dirs`. Leave `root_disk`
///   and `seed_iso` as `None`. Config delivered via VirtioFS shared directories.
///   The initramfs IS the root filesystem (unpacked into RAM by the kernel).
///
/// **Cloud-init boot** (traditional, disk-based):
///   Set `kernel` + `root_disk` + `seed_iso`. Cloud-init reads its config from
///   the seed ISO (NoCloud datasource). Requires a base disk image.
///
/// **UEFI boot** (macOS 13+ / Cloud Hypervisor):
///   Set `efi_variable_store` to enable UEFI boot. Required for booting
///   arbitrary distro ISOs and UEFI-only images. The variable store persists
///   UEFI NVRAM across reboots.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Unique name for this VM.
    pub name: String,
    /// Namespace (logical grouping, e.g., stack name).
    pub namespace: String,
    /// Path to the kernel image (required for direct Linux boot, ignored for UEFI).
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

    // ─── New capabilities ───────────────────────────────────────────────

    /// Enable vsock device for host↔guest communication (default: false).
    ///
    /// vsock provides bidirectional, FD-based I/O without network setup.
    /// Use for agent commands, health checks, and file transfer.
    pub vsock: bool,
    /// Persistent machine identifier bytes (default: None = generate new).
    ///
    /// Save `VmHandle::machine_id` after first boot and pass it back here
    /// on subsequent boots to maintain stable VM identity.
    pub machine_id: Option<Vec<u8>>,
    /// Path to EFI variable store for UEFI boot (default: None = Linux boot).
    ///
    /// If the file exists, it's opened; if not, it's created.
    /// Required for booting arbitrary distro ISOs and UEFI-only images.
    pub efi_variable_store: Option<PathBuf>,
    /// Enable Rosetta x86_64-to-ARM64 translation (default: false).
    ///
    /// macOS 13+ / Apple Silicon only. Adds a VirtioFS share at tag "rosetta".
    /// Guest must register the binary with binfmt_misc after mounting.
    pub rosetta: bool,
}

/// Network attachment for a VM.
#[derive(Debug, Clone)]
pub enum NetworkAttachment {
    /// File descriptor pair for L2 switch port (macOS).
    /// The FD is the VM's end of a socketpair — the switch holds the other end.
    SocketPairFd(RawFd),
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
    /// PID of the VMM process (Linux: cloud-hypervisor, macOS: not applicable).
    pub pid: Option<u32>,
    /// Serial console log path.
    pub serial_log: PathBuf,
    /// Machine identifier bytes (for persistence across reboots).
    ///
    /// Save this after first boot and pass back via `VmConfig::machine_id`
    /// on subsequent boots to maintain stable VM identity.
    pub machine_id: Option<Vec<u8>>,
}

/// VM lifecycle state.
#[derive(Debug, Clone, PartialEq)]
pub enum VmState {
    /// VM is being created / booting.
    Starting,
    /// VM is running and reachable.
    Running {
        /// IP address assigned to the VM.
        ip: String,
    },
    /// VM execution is suspended (memory preserved).
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
    fn vm_state_display_paused() {
        assert_eq!(VmState::Paused.to_string(), "paused");
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
}
