//! Linux driver — Cloud Hypervisor via CLI.
//!
//! Spawns the `cloud-hypervisor` binary with CLI arguments for VM boot.
//! Process lifecycle for stop/kill. No API socket needed for basic operations.
//!
//! For advanced features (hotplug, resize, snapshot), we'll add API socket mode
//! via `ch-remote` — Cloud Hypervisor's own CLI tool. See CAPABILITIES.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use crate::config::{NetworkAttachment, VmConfig, VmHandle, VmState};
use crate::driver::{VmDriver, VmError};

// ---------------------------------------------------------------------------
// Internal VM tracking
// ---------------------------------------------------------------------------

/// Metadata for a running Cloud Hypervisor VM process.
struct VmProcess {
    /// PID of the cloud-hypervisor process.
    pid: u32,
    /// TAP device name (for cleanup on stop).
    tap_device: Option<String>,
    /// virtiofsd sidecar PIDs (for VirtioFS shared dirs, cleaned up on stop).
    virtiofsd_pids: Vec<u32>,
}

/// Cloud Hypervisor driver for Linux.
///
/// Boots VMs via `cloud-hypervisor` CLI args. Manages process lifecycle
/// with signals: SIGTERM for graceful shutdown, SIGKILL for force-kill.
pub struct CloudHvDriver {
    vms: Mutex<HashMap<String, VmProcess>>,
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
            cmd.arg("--memory").arg(format!("size={}M", config.memory_mb));
        } else {
            cmd.arg("--memory").arg(format!("size={}M,shared=on", config.memory_mb));
        }

        // Disks: only attach if provided (initramfs boot needs no disks)
        if let Some(ref root_disk) = config.root_disk {
            cmd.arg("--disk").arg(format!("path={}", root_disk.display()));
        }
        if let Some(ref seed_iso) = config.seed_iso {
            cmd.arg("--disk").arg(format!("path={},readonly=on", seed_iso.display()));
        }
        if let Some(ref data_disk) = config.data_disk {
            cmd.arg("--disk").arg(format!("path={}", data_disk.display()));
        }

        // Network
        let mut tap_name = None;
        for net in &config.networks {
            match net {
                NetworkAttachment::Tap { name: tap, mac } => {
                    tap_name = Some(tap.clone());
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
        cmd.arg("--serial").arg(format!("file={}", config.serial_log.display()));
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
                .arg("--sandbox=none")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to spawn virtiofsd for '{}': {}", vol.tag, e),
                })?;

            tracing::info!(
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
                for mut c in virtiofsd_children {
                    let _ = c.kill();
                }
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
        let vmm_log = std::fs::File::create(&vmm_log_path).map_err(|e| VmError::BootFailed {
            name: name.clone(),
            detail: format!("failed to create VMM log file: {}", e),
        })?;
        let vmm_log_stderr = vmm_log.try_clone().map_err(|e| VmError::Io(e))?;
        let process = cmd
            .stdout(vmm_log)
            .stderr(vmm_log_stderr)
            .spawn()
            .map_err(|e| VmError::BootFailed {
                name: name.clone(),
                detail: format!("failed to spawn cloud-hypervisor: {}", e),
            })?;

        let pid = process.id();
        tracing::info!(vm = %name, pid = pid, "Cloud Hypervisor process started");

        // Collect virtiofsd PIDs for cleanup, then drop the Child handles.
        // Dropping Child on Unix does NOT kill the process — it only closes our
        // handle. The virtiofsd processes keep running and are killed via raw
        // kill() in stop/cleanup using the saved PIDs.
        let virtiofsd_pids: Vec<u32> = virtiofsd_children.iter().map(|c| c.id()).collect();
        drop(virtiofsd_children);

        // Brief pause then check if process exited immediately (bad binary, permissions, etc.)
        std::thread::sleep(std::time::Duration::from_millis(100));
        // SAFETY: kill(pid, 0) checks if process exists without sending a signal.
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive {
            // Clean up virtiofsd processes since VM failed
            cleanup_virtiofsd(&virtiofsd_pids);
            return Err(VmError::BootFailed {
                name: name.clone(),
                detail: format!(
                    "cloud-hypervisor process (PID {}) exited immediately. Check {}",
                    pid, vmm_log_path.display()
                ),
            });
        }

        // Track
        {
            let mut vms = self.vms.lock().map_err(|e| {
                VmError::Hypervisor(format!("lock poisoned: {}", e))
            })?;
            vms.insert(name.clone(), VmProcess { pid, tap_device: tap_name, virtiofsd_pids });
        }

        Ok(VmHandle {
            name: name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Starting,
            pid: Some(pid),
            serial_log: config.serial_log.clone(),
        })
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self.vms.lock().map_err(|e| {
            VmError::Hypervisor(format!("lock poisoned: {}", e))
        })?;
        let process = vms.remove(&handle.name).ok_or_else(|| VmError::NotFound {
            name: handle.name.clone(),
        })?;

        // SIGTERM → Cloud Hypervisor handles graceful ACPI shutdown
        // SAFETY: Sending SIGTERM to a PID we spawned. PID validity confirmed by prior operations.
        let ret = unsafe { libc::kill(process.pid as i32, libc::SIGTERM) };
        if ret != 0 {
            let errno = std::io::Error::last_os_error();
            tracing::warn!(
                vm = %handle.name,
                pid = process.pid,
                error = %errno,
                "SIGTERM failed (process may already be stopped)"
            );
        } else {
            // Wait for process to exit (up to 10s)
            wait_for_exit(process.pid, std::time::Duration::from_secs(10));
        }

        cleanup_tap(&process.tap_device);
        cleanup_virtiofsd(&process.virtiofsd_pids);
        Ok(())
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self.vms.lock().map_err(|e| {
            VmError::Hypervisor(format!("lock poisoned: {}", e))
        })?;

        let (pid, virtiofsd_pids) = if let Some(process) = vms.remove(&handle.name) {
            cleanup_tap(&process.tap_device);
            (process.pid, process.virtiofsd_pids)
        } else if let Some(pid) = handle.pid {
            (pid, Vec::new())
        } else {
            return Err(VmError::NotFound { name: handle.name.clone() });
        };

        // SAFETY: Sending SIGKILL to a PID we spawned.
        let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if ret != 0 {
            let errno = std::io::Error::last_os_error();
            tracing::warn!(
                vm = %handle.name,
                pid = pid,
                error = %errno,
                "SIGKILL failed (process may already be stopped)"
            );
        }

        cleanup_virtiofsd(&virtiofsd_pids);
        Ok(())
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let vms = self.vms.lock().map_err(|e| {
            VmError::Hypervisor(format!("lock poisoned: {}", e))
        })?;

        let process = match vms.get(&handle.name) {
            Some(p) => p,
            None => return Ok(VmState::Stopped),
        };

        // SAFETY: kill(pid, 0) checks if process exists without sending a signal.
        let alive = unsafe { libc::kill(process.pid as i32, 0) } == 0;
        if !alive {
            return Ok(VmState::Stopped);
        }

        // Process alive — check serial log for readiness marker
        if let Some(ip) = check_ready_marker(&handle.serial_log) {
            Ok(VmState::Running { ip })
        } else {
            Ok(VmState::Starting)
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
         Install from https://github.com/cloud-hypervisor/cloud-hypervisor/releases".into(),
    ))
}

/// Generate a deterministic MAC address from a VM name.
/// Uses the QEMU OUI prefix (52:54:00) for locally administered addresses.
fn generate_mac(name: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let hash = hasher.finish();

    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        (hash >> 16) as u8,
        (hash >> 8) as u8,
        hash as u8
    )
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
                tracing::warn!("virtiofsd candidate '{}' found but exited with {}", name, status);
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

/// Kill virtiofsd sidecar processes.
fn cleanup_virtiofsd(pids: &[u32]) {
    for &pid in pids {
        // SAFETY: Sending SIGKILL to PIDs we spawned.
        let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if ret == 0 {
            tracing::debug!(pid = pid, "virtiofsd killed");
        }
    }
}

/// Check the serial console log for the readiness marker.
/// The guest writes `VMRS_READY <ip>` when boot completes.
fn check_ready_marker(log_path: &Path) -> Option<String> {
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

/// Wait for a process to exit, polling with kill(pid, 0).
/// If the process doesn't exit within the timeout, escalates to SIGKILL.
fn wait_for_exit(pid: u32, timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        // SAFETY: kill(pid, 0) only checks if the process exists.
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive {
            tracing::debug!(pid = pid, "process exited cleanly");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    tracing::warn!(pid = pid, elapsed_ms = %timeout.as_millis(), "process did not exit within timeout, sending SIGKILL");
    // SAFETY: Sending SIGKILL to a PID we spawned and confirmed still exists.
    let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        tracing::warn!(pid = pid, error = %errno, "SIGKILL failed (process may have exited between check and kill)");
    }
}

/// Clean up a TAP device if one was created.
fn cleanup_tap(tap_device: &Option<String>) {
    if let Some(ref tap) = tap_device {
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
