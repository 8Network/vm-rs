//! boot loader module
use super::base::{Id, NSString, NSURL};

use objc::rc::StrongPtr;
use objc::runtime::Sel;
use objc::{class, msg_send, sel, sel_impl};

/// common behaviors for booting
pub trait VZBootLoader {
    fn id(&self) -> Id;
}

/// Builder for VZLinuxBootLoader.
///
/// # Examples
///
/// ```ignore
/// let boot_loader = VZLinuxBootLoaderBuilder::new()
///     .kernel_url("/path/to/vmlinuz")
///     .initial_ramdisk_url("/path/to/initramfs")
///     .command_line("console=hvc0")
///     .build();
/// ```
pub struct VZLinuxBootLoaderBuilder<KernelURL, InitialRamdiskURL, CommandLine> {
    kernel_url: KernelURL,
    initial_ramdisk_url: InitialRamdiskURL,
    command_line: CommandLine,
}

impl Default for VZLinuxBootLoaderBuilder<(), (), ()> {
    fn default() -> Self {
        Self::new()
    }
}

impl VZLinuxBootLoaderBuilder<(), (), ()> {
    pub fn new() -> Self {
        VZLinuxBootLoaderBuilder {
            kernel_url: (),
            initial_ramdisk_url: (),
            command_line: (),
        }
    }
}

impl<KernelURL, InitialRamdiskURL, CommandLine>
    VZLinuxBootLoaderBuilder<KernelURL, InitialRamdiskURL, CommandLine>
{
    pub fn kernel_url<T: Into<String>>(
        self,
        kernel_url: T,
    ) -> VZLinuxBootLoaderBuilder<String, InitialRamdiskURL, CommandLine> {
        VZLinuxBootLoaderBuilder {
            kernel_url: kernel_url.into(),
            initial_ramdisk_url: self.initial_ramdisk_url,
            command_line: self.command_line,
        }
    }

    pub fn initial_ramdisk_url<T: Into<String>>(
        self,
        initial_ramdisk_url: T,
    ) -> VZLinuxBootLoaderBuilder<KernelURL, String, CommandLine> {
        VZLinuxBootLoaderBuilder {
            kernel_url: self.kernel_url,
            initial_ramdisk_url: initial_ramdisk_url.into(),
            command_line: self.command_line,
        }
    }

    pub fn command_line<T: Into<String>>(
        self,
        command_line: T,
    ) -> VZLinuxBootLoaderBuilder<KernelURL, InitialRamdiskURL, String> {
        VZLinuxBootLoaderBuilder {
            kernel_url: self.kernel_url,
            initial_ramdisk_url: self.initial_ramdisk_url,
            command_line: command_line.into(),
        }
    }
}

impl VZLinuxBootLoaderBuilder<String, String, String> {
    pub fn build(self) -> VZLinuxBootLoader {
        unsafe {
            VZLinuxBootLoader::new(
                self.kernel_url.as_str(),
                self.initial_ramdisk_url.as_str(),
                self.command_line.as_str(),
            )
        }
    }
}

///  bootLoader for Linux kernel
pub struct VZLinuxBootLoader(StrongPtr);

impl VZLinuxBootLoader {
    unsafe fn new(
        kernel_url: &str,
        initial_ramdisk_url: &str,
        command_line: &str,
    ) -> VZLinuxBootLoader {
        let kernel_url_nsurl = NSURL::file_url_with_path(kernel_url, false).absolute_url();
        let initial_ramdisk_url_nsurl =
            NSURL::file_url_with_path(initial_ramdisk_url, false).absolute_url();
        let command_line_nsstring = NSString::new(command_line);
        let p = StrongPtr::new(msg_send![class!(VZLinuxBootLoader), new]);
        let _: Id = msg_send![*p, setKernelURL: *kernel_url_nsurl.0];
        let _: Id = msg_send![*p, setInitialRamdiskURL: *initial_ramdisk_url_nsurl.0];
        let _: Id = msg_send![*p, setCommandLine: *command_line_nsstring.0];
        VZLinuxBootLoader(p)
    }
}

impl VZBootLoader for VZLinuxBootLoader {
    fn id(&self) -> Id {
        *self.0
    }
}

// ─── UEFI Boot (macOS 13+) ─────────────────────────────────────────────

/// EFI boot loader for booting arbitrary distro ISOs and UEFI-only images (macOS 13+).
///
/// Requires a `VZEFIVariableStore` for NVRAM persistence.
///
/// # Examples
///
/// ```ignore
/// let store = VZEFIVariableStore::create("/path/to/nvram.bin").unwrap();
/// let boot_loader = VZEFIBootLoader::new(&store);
/// ```
pub struct VZEFIBootLoader(StrongPtr);

impl VZEFIBootLoader {
    /// Create a new EFI boot loader with the given variable store.
    pub fn new(variable_store: &VZEFIVariableStore) -> Self {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZEFIBootLoader), new]);
            let _: () = msg_send![*p, setVariableStore:*variable_store.0];
            VZEFIBootLoader(p)
        }
    }
}

impl VZBootLoader for VZEFIBootLoader {
    fn id(&self) -> Id {
        *self.0
    }
}

/// EFI variable store (NVRAM) for UEFI boot (macOS 13+).
///
/// Persists UEFI variables (boot order, Secure Boot state, etc.) across reboots.
/// Create once with `create()`, then open with `open()` on subsequent boots.
pub struct VZEFIVariableStore(StrongPtr);

impl VZEFIVariableStore {
    /// Create a new EFI variable store at the given path.
    ///
    /// Creates the file on disk. Use `open()` for subsequent boots.
    ///
    /// Handles both macOS 13-15 (`initCreatingVariableStoreAtURL:error:`)
    /// and macOS 16+ (`initCreatingVariableStoreAtURL:options:error:`) selectors.
    pub fn create(path: &str) -> Result<Self, super::base::NSError> {
        unsafe {
            let url = NSURL::file_url_with_path(path, false);
            let cls = class!(VZEFIVariableStore);
            let alloc: Id = msg_send![cls, alloc];
            let mut error: Id = super::base::NIL;

            // macOS 16+ changed the selector to include an options: parameter
            let new_sel = Sel::register("initCreatingVariableStoreAtURL:options:error:");
            let has_new: bool = msg_send![cls, instancesRespondToSelector: new_sel];

            let p: Id = if has_new {
                let options: u64 = 0; // default options
                msg_send![alloc, initCreatingVariableStoreAtURL:*url.0
                                 options:options
                                 error:&mut error]
            } else {
                msg_send![alloc, initCreatingVariableStoreAtURL:*url.0
                                 error:&mut error]
            };

            if !error.is_null() {
                Err(super::base::NSError(StrongPtr::retain(error)))
            } else {
                Ok(VZEFIVariableStore(StrongPtr::new(p)))
            }
        }
    }

    /// Open an existing EFI variable store from a previous boot.
    ///
    /// Handles both macOS 13-15 (`initWithURL:error:`) and macOS 16+
    /// (`initWithURL:` without error) selectors.
    pub fn open(path: &str) -> Result<Self, super::base::NSError> {
        unsafe {
            let url = NSURL::file_url_with_path(path, false);
            let cls = class!(VZEFIVariableStore);
            let alloc: Id = msg_send![cls, alloc];

            // macOS 16+ simplified to initWithURL: (no error: param)
            let old_sel = Sel::register("initWithURL:error:");
            let has_old: bool = msg_send![cls, instancesRespondToSelector: old_sel];

            if has_old {
                let mut error: Id = super::base::NIL;
                let p: Id = msg_send![alloc, initWithURL:*url.0 error:&mut error];
                if !error.is_null() {
                    Err(super::base::NSError(StrongPtr::retain(error)))
                } else {
                    Ok(VZEFIVariableStore(StrongPtr::new(p)))
                }
            } else {
                // macOS 16+: initWithURL: (no error param). Pre-check file
                // existence since the API may return non-nil for bad paths.
                if !std::path::Path::new(path).exists() {
                    return Err(super::base::NSError::from_description(
                        "VZEFIVariableStore",
                        "EFI variable store file does not exist",
                    ));
                }
                let p: Id = msg_send![alloc, initWithURL:*url.0];
                if p.is_null() {
                    Err(super::base::NSError::from_description(
                        "VZEFIVariableStore",
                        "Failed to open EFI variable store",
                    ))
                } else {
                    Ok(VZEFIVariableStore(StrongPtr::new(p)))
                }
            }
        }
    }
}
