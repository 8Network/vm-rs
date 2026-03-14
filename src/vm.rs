//! VmManager — multi-VM lifecycle orchestration.
//!
//! Auto-selects the correct driver for the current platform:
//! - macOS: AppleVzDriver (Apple Virtualization.framework)
//! - Linux: CloudHvDriver (Cloud Hypervisor REST API)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::config::{VmConfig, VmHandle, VmState};
use crate::driver::{VmDriver, VmError};

/// Manages all VMs on this node. Auto-selects the correct hypervisor driver.
///
/// Thread-safe: uses RwLock for concurrent access to VM state.
pub struct VmManager {
    driver: Box<dyn VmDriver>,
    vms: RwLock<HashMap<String, VmHandle>>,
    base_dir: PathBuf,
}

impl VmManager {
    /// Create a new VmManager with auto-detected driver.
    ///
    /// `base_dir` is the root directory for VM data (disks, logs, configs).
    /// Typically `~/.vm-rs/vms/` for development or `/var/lib/vm-rs/` for production.
    pub fn new(base_dir: PathBuf) -> Result<Self, VmError> {
        let driver = create_platform_driver()?;
        std::fs::create_dir_all(&base_dir).map_err(VmError::Io)?;
        Ok(Self {
            driver,
            vms: RwLock::new(HashMap::new()),
            base_dir,
        })
    }

    /// Create a VmManager with a specific driver (useful for testing).
    pub fn with_driver(driver: Box<dyn VmDriver>, base_dir: PathBuf) -> Result<Self, VmError> {
        std::fs::create_dir_all(&base_dir).map_err(VmError::Io)?;
        Ok(Self {
            driver,
            vms: RwLock::new(HashMap::new()),
            base_dir,
        })
    }

    /// Base directory for all VM data.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Directory for a specific VM's data (logs, disks, seed ISOs).
    pub fn vm_dir(&self, name: &str) -> PathBuf {
        self.base_dir.join(name)
    }

    /// Boot a VM. Creates the VM directory and delegates to the driver.
    pub fn start(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
        config.validate()?;

        // Reserve the name up front so concurrent callers cannot boot the same VM twice.
        {
            let mut vms = self
                .vms
                .write()
                .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
            if let Some(existing) = vms.get(&config.name) {
                if !matches!(existing.state, VmState::Stopped | VmState::Failed { .. }) {
                    return Err(VmError::BootFailed {
                        name: config.name.clone(),
                        detail: format!("VM already exists in state: {}", existing.state),
                    });
                }
            }
            vms.insert(
                config.name.clone(),
                VmHandle {
                    name: config.name.clone(),
                    namespace: config.namespace.clone(),
                    state: VmState::Starting,
                    process: None,
                    serial_log: config.serial_log.clone(),
                    machine_id: None,
                },
            );
        }

        // Ensure VM directory exists
        let vm_dir = self.vm_dir(&config.name);
        if let Err(e) = std::fs::create_dir_all(&vm_dir) {
            let mut vms = self.vms.write()
                .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
            vms.remove(&config.name);
            return Err(VmError::Io(e));
        }

        tracing::info!(
            vm = %config.name,
            cpus = config.cpus,
            memory_mb = config.memory_mb,
            "booting VM"
        );

        let handle = match self.driver.boot(config) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!(vm = %config.name, error = %err, "VM boot failed, cleaning up reservation");
                let mut vms = self
                    .vms
                    .write()
                    .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
                vms.remove(&config.name);
                return Err(err);
            }
        };

        // Track the VM
        {
            let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
            vms.insert(config.name.clone(), handle.clone());
        }

        Ok(handle)
    }

    /// Stop a VM gracefully.
    pub fn stop(&self, name: &str) -> Result<(), VmError> {
        tracing::info!(vm = %name, "stopping VM");
        let handle = self.get_handle(name)?;
        self.driver.stop(&handle)?;

        // Update state
        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Force-kill a VM.
    pub fn kill(&self, name: &str) -> Result<(), VmError> {
        tracing::info!(vm = %name, "force-killing VM");
        let handle = self.get_handle(name)?;
        self.driver.kill(&handle)?;

        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Stop a VM using a pre-built handle (e.g. restored from persisted metadata).
    ///
    /// Use this when the VM was started by a previous daemon process and isn't
    /// tracked in the in-memory map.
    pub fn stop_by_handle(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::info!(vm = %handle.name, "stopping VM by handle");
        self.driver.stop(handle)?;

        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(&handle.name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Force-kill a VM using a pre-built handle (e.g. restored from persisted metadata).
    pub fn kill_by_handle(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::info!(vm = %handle.name, "force-killing VM by handle");
        self.driver.kill(handle)?;

        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(&handle.name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Pause a running VM.
    pub fn pause(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.pause(&handle)?;
        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Paused;
        }
        Ok(())
    }

    /// Resume a paused VM.
    pub fn resume(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.resume(&handle)?;
        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Starting;
        }
        Ok(())
    }

    /// Query current state of a VM.
    pub fn state(&self, name: &str) -> Result<VmState, VmError> {
        let handle = self.get_handle(name)?;
        let state = self.driver.state(&handle)?;

        // Update cached state
        let mut vms = self.vms.write().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = state.clone();
        }
        Ok(state)
    }

    /// Get the IP address of a running VM.
    pub fn get_ip(&self, name: &str) -> Result<Option<String>, VmError> {
        match self.state(name)? {
            VmState::Running { ip } => Ok(Some(ip)),
            _ => Ok(None),
        }
    }

    /// List all tracked VMs.
    pub fn list(&self) -> Result<Vec<VmHandle>, VmError> {
        let vms = self.vms.read().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        Ok(vms.values().cloned().collect())
    }

    /// Wait for all VMs to reach Running state (with timeout).
    pub fn wait_all_ready(&self, timeout_secs: u64) -> Result<(), VmError> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            if start.elapsed() > timeout {
                let pending: Vec<String> = {
                    let vms = self.vms.read().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
                    vms.iter()
                        .filter(|(_, h)| !matches!(h.state, VmState::Running { .. }))
                        .map(|(name, _)| name.clone())
                        .collect()
                };
                return Err(VmError::Hypervisor(format!(
                    "timeout waiting for VMs: {}",
                    pending.join(", ")
                )));
            }

            let mut all_ready = true;
            let names: Vec<String> = {
                let vms = self.vms.read().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
                vms.keys().cloned().collect()
            };

            for name in &names {
                match self.state(name)? {
                    VmState::Running { .. } => {}
                    VmState::Failed { reason } => {
                        return Err(VmError::BootFailed {
                            name: name.clone(),
                            detail: reason,
                        });
                    }
                    _ => {
                        all_ready = false;
                    }
                }
            }

            if all_ready {
                return Ok(());
            }

            {
                let pending: Vec<String> = {
                    let vms = self.vms.read().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
                    vms.iter()
                        .filter(|(_, h)| !matches!(h.state, VmState::Running { .. }))
                        .map(|(name, _)| name.clone())
                        .collect()
                };
                let elapsed = start.elapsed().as_secs();
                if elapsed > 0 && elapsed.is_multiple_of(10) {
                    tracing::info!(pending = ?pending, elapsed_secs = start.elapsed().as_secs(), "waiting for VMs to become ready");
                }
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// Create a disk image as a CoW clone of a base image.
    ///
    /// On macOS: APFS clone (`cp -c`), instant and zero-space.
    /// On Linux: reflink (`cp --reflink=auto`), falls back to regular copy.
    pub fn clone_disk(base: &Path, target: &Path) -> Result<(), VmError> {
        if target.exists() {
            if let (Ok(base_meta), Ok(target_meta)) = (std::fs::metadata(base), std::fs::metadata(target)) {
                if base_meta.len() == target_meta.len() {
                    tracing::debug!(target = %target.display(), "disk clone target already exists with matching size, skipping");
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

        // Ensure parent directory exists
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

    fn get_handle(&self, name: &str) -> Result<VmHandle, VmError> {
        let vms = self.vms.read().map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        vms.get(name)
            .cloned()
            .ok_or_else(|| VmError::NotFound {
                name: name.to_string(),
            })
    }
}

/// Create the platform-appropriate VM driver.
fn create_platform_driver() -> Result<Box<dyn VmDriver>, VmError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::driver::apple_vz::AppleVzDriver::new()))
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(crate::driver::cloud_hv::CloudHvDriver::new()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(VmError::Hypervisor(format!(
            "unsupported platform: {}",
            std::env::consts::OS
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    use super::*;

    struct BlockingDriver {
        boot_calls: AtomicUsize,
        release_rx: Mutex<Option<mpsc::Receiver<()>>>,
    }

    struct FailedStateDriver;

    impl VmDriver for Arc<BlockingDriver> {
        fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
            self.boot_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(rx) = self.release_rx.lock().expect("release lock").take() {
                rx.recv().expect("release boot");
            }
            Ok(VmHandle {
                name: config.name.clone(),
                namespace: config.namespace.clone(),
                state: VmState::Starting,
                process: None,
                serial_log: config.serial_log.clone(),
                machine_id: None,
            })
        }

        fn stop(&self, _handle: &VmHandle) -> Result<(), VmError> {
            Ok(())
        }

        fn kill(&self, _handle: &VmHandle) -> Result<(), VmError> {
            Ok(())
        }

        fn state(&self, _handle: &VmHandle) -> Result<VmState, VmError> {
            Ok(VmState::Stopped)
        }
    }

    impl VmDriver for FailedStateDriver {
        fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
            Ok(VmHandle {
                name: config.name.clone(),
                namespace: config.namespace.clone(),
                state: VmState::Starting,
                process: None,
                serial_log: config.serial_log.clone(),
                machine_id: None,
            })
        }

        fn stop(&self, _handle: &VmHandle) -> Result<(), VmError> {
            Ok(())
        }

        fn kill(&self, _handle: &VmHandle) -> Result<(), VmError> {
            Ok(())
        }

        fn state(&self, _handle: &VmHandle) -> Result<VmState, VmError> {
            Ok(VmState::Failed {
                reason: "crashed".into(),
            })
        }
    }

    fn test_config(base_dir: &Path) -> VmConfig {
        VmConfig {
            name: "test-vm".into(),
            namespace: "tests".into(),
            kernel: PathBuf::from("/tmp/kernel"),
            initramfs: None,
            root_disk: None,
            data_disk: None,
            seed_iso: None,
            cpus: 1,
            memory_mb: 256,
            networks: vec![],
            shared_dirs: vec![],
            serial_log: base_dir.join("serial.log"),
            cmdline: None,
            netns: None,
            vsock: false,
            machine_id: None,
            efi_variable_store: None,
            rosetta: false,
        }
    }

    #[test]
    fn start_reserves_name_before_driver_boot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (release_tx, release_rx) = mpsc::channel();
        let driver = Arc::new(BlockingDriver {
            boot_calls: AtomicUsize::new(0),
            release_rx: Mutex::new(Some(release_rx)),
        });
        let manager = Arc::new(
            VmManager::with_driver(Box::new(driver.clone()), tmp.path().to_path_buf())
                .expect("manager"),
        );
        let config = test_config(tmp.path());

        let manager_clone = Arc::clone(&manager);
        let config_clone = config.clone();
        let boot_thread = std::thread::spawn(move || manager_clone.start(&config_clone));

        while driver.boot_calls.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let err = manager.start(&config).expect_err("second concurrent start should fail");
        assert!(err.to_string().contains("already exists"));

        release_tx.send(()).expect("release first boot");
        boot_thread.join().expect("join").expect("first boot should succeed");

        assert_eq!(driver.boot_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn restart_is_allowed_after_failed_state_is_observed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = VmManager::with_driver(Box::new(FailedStateDriver), tmp.path().to_path_buf())
            .expect("manager");
        let config = test_config(tmp.path());

        manager.start(&config).expect("first boot");
        let state = manager.state(&config.name).expect("state query");
        assert!(matches!(state, VmState::Failed { .. }));

        manager.start(&config).expect("restart after failed state should be allowed");
    }
}
