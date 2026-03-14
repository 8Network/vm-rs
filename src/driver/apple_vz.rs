//! macOS driver — Apple Virtualization.framework via virtualization-rs FFI.

use std::fs;
use std::os::unix::io::IntoRawFd;
use std::path::Path;
use std::sync::Mutex;

use block::{Block, ConcreteBlock};
use objc::rc::StrongPtr;

use crate::ffi::apple_vz::{
    base::{dispatch_async, dispatch_queue_create, Id, NSError, NSFileHandle, NIL},
    boot_loader::VZLinuxBootLoaderBuilder,
    entropy_device::VZVirtioEntropyDeviceConfiguration,
    memory_device::VZVirtioTraditionalMemoryBalloonDeviceConfiguration,
    network_device::{
        VZFileHandleNetworkDeviceAttachment, VZMACAddress, VZNATNetworkDeviceAttachment,
        VZVirtioNetworkDeviceConfiguration,
    },
    serial_port::{
        VZFileHandleSerialPortAttachmentBuilder, VZVirtioConsoleDeviceSerialPortConfiguration,
    },
    shared_directory::{
        VZSharedDirectory, VZSingleDirectoryShare, VZVirtioFileSystemDeviceConfiguration,
    },
    storage_device::{
        VZDiskImageCachingMode, VZDiskImageStorageDeviceAttachmentBuilder,
        VZDiskImageSynchronizationMode, VZVirtioBlockDeviceConfiguration,
    },
    virtual_machine::{
        VZVirtualMachine, VZVirtualMachineConfigurationBuilder, VZVirtualMachineState,
    },
};

use crate::config::{NetworkAttachment, VmConfig, VmHandle, VmState};
use crate::driver::{VmDriver, VmError};

/// Apple Virtualization.framework driver for macOS.
pub struct AppleVzDriver {
    vms: Mutex<std::collections::HashMap<String, VZVirtualMachine>>,
}

impl AppleVzDriver {
    pub fn new() -> Self {
        Self {
            vms: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Check if Apple VZ is supported on this machine.
    pub fn is_supported() -> bool {
        VZVirtualMachine::supported()
    }

    fn vm_state(handle: &VmHandle, vm: &VZVirtualMachine) -> VmState {
        // SAFETY: VM references come from the driver's registry and stay alive
        // for the lifetime of the driver entry.
        let ready_ip = super::check_ready_marker(&handle.serial_log);
        Self::map_native_state(unsafe { vm.state() }, ready_ip)
    }

    fn map_native_state(state: VZVirtualMachineState, ready_ip: Option<String>) -> VmState {
        match state {
            VZVirtualMachineState::VZVirtualMachineStateRunning => match ready_ip {
                Some(ip) => VmState::Running { ip },
                None => VmState::Starting,
            },
            VZVirtualMachineState::VZVirtualMachineStatePaused
            | VZVirtualMachineState::VZVirtualMachineStatePausing
            | VZVirtualMachineState::VZVirtualMachineStateResuming => VmState::Paused,
            VZVirtualMachineState::VZVirtualMachineStateStarting => VmState::Starting,
            VZVirtualMachineState::VZVirtualMachineStateStopped => VmState::Stopped,
            VZVirtualMachineState::VZVirtualMachineStateError => VmState::Failed {
                reason: "Apple Virtualization.framework reported an internal error".into(),
            },
            VZVirtualMachineState::Other => VmState::Failed {
                reason: "Apple Virtualization.framework reported an unknown VM state".into(),
            },
        }
    }
}

impl Default for AppleVzDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl VmDriver for AppleVzDriver {
    fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
        if !VZVirtualMachine::supported() {
            return Err(VmError::Hypervisor(
                "Apple Virtualization.framework is not supported on this machine".into(),
            ));
        }

        let name = &config.name;
        let memory_bytes = config.memory_mb * 1024 * 1024;

        // Resolve to absolute paths (required by VZ framework)
        let kernel_abs = resolve_path(&config.kernel, "kernel")?;
        let initrd_abs = config
            .initramfs
            .as_ref()
            .map(|p| resolve_path(p, "initramfs"))
            .transpose()?;
        let disk_abs = config
            .root_disk
            .as_ref()
            .map(|p| resolve_path(p, "root disk"))
            .transpose()?;
        let seed_abs = config
            .seed_iso
            .as_ref()
            .map(|p| resolve_path(p, "seed ISO"))
            .transpose()?;

        // Boot loader — direct Linux boot (fastest path)
        // Apple VZ requires initramfs for Linux boot
        let initrd = initrd_abs.ok_or_else(|| {
            VmError::InvalidConfig("initramfs is required for Apple VZ boot".into())
        })?;
        // Default cmdline depends on boot mode: disk-based vs initramfs-only
        let default_cmdline = if disk_abs.is_some() {
            "console=hvc0 root=/dev/vda1 rw ds=nocloud"
        } else {
            "console=hvc0"
        };
        let cmdline = config.cmdline.as_deref().unwrap_or(default_cmdline);
        let boot_loader = VZLinuxBootLoaderBuilder::new()
            .kernel_url(&kernel_abs)
            .initial_ramdisk_url(&initrd)
            .command_line(cmdline)
            .build();

        // Serial console → per-VM log file
        let log_file = fs::File::create(&config.serial_log).map_err(VmError::Io)?;
        let null_file = fs::File::open("/dev/null").map_err(VmError::Io)?;

        // SAFETY: into_raw_fd() transfers ownership; the fd is valid and open.
        let read_handle = unsafe { NSFileHandle::file_handle_with_fd(null_file.into_raw_fd()) };
        // SAFETY: into_raw_fd() transfers ownership; the fd is valid and open.
        let write_handle = unsafe { NSFileHandle::file_handle_with_fd(log_file.into_raw_fd()) };
        let serial_attachment = VZFileHandleSerialPortAttachmentBuilder::new()
            .file_handle_for_reading(read_handle)
            .file_handle_for_writing(write_handle)
            .build();
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new(serial_attachment);

        // Entropy device (for /dev/random inside VM)
        let entropy = VZVirtioEntropyDeviceConfiguration::new();

        // Memory balloon (dynamic memory management)
        let memory_balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();

        // NIC 1: NAT — internet access + SSH from host
        let nat_attachment = VZNATNetworkDeviceAttachment::new();
        let mut nat_nic = VZVirtioNetworkDeviceConfiguration::new(nat_attachment);
        nat_nic.set_mac_address(VZMACAddress::random_locally_administered_address());
        let mut network_devices: Vec<VZVirtioNetworkDeviceConfiguration> = vec![nat_nic];

        // NIC 2+: FileHandle per network — inter-VM via L2 switch
        for net in &config.networks {
            match net {
                NetworkAttachment::SocketPairFd(fd) => {
                    let socket_fd = fd.try_clone_owned().map_err(VmError::Io)?;
                    // SAFETY: the duplicated file descriptor remains valid for the Objective-C handle.
                    let file_handle =
                        unsafe { NSFileHandle::file_handle_with_fd(socket_fd.into_raw_fd()) };
                    let fh_attachment = VZFileHandleNetworkDeviceAttachment::new(file_handle);
                    let mut nic = VZVirtioNetworkDeviceConfiguration::new(fh_attachment);
                    nic.set_mac_address(VZMACAddress::random_locally_administered_address());
                    network_devices.push(nic);
                }
                NetworkAttachment::Tap { .. } => {
                    return Err(VmError::InvalidConfig(
                        "TAP devices are not supported on macOS; use SocketPairFd".into(),
                    ));
                }
            }
        }

        // Disks: attach only if provided (initramfs boot needs no disks)
        let mut storage_devices: Vec<VZVirtioBlockDeviceConfiguration> = Vec::new();

        if let Some(ref disk_path) = disk_abs {
            let root_attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
                .path(disk_path)
                .read_only(false)
                .caching_mode(VZDiskImageCachingMode::Automatic)
                .sync_mode(VZDiskImageSynchronizationMode::Full)
                .build()
                .map_err(|e| VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to attach root disk: code {}", e.code()),
                })?;
            storage_devices.push(VZVirtioBlockDeviceConfiguration::new(root_attachment));
        }

        if let Some(ref seed_path) = seed_abs {
            let seed_attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
                .path(seed_path)
                .read_only(true)
                .build()
                .map_err(|e| VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to attach seed ISO: code {}", e.code()),
                })?;
            storage_devices.push(VZVirtioBlockDeviceConfiguration::new(seed_attachment));
        }

        // VirtioFS shared directories (volume mounts)
        let mut shared_dirs: Vec<VZVirtioFileSystemDeviceConfiguration> = Vec::new();
        for vol in &config.shared_dirs {
            let host_str = vol
                .host_path
                .to_str()
                .ok_or_else(|| VmError::InvalidConfig("non-UTF8 shared dir path".into()))?;
            let dir = VZSharedDirectory::new(host_str, vol.read_only);
            let share = VZSingleDirectoryShare::new(dir);
            let mut fs_device = VZVirtioFileSystemDeviceConfiguration::new(&vol.tag);
            fs_device.set_share(share);
            shared_dirs.push(fs_device);
        }

        // Assemble VM configuration
        let mut builder = VZVirtualMachineConfigurationBuilder::new()
            .boot_loader(boot_loader)
            .cpu_count(config.cpus)
            .memory_size(memory_bytes)
            .entropy_devices(vec![entropy])
            .memory_balloon_devices(vec![memory_balloon])
            .network_devices(network_devices)
            .serial_ports(vec![serial])
            .storage_devices(storage_devices);

        if !shared_dirs.is_empty() {
            builder = builder.directory_sharing_devices(shared_dirs);
        }

        let vm_config = builder.build();

        // Validate
        vm_config.validate_with_error().map_err(|e| {
            let code = e.code();
            VmError::BootFailed {
                name: name.clone(),
                detail: format!(
                    "VZ configuration validation failed: code {}. \
                     Hint: the binary must be signed with virtualization entitlement",
                    code
                ),
            }
        })?;

        // Create dispatch queue with unique label per VM
        let label =
            std::ffi::CString::new(format!("rs.vm.{}", name)).map_err(|e| VmError::BootFailed {
                name: name.clone(),
                detail: format!("invalid VM name for queue label: {}", e),
            })?;
        // SAFETY: label is a valid null-terminated C string; NIL for serial queue.
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), NIL) };
        let vm = VZVirtualMachine::new(vm_config, queue);

        // Register for lifecycle management before start so the VM stays owned by the driver.
        {
            let mut registry = self
                .vms
                .lock()
                .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
            registry.insert(name.to_string(), vm.clone());
        }

        // Start VM on its dispatch queue with synchronous error reporting via channel
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let name_for_log = name.to_string();
        let name_for_err = name.to_string();
        let vm_for_start = vm.clone();
        let dispatch_block = ConcreteBlock::new(move || {
            tracing::debug!("dispatch block running for VM '{}'", name_for_log);
            let name_err = name_for_err.clone();
            let tx_clone = tx.clone();
            let completion_handler = ConcreteBlock::new(move |err: Id| {
                if err == NIL {
                    tracing::info!("VM '{}' started successfully", name_err);
                    let _ = tx_clone.send(Ok(()));
                } else {
                    // SAFETY: err is a valid NSError pointer from the framework.
                    let error = unsafe { NSError(StrongPtr::retain(err)) };
                    let msg = format!(
                        "code {}: {}",
                        error.code(),
                        error.localized_description().as_str()
                    );
                    tracing::error!("VM '{}' start FAILED {}", name_err, msg);
                    let _ = tx_clone.send(Err(msg));
                }
            });
            let completion_handler = completion_handler.copy();
            let completion_handler: &Block<(Id,), ()> = &completion_handler;
            vm_for_start.start_with_completion_handler(completion_handler);
        });
        let dispatch_block = dispatch_block.copy();
        let dispatch_block: &Block<(), ()> = &dispatch_block;
        // SAFETY: queue is a valid GCD dispatch queue, block is properly retained.
        unsafe {
            dispatch_async(queue, dispatch_block);
        }

        // Wait for the completion handler to fire (up to 10 seconds)
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => {}
            Ok(Err(error_msg)) => {
                self.vms
                    .lock()
                    .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?
                    .remove(name);
                return Err(VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("Apple VZ start failed: {}", error_msg),
                });
            }
            Err(_) => {
                self.vms
                    .lock()
                    .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?
                    .remove(name);
                return Err(VmError::BootFailed {
                    name: name.clone(),
                    detail: "Apple VZ start completion handler did not fire within 10 seconds"
                        .into(),
                });
            }
        }

        Ok(VmHandle {
            name: name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Starting,
            process: None, // Apple VZ runs in-process, no separate VM monitor PID
            serial_log: config.serial_log.clone(),
            machine_id: None,
        })
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::info!(vm = %handle.name, "requesting graceful stop via Apple VZ");
        let vm = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?
            .get(&handle.name)
            .cloned()
            .ok_or_else(|| VmError::NotFound {
                name: handle.name.clone(),
            })?;

        let mut vm_clone = vm.clone();
        // SAFETY: vm_clone is a valid VZVirtualMachine reference from the registry.
        unsafe {
            vm_clone
                .request_stop_with_error()
                .map_err(|e| VmError::StopFailed {
                    name: handle.name.clone(),
                    detail: format!(
                        "error code {}: {}",
                        e.code(),
                        e.localized_description().as_str()
                    ),
                })?;
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match Self::vm_state(handle, &vm) {
                VmState::Stopped => {
                    self.vms
                        .lock()
                        .map_err(|e| {
                            VmError::Hypervisor(format!("VM registry lock poisoned: {}", e))
                        })?
                        .remove(&handle.name);
                    tracing::info!(vm = %handle.name, "Apple VZ VM stopped");
                    return Ok(());
                }
                VmState::Failed { reason } => {
                    self.vms
                        .lock()
                        .map_err(|e| {
                            VmError::Hypervisor(format!("VM registry lock poisoned: {}", e))
                        })?
                        .remove(&handle.name);
                    return Err(VmError::StopFailed {
                        name: handle.name.clone(),
                        detail: reason,
                    });
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(200)),
            }
        }

        Err(VmError::StopFailed {
            name: handle.name.clone(),
            detail: "timed out waiting for Apple VZ VM to stop".into(),
        })
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        tracing::warn!(
            vm = %handle.name,
            "Apple VZ has no force-kill API; attempting graceful stop instead"
        );
        self.stop(handle)
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let registry = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
        let state = match registry.get(&handle.name) {
            Some(vm) => Self::vm_state(handle, vm),
            None => VmState::Stopped,
        };

        tracing::debug!(vm = %handle.name, state = %state, "Apple VZ VM state queried");
        Ok(state)
    }
}

/// Resolve a path to an absolute string (required by Apple VZ framework).
fn resolve_path(path: &Path, label: &str) -> Result<String, VmError> {
    if !path.exists() {
        return Err(VmError::InvalidConfig(format!(
            "{} not found: {}",
            label,
            path.display()
        )));
    }
    fs::canonicalize(path)
        .map_err(VmError::Io)?
        .to_str()
        .ok_or_else(|| VmError::InvalidConfig(format!("non-UTF8 {} path", label)))
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_native_state_maps_to_paused() {
        assert_eq!(
            AppleVzDriver::map_native_state(
                VZVirtualMachineState::VZVirtualMachineStatePaused,
                None,
            ),
            VmState::Paused
        );
    }

    #[test]
    fn pausing_and_resuming_native_states_map_to_paused() {
        assert_eq!(
            AppleVzDriver::map_native_state(
                VZVirtualMachineState::VZVirtualMachineStatePausing,
                None,
            ),
            VmState::Paused
        );
        assert_eq!(
            AppleVzDriver::map_native_state(
                VZVirtualMachineState::VZVirtualMachineStateResuming,
                None,
            ),
            VmState::Paused
        );
    }

    #[test]
    fn running_without_ready_marker_stays_starting() {
        assert_eq!(
            AppleVzDriver::map_native_state(
                VZVirtualMachineState::VZVirtualMachineStateRunning,
                None,
            ),
            VmState::Starting
        );
    }

    #[test]
    fn running_with_ready_marker_maps_to_running() {
        assert_eq!(
            AppleVzDriver::map_native_state(
                VZVirtualMachineState::VZVirtualMachineStateRunning,
                Some("10.0.0.2".into()),
            ),
            VmState::Running {
                ip: "10.0.0.2".into(),
            }
        );
    }
}
