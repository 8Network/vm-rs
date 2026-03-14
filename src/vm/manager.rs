use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use super::disk;
use super::platform::create_platform_driver;
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

        let vm_dir = self.vm_dir(&config.name);
        if let Err(e) = std::fs::create_dir_all(&vm_dir) {
            let mut vms = self
                .vms
                .write()
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

        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        vms.insert(config.name.clone(), handle.clone());

        Ok(handle)
    }

    /// Stop a VM gracefully.
    pub fn stop(&self, name: &str) -> Result<(), VmError> {
        tracing::info!(vm = %name, "stopping VM");
        let handle = self.get_handle(name)?;
        self.driver.stop(&handle)?;
        self.update_cached_state(name, VmState::Stopped)
    }

    /// Force-kill a VM.
    pub fn kill(&self, name: &str) -> Result<(), VmError> {
        tracing::info!(vm = %name, "force-killing VM");
        let handle = self.get_handle(name)?;
        self.driver.kill(&handle)?;
        self.update_cached_state(name, VmState::Stopped)
    }

    /// Stop a VM using a pre-built handle (e.g. restored from persisted metadata).
    pub fn stop_by_handle(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::info!(vm = %handle.name, "stopping VM by handle");
        self.driver.stop(handle)?;
        self.update_cached_state(&handle.name, VmState::Stopped)
    }

    /// Force-kill a VM using a pre-built handle (e.g. restored from persisted metadata).
    pub fn kill_by_handle(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::info!(vm = %handle.name, "force-killing VM by handle");
        self.driver.kill(handle)?;
        self.update_cached_state(&handle.name, VmState::Stopped)
    }

    /// Pause a running VM.
    pub fn pause(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.pause(&handle)?;
        self.update_cached_state(name, VmState::Paused)
    }

    /// Resume a paused VM.
    pub fn resume(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.resume(&handle)?;
        let resumed_state = self.driver.state(&handle)?;
        self.update_cached_state(name, resumed_state)
    }

    /// Query current state of a VM.
    pub fn state(&self, name: &str) -> Result<VmState, VmError> {
        let handle = self.get_handle(name)?;
        let state = self.driver.state(&handle)?;
        self.update_cached_state(name, state.clone())?;
        Ok(state)
    }

    /// Get the IP address of a ready VM.
    pub fn get_ip(&self, name: &str) -> Result<Option<String>, VmError> {
        Ok(self.state(name)?.ip().map(ToOwned::to_owned))
    }

    /// List all tracked VMs.
    pub fn list(&self) -> Result<Vec<VmHandle>, VmError> {
        let vms = self
            .vms
            .read()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        Ok(vms.values().cloned().collect())
    }

    /// Wait for all VMs to emit the readiness marker within the timeout.
    pub fn wait_all_ready(&self, timeout_secs: u64) -> Result<(), VmError> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            if start.elapsed() > timeout {
                let pending = self.pending_names(|state| state.is_ready())?;
                return Err(VmError::Hypervisor(format!(
                    "timeout waiting for VMs: {}",
                    pending.join(", ")
                )));
            }

            let mut all_ready = true;
            let names = self.vm_names()?;

            for name in &names {
                match self.state(name)? {
                    state if state.is_ready() => {}
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

            let elapsed = start.elapsed().as_secs();
            if elapsed > 0 && elapsed.is_multiple_of(10) {
                let pending = self.pending_names(|state| state.is_ready())?;
                tracing::info!(
                    pending = ?pending,
                    elapsed_secs = elapsed,
                    "waiting for VMs to become ready"
                );
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// Create a disk image as a CoW clone of a base image.
    pub fn clone_disk(base: &Path, target: &Path) -> Result<(), VmError> {
        disk::clone_disk(base, target)
    }

    fn get_handle(&self, name: &str) -> Result<VmHandle, VmError> {
        let vms = self
            .vms
            .read()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        vms.get(name).cloned().ok_or_else(|| VmError::NotFound {
            name: name.to_string(),
        })
    }

    fn update_cached_state(&self, name: &str, state: VmState) -> Result<(), VmError> {
        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(handle) = vms.get_mut(name) {
            handle.state = state;
        }
        Ok(())
    }

    fn vm_names(&self) -> Result<Vec<String>, VmError> {
        let vms = self
            .vms
            .read()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        Ok(vms.keys().cloned().collect())
    }

    fn pending_names(&self, predicate: impl Fn(&VmState) -> bool) -> Result<Vec<String>, VmError> {
        let vms = self
            .vms
            .read()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        Ok(vms
            .iter()
            .filter(|(_, handle)| !predicate(&handle.state))
            .map(|(name, _)| name.clone())
            .collect())
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
    struct ReadyAfterTwoPollsDriver {
        polls: AtomicUsize,
    }

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

    impl VmDriver for ReadyAfterTwoPollsDriver {
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
            let poll = self.polls.fetch_add(1, Ordering::SeqCst);
            if poll == 0 {
                Ok(VmState::Running)
            } else {
                Ok(VmState::Ready {
                    ip: "10.0.0.2".into(),
                })
            }
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

        let err = manager
            .start(&config)
            .expect_err("second concurrent start should fail");
        assert!(err.to_string().contains("already exists"));

        release_tx.send(()).expect("release first boot");
        boot_thread
            .join()
            .expect("join")
            .expect("first boot should succeed");

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

        manager
            .start(&config)
            .expect("restart after failed state should be allowed");
    }

    #[test]
    fn wait_all_ready_waits_for_ready_not_just_running() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manager = VmManager::with_driver(
            Box::new(ReadyAfterTwoPollsDriver {
                polls: AtomicUsize::new(0),
            }),
            tmp.path().to_path_buf(),
        )
        .expect("manager");
        let config = test_config(tmp.path());

        manager.start(&config).expect("boot");
        manager
            .wait_all_ready(2)
            .expect("wait_all_ready should wait until ready");
        assert_eq!(
            manager.get_ip(&config.name).expect("ip query"),
            Some("10.0.0.2".into())
        );
    }
}
