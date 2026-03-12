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
    ///
    /// Holds a write lock across the duplicate check AND boot to prevent
    /// two concurrent callers from booting the same VM name.
    pub fn start(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
        // Take write lock for the entire operation: check + boot + insert.
        // This prevents a TOCTOU race where two callers both pass the
        // duplicate check and boot separate VMs with the same name.
        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;

        if let Some(existing) = vms.get(&config.name) {
            if existing.state != VmState::Stopped {
                return Err(VmError::BootFailed {
                    name: config.name.clone(),
                    detail: format!("VM already exists in state: {}", existing.state),
                });
            }
        }

        // Reserve the name with a Starting entry before boot.
        // If boot fails, we remove it.
        let placeholder = VmHandle {
            name: config.name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Starting,
            pid: None,
            serial_log: config.serial_log.clone(),
            machine_id: None,
        };
        vms.insert(config.name.clone(), placeholder);

        // Release the lock during the actual boot (which may take time)
        drop(vms);

        // Ensure VM directory exists
        let vm_dir = self.vm_dir(&config.name);
        std::fs::create_dir_all(&vm_dir).map_err(VmError::Io)?;

        tracing::info!(
            vm = %config.name,
            cpus = config.cpus,
            memory_mb = config.memory_mb,
            "booting VM"
        );

        match self.driver.boot(config) {
            Ok(handle) => {
                let mut vms = self
                    .vms
                    .write()
                    .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
                vms.insert(config.name.clone(), handle.clone());
                Ok(handle)
            }
            Err(e) => {
                // Boot failed — remove the placeholder
                if let Ok(mut vms) = self.vms.write() {
                    vms.remove(&config.name);
                }
                Err(e)
            }
        }
    }

    /// Stop a VM gracefully.
    pub fn stop(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.stop(&handle)?;

        // Update state
        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Force-kill a VM.
    pub fn kill(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.kill(&handle)?;

        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
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
        self.driver.stop(handle)?;

        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(&handle.name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Force-kill a VM using a pre-built handle (e.g. restored from persisted metadata).
    pub fn kill_by_handle(&self, handle: &VmHandle) -> Result<(), VmError> {
        self.driver.kill(handle)?;

        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(&handle.name) {
            h.state = VmState::Stopped;
        }
        Ok(())
    }

    /// Query current state of a VM.
    pub fn state(&self, name: &str) -> Result<VmState, VmError> {
        let handle = self.get_handle(name)?;
        let state = self.driver.state(&handle)?;

        // Update cached state
        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = state.clone();
        }
        Ok(state)
    }

    /// Pause a running VM (suspends execution, preserves memory).
    pub fn pause(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.pause(&handle)?;

        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Paused;
        }
        Ok(())
    }

    /// Resume a paused VM.
    pub fn resume(&self, name: &str) -> Result<(), VmError> {
        let handle = self.get_handle(name)?;
        self.driver.resume(&handle)?;

        let mut vms = self
            .vms
            .write()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        if let Some(h) = vms.get_mut(name) {
            h.state = VmState::Starting; // Will transition to Running once ready marker appears
        }
        Ok(())
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
        let vms = self
            .vms
            .read()
            .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
        Ok(vms.values().cloned().collect())
    }

    /// Wait for all VMs to reach Running state (with timeout).
    pub fn wait_all_ready(&self, timeout_secs: u64) -> Result<(), VmError> {
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);

        loop {
            if start.elapsed() > timeout {
                let pending: Vec<String> = {
                    let vms = self
                        .vms
                        .read()
                        .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
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
                let vms = self
                    .vms
                    .read()
                    .map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))?;
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

            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    /// Create a disk image as a CoW clone of a base image.
    ///
    /// On macOS: APFS clone (`cp -c`), instant and zero-space.
    /// On Linux: reflink (`cp --reflink=auto`), falls back to regular copy.
    pub fn clone_disk(base: &Path, target: &Path) -> Result<(), VmError> {
        if target.exists() {
            tracing::debug!(target = %target.display(), "disk clone target already exists, skipping");
            return Ok(());
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
                // Fallback to regular copy if APFS clone not supported
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
                std::fs::copy(base, target).map_err(VmError::Io)?;
            }
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            // No CoW support — plain copy
            std::fs::copy(base, target).map_err(VmError::Io)?;
        }

        Ok(())
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

    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::driver::whp::WhpDriver::new()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(VmError::Hypervisor(format!(
            "unsupported platform: {}",
            std::env::consts::OS
        )))
    }
}
