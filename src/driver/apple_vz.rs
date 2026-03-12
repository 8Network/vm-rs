//! macOS driver — Apple Virtualization.framework via virtualization-rs FFI.
//!
//! # Thread Safety
//!
//! All Objective-C / VZ framework calls are dispatched to a per-VM GCD serial
//! queue. The calling thread (which may be a Rust test runner thread) NEVER
//! makes ObjC calls directly. This prevents ObjC autoreleased objects from
//! accumulating in the caller's thread-local storage (TLS) autorelease pool,
//! which would cause SIGSEGV in `_pthread_tsd_cleanup` when the thread exits.
//!
//! Uses `Pin<Box<>>` to keep `VZVirtualMachine` alive with a stable address
//! (Objective-C reference counting requires the object to outlive the dispatch
//! queue). Removing the VM from the registry drops the `Pin<Box<>>`.

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::IntoRawFd;
use std::path::Path;
use std::sync::Mutex;

use block::{Block, ConcreteBlock};
use objc::rc::StrongPtr;

use crate::ffi::apple_vz::{
    base::{dispatch_async, dispatch_queue_create, Id, NSError, NSFileHandle, NIL},
    boot_loader::{VZEFIBootLoader, VZEFIVariableStore, VZLinuxBootLoaderBuilder},
    entropy_device::VZVirtioEntropyDeviceConfiguration,
    memory_device::VZVirtioTraditionalMemoryBalloonDeviceConfiguration,
    network_device::{
        VZFileHandleNetworkDeviceAttachment, VZMACAddress, VZNATNetworkDeviceAttachment,
        VZVirtioNetworkDeviceConfiguration,
    },
    platform::{VZGenericMachineIdentifier, VZGenericPlatformConfiguration},
    serial_port::{
        VZFileHandleSerialPortAttachmentBuilder, VZVirtioConsoleDeviceSerialPortConfiguration,
    },
    shared_directory::{
        VZLinuxRosettaAvailability, VZLinuxRosettaDirectoryShare, VZSharedDirectory,
        VZSingleDirectoryShare, VZVirtioFileSystemDeviceConfiguration,
    },
    socket_device::VZVirtioSocketDeviceConfiguration,
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

/// Per-VM state stored in the global registry.
///
/// The `VZVirtualMachine` must stay alive for the VM to keep running.
/// We store it in a `Pin<Box<>>` to prevent moves and ensure stable
/// memory addresses for ObjC reference counting.
struct VmEntry {
    vm: std::pin::Pin<Box<VZVirtualMachine>>,
    queue: Id, // GCD dispatch queue for this VM
}

// SAFETY: Id (raw pointer to ObjC object) is Send — the dispatch queue is
// retained by GCD and safe to reference from any thread. We only use it
// via dispatch_sync/dispatch_async which are thread-safe by design.
unsafe impl Send for VmEntry {}

/// Global registry of leaked VM references, keyed by VM name.
/// Required because Apple VZ needs the VZVirtualMachine object to stay alive
/// for the VM to keep running. Box::leak prevents Rust from dropping it.
static VM_REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, VmEntry>>> =
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
        // ── Step 1: Pre-validate and resolve paths (pure Rust, no ObjC) ──

        let name = config.name.clone();
        let namespace = config.namespace.clone();
        let serial_log = config.serial_log.clone();
        let memory_bytes = config.memory_mb * 1024 * 1024;
        let cpus = config.cpus;

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

        let use_uefi = config.efi_variable_store.is_some();

        // For direct Linux boot, initramfs is required
        if !use_uefi && initrd_abs.is_none() {
            return Err(VmError::InvalidConfig(
                "initramfs is required for direct Linux boot on Apple VZ".into(),
            ));
        }

        let initrd = initrd_abs;

        let default_cmdline = if disk_abs.is_some() {
            "console=hvc0 root=/dev/vda1 rw ds=nocloud"
        } else {
            "console=hvc0"
        };
        let cmdline = config
            .cmdline
            .clone()
            .unwrap_or_else(|| default_cmdline.to_string());

        // Validate network config (reject unsupported types before dispatching)
        for net in &config.networks {
            if matches!(net, NetworkAttachment::Tap { .. }) {
                return Err(VmError::InvalidConfig(
                    "TAP devices are not supported on macOS; use SocketPairFd".into(),
                ));
            }
        }
        let networks = config.networks.clone();
        let shared_dirs = config.shared_dirs.clone();

        // Open file descriptors (pure Rust syscalls, no ObjC)
        let log_fd = fs::File::create(&config.serial_log)
            .map_err(VmError::Io)?
            .into_raw_fd();
        let null_fd = fs::File::open("/dev/null")
            .map_err(VmError::Io)?
            .into_raw_fd();

        // New capability flags
        let enable_vsock = config.vsock;
        let enable_rosetta = config.rosetta;
        let machine_id_bytes = config.machine_id.clone();
        let efi_store_path = config.efi_variable_store.clone();

        // ── Step 2: Create GCD queue (C function, no ObjC autorelease) ──

        let label =
            std::ffi::CString::new(format!("rs.vm.{}", name)).map_err(|e| VmError::BootFailed {
                name: name.clone(),
                detail: format!("invalid VM name for queue label: {}", e),
            })?;
        // SAFETY: label is a valid null-terminated C string; NIL for serial queue.
        let queue = unsafe { dispatch_queue_create(label.as_ptr(), NIL) };

        // ── Step 3: Dispatch ALL ObjC/VZ work to the GCD queue ──
        //
        // GCD manages autorelease pools for queue worker threads. All ObjC
        // autoreleased objects stay on the queue's thread — never polluting
        // the caller's TLS pool. This prevents SIGSEGV at thread exit.

        type BootResult =
            Result<(VmHandle, std::sync::mpsc::Receiver<Result<(), String>>), VmError>;
        let (tx, rx) = std::sync::mpsc::channel::<BootResult>();
        let name_q = name.clone();
        let namespace_q = namespace.clone();
        let serial_log_q = serial_log.clone();

        let boot_block = ConcreteBlock::new(move || {
            let result = (|| -> Result<(VmHandle, std::sync::mpsc::Receiver<Result<(), String>>), VmError> {
                // Check VZ support (ObjC class method)
                if !VZVirtualMachine::supported() {
                    return Err(VmError::Hypervisor(
                        "Apple Virtualization.framework is not supported on this machine".into(),
                    ));
                }

                // ── Boot loader ──

                let boot_loader_id: Id = if let Some(ref efi_path) = efi_store_path {
                    // UEFI boot (macOS 13+)
                    let efi_path_str = efi_path.to_str().ok_or_else(|| {
                        VmError::InvalidConfig("non-UTF8 EFI variable store path".into())
                    })?;
                    let store = if efi_path.exists() {
                        VZEFIVariableStore::open(efi_path_str).map_err(|e| {
                            VmError::BootFailed {
                                name: name_q.clone(),
                                detail: format!(
                                    "failed to open EFI variable store: code {}",
                                    e.code()
                                ),
                            }
                        })?
                    } else {
                        VZEFIVariableStore::create(efi_path_str).map_err(|e| {
                            VmError::BootFailed {
                                name: name_q.clone(),
                                detail: format!(
                                    "failed to create EFI variable store: code {}",
                                    e.code()
                                ),
                            }
                        })?
                    };
                    let efi_loader = VZEFIBootLoader::new(&store);
                    use crate::ffi::apple_vz::boot_loader::VZBootLoader;
                    efi_loader.id()
                } else {
                    // Direct Linux boot
                    let initrd_str = initrd.as_deref().ok_or_else(|| {
                        VmError::InvalidConfig("initramfs required for Linux boot".into())
                    })?;
                    let loader = VZLinuxBootLoaderBuilder::new()
                        .kernel_url(&kernel_abs)
                        .initial_ramdisk_url(initrd_str)
                        .command_line(&cmdline)
                        .build();
                    use crate::ffi::apple_vz::boot_loader::VZBootLoader;
                    loader.id()
                };

                // ── Platform configuration (macOS 12+) ──

                let mut platform = VZGenericPlatformConfiguration::new();
                let machine_id = if let Some(ref bytes) = machine_id_bytes {
                    VZGenericMachineIdentifier::from_data(bytes).unwrap_or_else(|| {
                        tracing::warn!(
                            vm = %name_q,
                            "invalid machine_id bytes, generating new identifier"
                        );
                        VZGenericMachineIdentifier::new()
                    })
                } else {
                    VZGenericMachineIdentifier::new()
                };
                let machine_id_data = machine_id.data_representation();
                platform.set_machine_identifier(&machine_id);

                // ── Serial console → per-VM log file ──

                // SAFETY: fds are valid (opened on caller thread, ownership transferred here).
                let read_handle = unsafe { NSFileHandle::file_handle_with_fd_owned(null_fd) };
                let write_handle = unsafe { NSFileHandle::file_handle_with_fd_owned(log_fd) };
                let serial_attachment = VZFileHandleSerialPortAttachmentBuilder::new()
                    .file_handle_for_reading(read_handle)
                    .file_handle_for_writing(write_handle)
                    .build();
                let serial =
                    VZVirtioConsoleDeviceSerialPortConfiguration::new(serial_attachment);

                let entropy = VZVirtioEntropyDeviceConfiguration::new();
                let memory_balloon =
                    VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();

                // ── Network devices ──

                // NIC 1: NAT
                let nat_attachment = VZNATNetworkDeviceAttachment::new();
                let mut nat_nic = VZVirtioNetworkDeviceConfiguration::new(nat_attachment);
                nat_nic.set_mac_address(VZMACAddress::random_locally_administered_address());
                let mut network_devices = vec![nat_nic];

                // NIC 2+: FileHandle per network
                for net in &networks {
                    if let NetworkAttachment::SocketPairFd(fd) = net {
                        // SAFETY: fd from socketpair managed by NetworkSwitch.
                        let file_handle =
                            unsafe { NSFileHandle::file_handle_with_fd_borrowed(*fd) };
                        let fh_attachment =
                            VZFileHandleNetworkDeviceAttachment::new(file_handle);
                        let mut nic = VZVirtioNetworkDeviceConfiguration::new(fh_attachment);
                        nic.set_mac_address(
                            VZMACAddress::random_locally_administered_address(),
                        );
                        network_devices.push(nic);
                    }
                }

                // ── Disks ──

                let mut storage_devices: Vec<VZVirtioBlockDeviceConfiguration> = Vec::new();
                if let Some(ref disk_path) = disk_abs {
                    let root_attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
                        .path(disk_path)
                        .read_only(false)
                        .caching_mode(VZDiskImageCachingMode::Automatic)
                        .sync_mode(VZDiskImageSynchronizationMode::Full)
                        .build()
                        .map_err(|e| VmError::BootFailed {
                            name: name_q.clone(),
                            detail: format!(
                                "failed to attach root disk: code {}",
                                e.code()
                            ),
                        })?;
                    storage_devices
                        .push(VZVirtioBlockDeviceConfiguration::new(root_attachment));
                }
                if let Some(ref seed_path) = seed_abs {
                    let seed_attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
                        .path(seed_path)
                        .read_only(true)
                        .build()
                        .map_err(|e| VmError::BootFailed {
                            name: name_q.clone(),
                            detail: format!(
                                "failed to attach seed ISO: code {}",
                                e.code()
                            ),
                        })?;
                    storage_devices
                        .push(VZVirtioBlockDeviceConfiguration::new(seed_attachment));
                }

                // ── VirtioFS shared directories ──

                let mut vz_shared_dirs: Vec<VZVirtioFileSystemDeviceConfiguration> =
                    Vec::new();
                for vol in &shared_dirs {
                    let host_str = vol.host_path.to_str().ok_or_else(|| {
                        VmError::InvalidConfig("non-UTF8 shared dir path".into())
                    })?;
                    let dir = VZSharedDirectory::new(host_str, vol.read_only);
                    let share = VZSingleDirectoryShare::new(dir);
                    let mut fs_device =
                        VZVirtioFileSystemDeviceConfiguration::new(&vol.tag);
                    fs_device.set_share(share);
                    vz_shared_dirs.push(fs_device);
                }

                // ── Rosetta x86_64 translation (macOS 13+, Apple Silicon) ──

                if enable_rosetta {
                    match VZLinuxRosettaDirectoryShare::availability() {
                        VZLinuxRosettaAvailability::Installed => {
                            let rosetta = VZLinuxRosettaDirectoryShare::new();
                            let mut fs_device =
                                VZVirtioFileSystemDeviceConfiguration::new("rosetta");
                            fs_device.set_share(rosetta);
                            vz_shared_dirs.push(fs_device);
                            tracing::info!(vm = %name_q, "Rosetta x86_64 translation enabled");
                        }
                        VZLinuxRosettaAvailability::NotInstalled => {
                            return Err(VmError::InvalidConfig(
                                "Rosetta is supported but not installed. Run: \
                                 softwareupdate --install-rosetta"
                                    .into(),
                            ));
                        }
                        VZLinuxRosettaAvailability::NotSupported => {
                            return Err(VmError::InvalidConfig(
                                "Rosetta is not supported on this machine \
                                 (requires Apple Silicon)"
                                    .into(),
                            ));
                        }
                    }
                }

                // ── vsock device ──

                let vsock_devices = if enable_vsock {
                    vec![VZVirtioSocketDeviceConfiguration::new()]
                } else {
                    vec![]
                };

                // ── Assemble VM configuration ──

                // Boot loader — use a wrapper struct to pass the raw Id
                struct RawBootLoader(Id);
                impl crate::ffi::apple_vz::boot_loader::VZBootLoader for RawBootLoader {
                    fn id(&self) -> Id {
                        self.0
                    }
                }

                let mut builder = VZVirtualMachineConfigurationBuilder::new()
                    .boot_loader(RawBootLoader(boot_loader_id))
                    .cpu_count(cpus)
                    .memory_size(memory_bytes)
                    .entropy_devices(vec![entropy])
                    .memory_balloon_devices(vec![memory_balloon])
                    .network_devices(network_devices)
                    .serial_ports(vec![serial])
                    .storage_devices(storage_devices)
                    .platform(platform);

                if !vz_shared_dirs.is_empty() {
                    builder = builder.directory_sharing_devices(vz_shared_dirs);
                }
                if !vsock_devices.is_empty() {
                    builder = builder.socket_devices(vsock_devices);
                }

                let vm_config = builder.build();

                // Validate
                vm_config.validate_with_error().map_err(|e| {
                    VmError::BootFailed {
                        name: name_q.clone(),
                        detail: format!(
                            "VZ configuration validation failed: code {}. \
                             Hint: the binary must be signed with virtualization entitlement",
                            e.code()
                        ),
                    }
                })?;

                // Create VM on this queue
                let vm = VZVirtualMachine::new(vm_config, queue);
                let vm_boxed = Box::pin(vm);

                // Register VM — the Pin<Box<>> keeps it alive and at a
                // stable address. Removing from registry drops the VM.
                {
                    let mut registry = VM_REGISTRY.lock().map_err(|e| {
                        VmError::Hypervisor(format!("VM registry lock poisoned: {}", e))
                    })?;
                    registry.insert(
                        name_q.clone(),
                        VmEntry {
                            vm: vm_boxed,
                            queue,
                        },
                    );
                }

                // SAFETY: We need a &VZVirtualMachine for the start call.
                // The VM is pinned in the registry and won't move or be
                // dropped while we hold the registry lock (released above).
                // The GCD queue keeps a reference via ObjC retain count.
                let vm_ref: &VZVirtualMachine = {
                    let registry = VM_REGISTRY.lock().map_err(|e| {
                        VmError::Hypervisor(format!("VM registry lock poisoned: {}", e))
                    })?;
                    let entry = registry.get(&name_q).ok_or_else(|| {
                        VmError::Hypervisor("VM disappeared from registry".into())
                    })?;
                    // SAFETY: Pointer is stable because Pin<Box<>> prevents
                    // moves. The VM lives as long as it's in the registry.
                    unsafe { &*((&*entry.vm) as *const VZVirtualMachine) }
                };

                // Start VM (we're already on the VM's dispatch queue).
                // Wait for the start completion handler to confirm the VM
                // actually started — do not return success prematurely.
                let (start_tx, start_rx) =
                    std::sync::mpsc::channel::<Result<(), String>>();
                let name_for_err = name_q.clone();
                let completion_handler = ConcreteBlock::new(move |err: Id| {
                    if err == NIL {
                        tracing::info!("VM '{}' started successfully", name_for_err);
                        let _ = start_tx.send(Ok(()));
                    } else {
                        // SAFETY: err is a valid NSError from the framework.
                        let error = unsafe { NSError(StrongPtr::retain(err)) };
                        let detail = format!("start error code {}", error.code());
                        tracing::error!(
                            "VM '{}' start FAILED: {}",
                            name_for_err,
                            detail
                        );
                        let _ = start_tx.send(Err(detail));
                    }
                });
                let completion_handler = completion_handler.copy();
                let completion_handler: &Block<(Id,), ()> = &completion_handler;
                vm_ref.start_with_completion_handler(completion_handler);

                // The completion handler fires asynchronously on this same
                // GCD queue, so we must return from this block first.
                // The outer recv on `rx` handles the boot() result; we
                // stash start_rx in a second channel read below.
                //
                // Since we're inside the GCD block, we can't block here
                // waiting for start_rx (the completion runs on this queue).
                // Instead, pass start_rx out through the boot result so
                // the caller thread can wait for it.

                Ok((
                    VmHandle {
                        name: name_q.clone(),
                        namespace: namespace_q.clone(),
                        state: VmState::Starting,
                        pid: None, // Apple VZ runs in-process, no separate PID
                        serial_log: serial_log_q.clone(),
                        machine_id: Some(machine_id_data),
                    },
                    start_rx,
                ))
            })();

            let _ = tx.send(result);
        });
        let boot_block = boot_block.copy();
        let boot_block: &Block<(), ()> = &boot_block;

        // SAFETY: queue is a valid GCD queue, block is properly retained.
        unsafe {
            dispatch_async(queue, boot_block);
        }

        // ── Step 4: Wait for result ──

        // Wait for the GCD block to finish configuration + start call
        let (handle, start_rx) = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(result) => result?,
            Err(_) => {
                return Err(VmError::BootFailed {
                    name,
                    detail: "boot timed out after 10s (GCD block did not execute)".into(),
                })
            }
        };

        // Now wait for the actual start completion handler to confirm the
        // VM started. This fires asynchronously on the GCD queue.
        match start_rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(Ok(())) => Ok(handle),
            Ok(Err(detail)) => {
                // VM start failed — remove from registry
                if let Ok(mut registry) = VM_REGISTRY.lock() {
                    registry.remove(&handle.name);
                }
                Err(VmError::BootFailed {
                    name: handle.name,
                    detail,
                })
            }
            Err(_) => {
                // Timeout waiting for start confirmation — VM may still
                // be starting (large kernels, slow disks). Return the
                // handle in Starting state; caller can poll state().
                tracing::warn!(
                    vm = %handle.name,
                    "start completion handler did not fire within 30s, \
                     VM may still be starting"
                );
                Ok(handle)
            }
        }
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        let (vm_ptr, queue) = get_vm_and_queue(&handle.name)?;
        // Convert to usize for Send across GCD block boundary.
        let vm_addr = vm_ptr as usize;
        let name = handle.name.clone();

        // dispatch_async + channel avoids placing ObjC autorelease objects
        // on the caller's thread pool (prevents SIGSEGV at thread exit).
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let stop_block = ConcreteBlock::new(move || {
            // SAFETY: pointer is stable (Pin<Box<>>) and valid while VM is in registry.
            // Called on the VM's GCD queue. Mutable ref needed for request_stop_with_error.
            let vm_ref = unsafe { &mut *(vm_addr as *mut VZVirtualMachine) };
            let result = unsafe {
                match vm_ref.request_stop_with_error() {
                    Ok(_) => {
                        tracing::info!("VM '{}' stop requested", name);
                        Ok(())
                    }
                    Err(e) => Err(format!("error code {}", e.code())),
                }
            };
            let _ = tx.send(result);
        });
        let stop_block = stop_block.copy();
        let stop_block: &Block<(), ()> = &stop_block;

        // SAFETY: queue is a valid GCD queue, block is retained.
        unsafe {
            dispatch_async(queue, stop_block);
        }

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(detail)) => Err(VmError::StopFailed {
                name: handle.name.clone(),
                detail,
            }),
            Err(_) => Err(VmError::StopFailed {
                name: handle.name.clone(),
                detail: "stop timed out after 5s".into(),
            }),
        }
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        // Remove entry and drop the lock before blocking on the channel.
        let entry = {
            let mut registry = VM_REGISTRY
                .lock()
                .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
            registry.remove(&handle.name)
        };

        if let Some(entry) = entry {
            // Get raw pointer before moving entry into closure.
            let vm_ptr: *const VZVirtualMachine = &*entry.vm;
            let vm_addr = vm_ptr as usize;
            let queue = entry.queue;
            let name = handle.name.clone();

            // Keep the entry alive until the GCD block completes — the VM
            // object must not be dropped while stop is in flight.
            // We move `entry` into the outer closure for this.

            // stopWithCompletionHandler (macOS 14+) via dispatch_async.
            // dispatch_async keeps all ObjC autoreleases on the GCD queue's
            // thread — never the caller's TLS pool.
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            let name_inner = name.clone();
            let stop_block = ConcreteBlock::new(move || {
                // SAFETY: pointer is valid — entry (moved into this closure) keeps the VM alive.
                let vm_ref = unsafe { &*(vm_addr as *const VZVirtualMachine) };
                let name_err = name_inner.clone();
                let tx_inner = tx.clone();
                let completion = ConcreteBlock::new(move |err: Id| {
                    if err == NIL {
                        tracing::debug!("VM '{}' force-stopped", name_err);
                    } else {
                        // SAFETY: err is a valid NSError from the framework.
                        let error = unsafe { NSError(StrongPtr::retain(err)) };
                        tracing::debug!(
                            "VM '{}' force-stop error: code {}",
                            name_err,
                            error.code()
                        );
                    }
                    let _ = tx_inner.send(());
                });
                let completion = completion.copy();
                let completion: &Block<(Id,), ()> = &completion;
                vm_ref.stop_with_completion_handler(completion);
                // `entry` is implicitly kept alive here by the closure capture.
                let _ = &entry;
            });
            let stop_block = stop_block.copy();
            let stop_block: &Block<(), ()> = &stop_block;

            // SAFETY: queue is a valid GCD queue, block is properly retained.
            unsafe {
                dispatch_async(queue, stop_block);
            }

            // Wait for completion (with timeout to avoid hanging)
            let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
        }

        tracing::info!("killed VM '{}'", handle.name);
        Ok(())
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let (vm_ptr, queue) = match get_vm_and_queue(&handle.name) {
            Ok(pair) => pair,
            Err(VmError::NotFound { .. }) => return Ok(VmState::Stopped),
            Err(e) => return Err(e),
        };
        let vm_addr = vm_ptr as usize;

        let serial_log = handle.serial_log.clone();

        // Dispatch the ObjC state query to the VM's GCD queue.
        // This keeps ObjC autoreleases off the caller's thread.
        let (tx, rx) = std::sync::mpsc::channel::<VmState>();
        let state_block = ConcreteBlock::new(move || {
            // SAFETY: pointer is stable (Pin<Box<>>) and valid while VM is in registry.
            let vm_ref = unsafe { &*(vm_addr as *const VZVirtualMachine) };
            let vz_state = unsafe { vm_ref.state() };
            let result = match vz_state {
                VZVirtualMachineState::VZVirtualMachineStateStopped => VmState::Stopped,
                VZVirtualMachineState::VZVirtualMachineStateError => VmState::Failed {
                    reason: "VZ framework reported error state".into(),
                },
                VZVirtualMachineState::VZVirtualMachineStateStarting => VmState::Starting,
                VZVirtualMachineState::VZVirtualMachineStatePaused => VmState::Paused,
                VZVirtualMachineState::VZVirtualMachineStatePausing
                | VZVirtualMachineState::VZVirtualMachineStateResuming => VmState::Paused,
                VZVirtualMachineState::VZVirtualMachineStateRunning => {
                    if let Some(ip) = check_ready_marker(&serial_log) {
                        VmState::Running { ip }
                    } else {
                        VmState::Starting
                    }
                }
                VZVirtualMachineState::Other => VmState::Starting,
            };
            let _ = tx.send(result);
        });
        let state_block = state_block.copy();
        let state_block: &Block<(), ()> = &state_block;

        // SAFETY: queue is a valid GCD queue, block is retained.
        unsafe {
            dispatch_async(queue, state_block);
        }

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(state) => Ok(state),
            Err(_) => Err(VmError::Hypervisor("state query timed out after 5s".into())),
        }
    }

    fn pause(&self, handle: &VmHandle) -> Result<(), VmError> {
        let (vm_ptr, queue) = get_vm_and_queue(&handle.name)?;
        let vm_addr = vm_ptr as usize;
        let name = handle.name.clone();

        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let pause_block = ConcreteBlock::new(move || {
            // SAFETY: pointer is stable (Pin<Box<>>) and valid while VM is in registry.
            let vm_ref = unsafe { &*(vm_addr as *const VZVirtualMachine) };
            let can = unsafe { vm_ref.can_pause() };
            if !can {
                let _ = tx.send(Err("VM is not in a state that can be paused".into()));
                return;
            }

            let tx_inner = tx.clone();
            let name_inner = name.clone();
            let completion = ConcreteBlock::new(move |err: Id| {
                if err == NIL {
                    tracing::info!("VM '{}' paused", name_inner);
                    let _ = tx_inner.send(Ok(()));
                } else {
                    let error = unsafe { NSError(StrongPtr::retain(err)) };
                    let _ = tx_inner.send(Err(format!("pause error code {}", error.code())));
                }
            });
            let completion = completion.copy();
            let completion: &Block<(Id,), ()> = &completion;
            vm_ref.pause_with_completion_handler(completion);
        });
        let pause_block = pause_block.copy();
        let pause_block: &Block<(), ()> = &pause_block;

        unsafe {
            dispatch_async(queue, pause_block);
        }

        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(detail)) => Err(VmError::Hypervisor(format!(
                "pause failed for '{}': {}",
                handle.name, detail
            ))),
            Err(_) => Err(VmError::Hypervisor(format!(
                "pause timed out for '{}'",
                handle.name
            ))),
        }
    }

    fn resume(&self, handle: &VmHandle) -> Result<(), VmError> {
        let (vm_ptr, queue) = get_vm_and_queue(&handle.name)?;
        let vm_addr = vm_ptr as usize;
        let name = handle.name.clone();

        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let resume_block = ConcreteBlock::new(move || {
            // SAFETY: pointer is stable (Pin<Box<>>) and valid while VM is in registry.
            let vm_ref = unsafe { &*(vm_addr as *const VZVirtualMachine) };
            let can = unsafe { vm_ref.can_resume() };
            if !can {
                let _ = tx.send(Err("VM is not in a state that can be resumed".into()));
                return;
            }

            let tx_inner = tx.clone();
            let name_inner = name.clone();
            let completion = ConcreteBlock::new(move |err: Id| {
                if err == NIL {
                    tracing::info!("VM '{}' resumed", name_inner);
                    let _ = tx_inner.send(Ok(()));
                } else {
                    let error = unsafe { NSError(StrongPtr::retain(err)) };
                    let _ = tx_inner.send(Err(format!("resume error code {}", error.code())));
                }
            });
            let completion = completion.copy();
            let completion: &Block<(Id,), ()> = &completion;
            vm_ref.resume_with_completion_handler(completion);
        });
        let resume_block = resume_block.copy();
        let resume_block: &Block<(), ()> = &resume_block;

        unsafe {
            dispatch_async(queue, resume_block);
        }

        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(detail)) => Err(VmError::Hypervisor(format!(
                "resume failed for '{}': {}",
                handle.name, detail
            ))),
            Err(_) => Err(VmError::Hypervisor(format!(
                "resume timed out for '{}'",
                handle.name
            ))),
        }
    }
}

/// Extract a raw pointer to the VM and the queue from the registry.
///
/// SAFETY: The returned pointer is valid as long as the VM remains in the
/// registry. The caller must not hold it across operations that could remove
/// the VM from the registry.
fn get_vm_and_queue(name: &str) -> Result<(*const VZVirtualMachine, Id), VmError> {
    let registry = VM_REGISTRY
        .lock()
        .map_err(|e| VmError::Hypervisor(format!("VM registry lock poisoned: {}", e)))?;
    let entry = registry.get(name).ok_or_else(|| VmError::NotFound {
        name: name.to_string(),
    })?;
    // SAFETY: Pin<Box<>> ensures stable address. Pointer valid while in registry.
    let vm_ptr: *const VZVirtualMachine = &*entry.vm;
    Ok((vm_ptr, entry.queue))
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
