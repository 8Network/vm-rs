//! macOS driver — Apple Virtualization.framework via virtualization-rs FFI.
//!
//! Uses Box::leak to keep VZVirtualMachine alive (Objective-C reference counting
//! requires the object to outlive the dispatch queue).

use std::collections::HashMap;
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

/// Global registry of leaked VM references, keyed by VM name.
/// Required because Apple VZ needs the VZVirtualMachine object to stay alive
/// for the VM to keep running. Box::leak prevents Rust from dropping it.
static VM_REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, &'static VZVirtualMachine>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Apple Virtualization.framework driver for macOS.
pub struct AppleVzDriver;

impl Default for AppleVzDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleVzDriver {
    pub fn new() -> Self {
        AppleVzDriver
    }

    /// Check if Apple VZ is supported on this machine.
    pub fn is_supported() -> bool {
        VZVirtualMachine::supported()
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

        // SAFETY: into_raw_fd() transfers ownership to the NSFileHandle (closeOnDealloc: YES).
        let read_handle =
            unsafe { NSFileHandle::file_handle_with_fd_owned(null_file.into_raw_fd()) };
        // SAFETY: into_raw_fd() transfers ownership to the NSFileHandle (closeOnDealloc: YES).
        let write_handle =
            unsafe { NSFileHandle::file_handle_with_fd_owned(log_file.into_raw_fd()) };
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
                    // SAFETY: fd comes from a socketpair managed by the NetworkSwitch.
                    // The switch owns the fd lifetime, so we borrow (closeOnDealloc: NO).
                    let file_handle = unsafe { NSFileHandle::file_handle_with_fd_borrowed(*fd) };
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

        // Box::leak: keep the VZVirtualMachine alive for the lifetime of the process.
        // Without this, Rust drops the VM object, Objective-C releases the underlying
        // VZVirtualMachine, and the VM dies immediately.
        let vm_leaked: &'static VZVirtualMachine = Box::leak(Box::new(vm));

        // Register for graceful shutdown
        {
            let mut registry = VM_REGISTRY
                .lock()
                .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
            registry.insert(name.to_string(), vm_leaked);
        }

        // Start VM on its dispatch queue
        let name_for_log = name.to_string();
        let name_for_err = name.to_string();
        let dispatch_block = ConcreteBlock::new(move || {
            tracing::debug!("dispatch block running for VM '{}'", name_for_log);
            let name_err = name_for_err.clone();
            let completion_handler = ConcreteBlock::new(move |err: Id| {
                if err == NIL {
                    tracing::info!("VM '{}' started successfully", name_err);
                } else {
                    // SAFETY: err is a valid NSError pointer from the framework.
                    let error = unsafe { NSError(StrongPtr::retain(err)) };
                    let code = error.code();
                    tracing::error!("VM '{}' start FAILED (error code {})", name_err, code);
                }
            });
            let completion_handler = completion_handler.copy();
            let completion_handler: &Block<(Id,), ()> = &completion_handler;
            vm_leaked.start_with_completion_handler(completion_handler);
        });
        let dispatch_block = dispatch_block.copy();
        let dispatch_block: &Block<(), ()> = &dispatch_block;
        // SAFETY: queue is a valid GCD dispatch queue, block is properly retained.
        unsafe {
            dispatch_async(queue, dispatch_block);
        }

        // Give dispatch queue a moment to fire
        std::thread::sleep(std::time::Duration::from_millis(500));

        Ok(VmHandle {
            name: name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Starting,
            pid: None, // Apple VZ runs in-process, no separate PID
            serial_log: config.serial_log.clone(),
        })
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        let registry = VM_REGISTRY
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
        let vm = registry
            .get(&handle.name)
            .ok_or_else(|| VmError::NotFound {
                name: handle.name.clone(),
            })?;

        let mut vm_clone = (*vm).clone();
        // SAFETY: vm_clone is a valid VZVirtualMachine reference from the registry.
        unsafe {
            vm_clone
                .request_stop_with_error()
                .map_err(|e| VmError::StopFailed {
                    name: handle.name.clone(),
                    detail: format!("error code {}", e.code()),
                })?;
        }

        Ok(())
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        // Apple VZ doesn't have a force-kill API separate from stop.
        // Remove from registry — the leaked reference stays but the VM is abandoned.
        let mut registry = VM_REGISTRY
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
        registry.remove(&handle.name);
        tracing::warn!("force-killed VM '{}' (removed from registry)", handle.name);
        Ok(())
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let registry = VM_REGISTRY
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
        let vm = match registry.get(&handle.name) {
            Some(vm) => *vm,
            None => return Ok(VmState::Stopped),
        };

        // Query actual VZ framework state instead of assuming Running
        // SAFETY: state() sends an ObjC message on the VM object we hold in the registry.
        let vz_state = unsafe { vm.state() };
        match vz_state {
            VZVirtualMachineState::VZVirtualMachineStateStopped
            | VZVirtualMachineState::VZVirtualMachineStateError => Ok(VmState::Stopped),
            VZVirtualMachineState::VZVirtualMachineStateStarting => Ok(VmState::Starting),
            VZVirtualMachineState::VZVirtualMachineStatePaused
            | VZVirtualMachineState::VZVirtualMachineStatePausing
            | VZVirtualMachineState::VZVirtualMachineStateResuming => {
                // Report paused states as Starting (we don't have a Paused variant)
                Ok(VmState::Starting)
            }
            VZVirtualMachineState::VZVirtualMachineStateRunning => {
                // VZ says Running — check console log for readiness marker with IP
                if let Some(ip) = check_ready_marker(&handle.serial_log) {
                    Ok(VmState::Running { ip })
                } else {
                    Ok(VmState::Starting)
                }
            }
            VZVirtualMachineState::Other => Ok(VmState::Starting),
        }
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

/// Check the console log for the readiness marker and extract the IP.
///
/// The init script writes `VMRS_READY <ip>` to the console
/// when the VM is fully booted and ready for connections.
fn check_ready_marker(log_path: &Path) -> Option<String> {
    let content = match fs::read_to_string(log_path) {
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

/// Watch a console log file for the VM readiness marker.
///
/// Blocks until either the marker is found or the stop flag is set.
/// Returns the IP address if found.
pub fn watch_for_ready(log_path: &Path, stop: &std::sync::atomic::AtomicBool) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::Ordering;

    let mut last_size = 0u64;
    let mut accumulated = String::new();

    while !stop.load(Ordering::Relaxed) {
        let file = match fs::File::open(log_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Log file not created yet — normal during early boot
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            Err(e) => {
                tracing::warn!(path = %log_path.display(), "failed to open serial log: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let metadata = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(path = %log_path.display(), "failed to read serial log metadata: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let current_size = metadata.len();
        if current_size > last_size {
            let mut file = file;
            if let Err(e) = file.seek(SeekFrom::Start(last_size)) {
                tracing::warn!(path = %log_path.display(), "failed to seek serial log: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            let mut buf = vec![0u8; (current_size - last_size) as usize];
            match file.read(&mut buf) {
                Ok(n) => accumulated.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) => {
                    tracing::warn!(path = %log_path.display(), "failed to read serial log: {}", e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            }
            last_size = current_size;

            if let Some(pos) = accumulated.find(crate::config::READY_MARKER) {
                let after = &accumulated[pos + crate::config::READY_MARKER.len()..];
                if let Some(ip) = after.split_whitespace().next() {
                    let ip = ip.trim().to_string();
                    if !ip.is_empty() {
                        return Some(ip);
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    None
}
