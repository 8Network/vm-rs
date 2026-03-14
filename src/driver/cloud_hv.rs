//! Linux driver — Cloud Hypervisor via CLI.
//!
//! Spawns the `cloud-hypervisor` binary with CLI arguments for VM boot.
//! Process lifecycle for stop/kill. No API socket needed for basic operations.
//!
//! For advanced features (hotplug, resize, snapshot), we'll add API socket mode
//! via `ch-remote` — Cloud Hypervisor's own CLI tool. See CAPABILITIES.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use crate::config::{NetworkAttachment, VmConfig, VmHandle, VmState, VmmProcess};
use crate::driver::{ReadyMarkerCache, VmDriver, VmError};

// ---------------------------------------------------------------------------
// Internal VM tracking
// ---------------------------------------------------------------------------

/// Metadata for a running Cloud Hypervisor VM process.
struct VmProcess {
    /// Stable identity of the cloud-hypervisor process.
    identity: VmmProcess,
    /// Child handle retained so we can reap exit status and avoid zombie confusion.
    child: Child,
    /// TAP device names (for cleanup on stop).
    tap_devices: Vec<String>,
    /// virtiofsd sidecar child handles retained so they can be terminated and reaped.
    virtiofsd_children: Vec<Child>,
    /// Cached guest readiness marker from the serial log.
    ready: ReadyMarkerCache,
}

/// Cloud Hypervisor driver for Linux.
///
/// Boots VMs via `cloud-hypervisor` CLI args. Manages process lifecycle
/// with signals: SIGTERM for graceful shutdown, SIGKILL for force-kill.
pub struct CloudHvDriver {
    vms: Mutex<HashMap<String, VmProcess>>,
}

impl Default for CloudHvDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudHvDriver {
    pub fn new() -> Self {
        Self {
            vms: Mutex::new(HashMap::new()),
        }
    }
}

impl VmDriver for CloudHvDriver {
    fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
        let ch_binary = find_ch_binary()?;
        let name = &config.name;

        tracing::info!(
            driver = "cloud_hv",
            vm = %name,
            cpus = config.cpus,
            memory_mb = config.memory_mb,
            "booting VM via Cloud Hypervisor"
        );

        // Build CLI command — optionally inside a network namespace
        let mut cmd = if let Some(ref netns) = config.netns {
            let mut c = Command::new("ip");
            c.args(["netns", "exec", netns]);
            c.arg(&ch_binary);
            c
        } else {
            Command::new(&ch_binary)
        };

        // Kernel
        cmd.arg("--kernel").arg(&config.kernel);

        // Kernel command line — default depends on boot mode
        let default_cmdline = if config.root_disk.is_some() {
            "console=ttyS0 root=/dev/vda1 rw ds=nocloud"
        } else {
            "console=ttyS0"
        };
        let cmdline = config.cmdline.as_deref().unwrap_or(default_cmdline);
        cmd.arg("--cmdline").arg(cmdline);

        // Initramfs
        if let Some(ref initramfs) = config.initramfs {
            cmd.arg("--initramfs").arg(initramfs);
        }

        // CPU
        cmd.arg("--cpus").arg(format!("boot={}", config.cpus));

        // Memory — shared=on required when using VirtioFS
        if config.shared_dirs.is_empty() {
            cmd.arg("--memory")
                .arg(format!("size={}M", config.memory_mb));
        } else {
            cmd.arg("--memory")
                .arg(format!("size={}M,shared=on", config.memory_mb));
        }

        // Disks: only attach if provided (initramfs boot needs no disks)
        if let Some(ref root_disk) = config.root_disk {
            cmd.arg("--disk")
                .arg(format!("path={}", root_disk.display()));
        }
        if let Some(ref seed_iso) = config.seed_iso {
            cmd.arg("--disk")
                .arg(format!("path={},readonly=on", seed_iso.display()));
        }
        if let Some(ref data_disk) = config.data_disk {
            cmd.arg("--disk")
                .arg(format!("path={}", data_disk.display()));
        }

        // Network
        let mut tap_devices = Vec::new();
        for net in &config.networks {
            match net {
                NetworkAttachment::Tap { name: tap, mac } => {
                    tap_devices.push(tap.clone());
                    let mac_str = mac.clone().unwrap_or_else(|| generate_mac(name));
                    cmd.arg("--net").arg(format!("tap={},mac={}", tap, mac_str));
                }
                NetworkAttachment::SocketPairFd(_) => {
                    return Err(VmError::InvalidConfig(
                        "SocketPairFd not supported on Linux; use TAP devices".into(),
                    ));
                }
            }
        }

        // Serial console → file
        cmd.arg("--serial")
            .arg(format!("file={}", config.serial_log.display()));
        cmd.arg("--console").arg("off");

        // VirtioFS shared directories via virtiofsd sidecar processes.
        // Each shared dir needs its own virtiofsd instance with a Unix socket.
        let mut virtiofsd_children: Vec<std::process::Child> = Vec::new();
        let vm_dir = config.serial_log.parent().unwrap_or(Path::new("/tmp"));
        for vol in &config.shared_dirs {
            let socket_path = vm_dir.join(format!("virtiofs-{}.sock", vol.tag));
            let virtiofsd = find_virtiofsd()?;
            let host_path = vol.host_path.to_str().ok_or_else(|| {
                VmError::InvalidConfig(format!("non-UTF8 shared dir path: {:?}", vol.host_path))
            })?;

            // Remove stale socket if it exists
            let _ = std::fs::remove_file(&socket_path);

            let child = std::process::Command::new(&virtiofsd)
                .arg(format!("--socket-path={}", socket_path.display()))
                .arg(format!("--shared-dir={}", host_path))
                .arg("--cache=never")
                .arg("--sandbox=chroot")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to spawn virtiofsd for '{}': {}", vol.tag, e),
                })?;

            tracing::info!(
                driver = "cloud_hv",
                vm = %name,
                tag = %vol.tag,
                pid = child.id(),
                "virtiofsd started"
            );
            virtiofsd_children.push(child);

            // Wait for socket to appear (virtiofsd needs a moment)
            let socket_ready = wait_for_socket(&socket_path, std::time::Duration::from_secs(5));
            if !socket_ready {
                // Clean up already-started virtiofsd processes
                cleanup_virtiofsd(virtiofsd_children);
                return Err(VmError::BootFailed {
                    name: name.clone(),
                    detail: format!(
                        "virtiofsd socket did not appear for '{}' at {}",
                        vol.tag,
                        socket_path.display()
                    ),
                });
            }

            cmd.arg("--fs").arg(format!(
                "tag={},socket={},num_queues=1,queue_size=512",
                vol.tag,
                socket_path.display()
            ));
        }

        // Spawn — redirect stdout/stderr to a log file for debugging
        let vmm_log_path = config.serial_log.with_extension("vmm.log");
        let vmm_log = match std::fs::File::create(&vmm_log_path) {
            Ok(file) => file,
            Err(e) => {
                cleanup_virtiofsd(virtiofsd_children);
                return Err(VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to create VMM log file: {}", e),
                });
            }
        };
        let vmm_log_stderr = match vmm_log.try_clone() {
            Ok(file) => file,
            Err(e) => {
                cleanup_virtiofsd(virtiofsd_children);
                return Err(VmError::Io(e));
            }
        };
        let mut process = match cmd.stdout(vmm_log).stderr(vmm_log_stderr).spawn() {
            Ok(child) => child,
            Err(e) => {
                cleanup_virtiofsd(virtiofsd_children);
                return Err(VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to spawn cloud-hypervisor: {}", e),
                });
            }
        };

        let pid = process.id();
        let identity = match process_identity(pid, name) {
            Ok(identity) => identity,
            Err(e) => {
                let _ = process.kill();
                let _ = process.wait();
                cleanup_virtiofsd(virtiofsd_children);
                return Err(e);
            }
        };
        tracing::info!(driver = "cloud_hv", vm = %name, pid = pid, "Cloud Hypervisor process started");

        // Brief pause then check if process exited immediately (bad binary, permissions, etc.)
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Some(status) = process.try_wait().map_err(VmError::Io)? {
            // Clean up virtiofsd processes since VM failed
            cleanup_virtiofsd(virtiofsd_children);
            return Err(VmError::BootFailed {
                name: name.clone(),
                detail: format!(
                    "cloud-hypervisor process (PID {}) exited immediately with {}. Check {}",
                    pid,
                    status,
                    vmm_log_path.display()
                ),
            });
        }

        // Track
        {
            let mut vms = self
                .vms
                .lock()
                .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
            vms.insert(
                name.clone(),
                VmProcess {
                    child: process,
                    identity: identity.clone(),
                    tap_devices,
                    virtiofsd_children,
                    ready: ReadyMarkerCache::default(),
                },
            );
        }

        Ok(VmHandle {
            name: name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Starting,
            process: Some(identity),
            serial_log: config.serial_log.clone(),
            machine_id: None,
        })
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::info!(driver = "cloud_hv", vm = %handle.name, "requesting graceful stop via Cloud Hypervisor");
        let mut vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        let process = if let Some(process) = vms.remove(&handle.name) {
            process
        } else if let Some(ref process) = handle.process {
            validate_cloud_hypervisor_process(process, &handle.name)?;
            let ret = unsafe { libc::kill(process.pid() as i32, libc::SIGTERM) };
            if ret != 0 {
                let errno = std::io::Error::last_os_error();
                return Err(VmError::StopFailed {
                    name: handle.name.clone(),
                    detail: format!(
                        "failed to send SIGTERM to restored VM PID {}: {}",
                        process.pid(),
                        errno
                    ),
                });
            }
            wait_for_pid_exit(process, &handle.name, std::time::Duration::from_secs(10))?;
            return Ok(());
        } else {
            return Err(VmError::NotFound {
                name: handle.name.clone(),
            });
        };

        // SIGTERM → Cloud Hypervisor handles graceful ACPI shutdown
        // SAFETY: Sending SIGTERM to a PID we spawned. PID validity confirmed by prior operations.
        let ret = unsafe { libc::kill(process.identity.pid() as i32, libc::SIGTERM) };
        let wait_result = if ret != 0 {
            let errno = std::io::Error::last_os_error();
            tracing::warn!(
                driver = "cloud_hv",
                vm = %handle.name,
                pid = process.identity.pid(),
                error = %errno,
                "SIGTERM failed (process may already be stopped)"
            );
            Ok(())
        } else {
            // Wait for process to exit (up to 10s)
            wait_for_exit(process.child, std::time::Duration::from_secs(10)).map_err(|e| {
                VmError::StopFailed {
                    name: handle.name.clone(),
                    detail: format!(
                        "cloud-hypervisor PID {} did not exit cleanly: {}",
                        process.identity.pid(),
                        e
                    ),
                }
            })
        };

        cleanup_taps(&process.tap_devices);
        cleanup_virtiofsd(process.virtiofsd_children);
        wait_result
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::warn!(driver = "cloud_hv", vm = %handle.name, "force-killing Cloud Hypervisor VM");
        let mut vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;

        let (identity, mut child, virtiofsd_children, tap_devices) = if let Some(process) = vms.remove(&handle.name)
        {
            (
                process.identity,
                Some(process.child),
                process.virtiofsd_children,
                process.tap_devices,
            )
        } else if let Some(ref process) = handle.process {
            validate_cloud_hypervisor_process(process, &handle.name)?;
            (process.clone(), None, Vec::new(), Vec::new())
        } else {
            return Err(VmError::NotFound {
                name: handle.name.clone(),
            });
        };

        let kill_result = if let Some(child) = child.as_mut() {
            child.kill().map_err(|e| VmError::StopFailed {
                name: handle.name.clone(),
                detail: format!("failed to SIGKILL child PID {}: {}", identity.pid(), e),
            })
        } else {
            let ret = unsafe { libc::kill(identity.pid() as i32, libc::SIGKILL) };
            if ret != 0 {
                let errno = std::io::Error::last_os_error();
                Err(VmError::StopFailed {
                    name: handle.name.clone(),
                    detail: format!(
                        "failed to SIGKILL restored VM PID {}: {}",
                        identity.pid(),
                        errno
                    ),
                })
            } else {
                Ok(())
            }
        };
        let wait_result = if let Some(child) = child {
            wait_for_exit(child, std::time::Duration::from_secs(2)).map_err(|e| {
                VmError::StopFailed {
                    name: handle.name.clone(),
                    detail: format!("failed to reap killed VM PID {}: {}", identity.pid(), e),
                }
            })
        } else {
            Ok(())
        };

        cleanup_taps(&tap_devices);
        cleanup_virtiofsd(virtiofsd_children);
        kill_result?;
        wait_result
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let mut vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;

        let process = match vms.get_mut(&handle.name) {
            Some(p) => p,
            None => {
                if let Some(ref process) = handle.process {
                    if validate_cloud_hypervisor_process(process, &handle.name).is_err() {
                        return Ok(VmState::Stopped);
                    }
                    if !pid_exists(process) {
                        return Ok(VmState::Stopped);
                    }
                } else {
                    return Ok(VmState::Stopped);
                }
                if let Some(ip) = super::check_ready_marker(&handle.serial_log) {
                    return Ok(VmState::Ready { ip });
                }
                return Ok(VmState::Running);
            }
        };

        if process.child.try_wait().map_err(VmError::Io)?.is_some() {
            vms.remove(&handle.name);
            return Ok(VmState::Stopped);
        }

        // Process alive — check serial log for readiness marker
        if let Some(ip) = process.ready.scan(&handle.serial_log) {
            Ok(VmState::Ready { ip })
        } else {
            Ok(VmState::Running)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the cloud-hypervisor binary on PATH or well-known locations.
fn find_ch_binary() -> Result<PathBuf, VmError> {
    // Check PATH first
    match Command::new("cloud-hypervisor")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => return Ok(PathBuf::from("cloud-hypervisor")),
        Ok(status) => {
            tracing::warn!("cloud-hypervisor found on PATH but exited with {}", status);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Expected: binary simply not on PATH, will try well-known locations next
        }
        Err(e) => {
            tracing::warn!("failed to probe cloud-hypervisor on PATH: {}", e);
        }
    }

    // Well-known locations
    for path in &[
        "/usr/bin/cloud-hypervisor",
        "/usr/local/bin/cloud-hypervisor",
    ] {
        if Path::new(path).exists() {
            return Ok(PathBuf::from(path));
        }
    }

    Err(VmError::InvalidConfig(
        "cloud-hypervisor binary not found on PATH or in /usr/bin, /usr/local/bin. \
         Install from https://github.com/cloud-hypervisor/cloud-hypervisor/releases"
            .into(),
    ))
}

/// Generate a deterministic MAC address from a VM name.
/// Uses the QEMU OUI prefix (52:54:00) for locally administered addresses.
fn generate_mac(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(name.as_bytes());
    format!("52:54:00:{:02x}:{:02x}:{:02x}", hash[0], hash[1], hash[2])
}

/// Find the virtiofsd binary on PATH or well-known locations.
fn find_virtiofsd() -> Result<PathBuf, VmError> {
    for name in &["virtiofsd", "/usr/libexec/virtiofsd", "/usr/lib/virtiofsd"] {
        let path = Path::new(name);
        if path.is_absolute() && path.exists() {
            return Ok(path.to_path_buf());
        }
        match Command::new(name)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => return Ok(PathBuf::from(name)),
            Ok(status) => {
                tracing::warn!(
                    "virtiofsd candidate '{}' found but exited with {}",
                    name,
                    status
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Expected: candidate not at this location, try next
            }
            Err(e) => {
                tracing::warn!("failed to probe virtiofsd candidate '{}': {}", name, e);
            }
        }
    }
    Err(VmError::InvalidConfig(
        "virtiofsd not found. Required for VirtioFS shared directories on Linux. \
         Install: apt install virtiofsd (Debian/Ubuntu) or from \
         https://gitlab.com/virtio-fs/virtiofsd"
            .into(),
    ))
}

/// Wait for a Unix socket file to appear on disk.
fn wait_for_socket(path: &Path, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Kill and reap virtiofsd sidecar processes.
fn cleanup_virtiofsd(children: Vec<Child>) {
    for child in children {
        let pid = child.id();
        match wait_for_exit(child, std::time::Duration::from_secs(1)) {
            Ok(()) => tracing::debug!(pid = pid, "virtiofsd exited"),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                tracing::debug!(pid = pid, "virtiofsd required forced termination: {}", e);
            }
            Err(e) => {
                tracing::warn!(pid = pid, error = %e, "failed to clean up virtiofsd sidecar");
            }
        }
    }
}

/// Wait for a tracked child process to exit.
/// If the process doesn't exit within the timeout, escalates to SIGKILL and
/// reports that the graceful stop timed out.
fn wait_for_exit(mut child: Child, timeout: std::time::Duration) -> std::io::Result<()> {
    let start = std::time::Instant::now();
    let pid = child.id();
    while start.elapsed() < timeout {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(pid = pid, %status, "process exited");
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(pid = pid, error = %e, "failed to query child exit status");
                return Err(e);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    tracing::warn!(pid = pid, elapsed_ms = %timeout.as_millis(), "process did not exit within timeout, sending SIGKILL");
    child.kill()?;
    let _status = child.wait()?;
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "PID {} required SIGKILL after waiting {} ms",
            pid,
            timeout.as_millis()
        ),
    ))
}

fn wait_for_pid_exit(
    process: &VmmProcess,
    vm_name: &str,
    timeout: std::time::Duration,
) -> Result<(), VmError> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if !pid_exists(process) {
            tracing::debug!(pid = process.pid(), "process exited");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    tracing::warn!(pid = process.pid(), elapsed_ms = %timeout.as_millis(), "process did not exit within timeout, sending SIGKILL");
    // SAFETY: Sending SIGKILL to a PID we validated still belongs to our process.
    let ret = unsafe { libc::kill(process.pid() as i32, libc::SIGKILL) };
    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(VmError::StopFailed {
            name: vm_name.to_string(),
            detail: format!(
                "failed to SIGKILL restored VM PID {}: {}",
                process.pid(),
                errno
            ),
        });
    }
    // Brief wait for SIGKILL to take effect
    let kill_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < kill_deadline {
        if !pid_exists(process) {
            tracing::debug!(pid = process.pid(), "process exited after SIGKILL");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::error!(pid = process.pid(), "process still alive after SIGKILL");
    Err(VmError::StopFailed {
        name: vm_name.to_string(),
        detail: format!(
            "restored VM PID {} remained alive after SIGKILL",
            process.pid()
        ),
    })
}

fn pid_exists(process: &VmmProcess) -> bool {
    // SAFETY: kill(pid, 0) checks if the process exists without sending a signal.
    if unsafe { libc::kill(process.pid() as i32, 0) } != 0 {
        return false;
    }
    match process.start_time_ticks() {
        Some(expected_start) => read_proc_start_time(process.pid()) == Some(expected_start),
        None => true,
    }
}

fn validate_cloud_hypervisor_process(process: &VmmProcess, vm_name: &str) -> Result<(), VmError> {
    let pid = process.pid();
    let cmdline_path = format!("/proc/{}/cmdline", pid);
    let cmdline = std::fs::read(&cmdline_path).map_err(|e| VmError::StateFailed {
        name: vm_name.to_string(),
        detail: format!("unable to inspect PID {}: {}", pid, e),
    })?;
    if let Some(expected_start) = process.start_time_ticks() {
        let actual_start = read_proc_start_time(pid).ok_or_else(|| VmError::StateFailed {
            name: vm_name.to_string(),
            detail: format!("unable to read start time for PID {}", pid),
        })?;
        if actual_start != expected_start {
            return Err(VmError::StateFailed {
                name: vm_name.to_string(),
                detail: format!("PID {} has been reused by a different process", pid),
            });
        }
    }
    let cmdline = String::from_utf8_lossy(&cmdline);
    if !cmdline.contains("cloud-hypervisor") {
        return Err(VmError::StateFailed {
            name: vm_name.to_string(),
            detail: format!("PID {} is not a cloud-hypervisor process", pid),
        });
    }
    Ok(())
}

fn process_identity(pid: u32, vm_name: &str) -> Result<VmmProcess, VmError> {
    let start_time_ticks = read_proc_start_time(pid).ok_or_else(|| VmError::StateFailed {
        name: vm_name.to_string(),
        detail: format!("unable to read start time for PID {}", pid),
    })?;
    Ok(VmmProcess::new(pid, Some(start_time_ticks)))
}

fn read_proc_start_time(pid: u32) -> Option<u64> {
    let stat_path = format!("/proc/{}/stat", pid);
    let stat = std::fs::read_to_string(stat_path).ok()?;
    let (_, after_comm) = stat.rsplit_once(") ")?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

/// Clean up all TAP devices created for a VM.
fn cleanup_taps(tap_devices: &[String]) {
    for tap in tap_devices {
        let status = Command::new("ip")
            .args(["tuntap", "del", "dev", tap, "mode", "tap"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {
                tracing::debug!(tap = %tap, "TAP device cleaned up");
            }
            Ok(s) => {
                tracing::warn!(tap = %tap, exit = %s, "TAP device cleanup failed (may not exist)");
            }
            Err(e) => {
                tracing::error!(tap = %tap, error = %e, "failed to run ip command for TAP cleanup");
            }
        }
    }
}
