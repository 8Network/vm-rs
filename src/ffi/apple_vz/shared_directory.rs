//! VirtioFS shared directory module — share host directories with VMs.
//!
//! Three share types:
//! - `VZSingleDirectoryShare` — one host directory per VirtioFS device
//! - `VZMultipleDirectoryShare` — multiple host directories under one device (macOS 12+)
//! - `VZLinuxRosettaDirectoryShare` — x86_64 translation on Apple Silicon (macOS 13+)

use super::base::{Id, NSDictionary, NSString};

use objc::rc::StrongPtr;
use objc::runtime::YES;
use objc::{class, msg_send, sel, sel_impl};

/// Trait for directory sharing configurations.
pub trait VZDirectorySharingDeviceConfiguration {
    fn id(&self) -> Id;
}

/// Trait for directory share types (VZDirectoryShare).
///
/// Implemented by `VZSingleDirectoryShare`, `VZMultipleDirectoryShare`,
/// and `VZLinuxRosettaDirectoryShare`.
pub trait VZDirectoryShare {
    fn id(&self) -> Id;
}

/// VirtioFS device configuration — exposes a host directory to the VM.
pub struct VZVirtioFileSystemDeviceConfiguration(StrongPtr);

impl VZVirtioFileSystemDeviceConfiguration {
    /// Create a VirtioFS device with the given mount tag.
    /// The tag is used inside the VM to mount: `mount -t virtiofs <tag> /mnt/point`
    pub fn new(tag: &str) -> Self {
        unsafe {
            let tag_ns = NSString::new(tag);
            let alloc: Id = msg_send![class!(VZVirtioFileSystemDeviceConfiguration), alloc];
            let p = StrongPtr::new(msg_send![alloc, initWithTag:*tag_ns.0]);
            VZVirtioFileSystemDeviceConfiguration(p)
        }
    }

    /// Set the directory share for this device (any share type).
    pub fn set_share<T: VZDirectoryShare>(&mut self, share: T) {
        unsafe {
            let _: () = msg_send![*self.0, setShare:share.id()];
        }
    }
}

impl VZDirectorySharingDeviceConfiguration for VZVirtioFileSystemDeviceConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}

// ─── Single Directory Share ─────────────────────────────────────────────

/// A single directory share.
pub struct VZSingleDirectoryShare(StrongPtr);

impl VZSingleDirectoryShare {
    pub fn new(directory: VZSharedDirectory) -> Self {
        unsafe {
            let alloc: Id = msg_send![class!(VZSingleDirectoryShare), alloc];
            let p = StrongPtr::new(msg_send![alloc, initWithDirectory:*directory.0]);
            VZSingleDirectoryShare(p)
        }
    }
}

impl VZDirectoryShare for VZSingleDirectoryShare {
    fn id(&self) -> Id {
        *self.0
    }
}

/// A shared directory path.
pub struct VZSharedDirectory(pub StrongPtr);

impl VZSharedDirectory {
    /// Create a shared directory from a host path.
    /// `read_only`: if true, the VM can only read files.
    pub fn new(path: &str, read_only: bool) -> Self {
        unsafe {
            let url = super::base::NSURL::file_url_with_path(path, true);
            let ro = if read_only { YES } else { objc::runtime::NO };
            let alloc: Id = msg_send![class!(VZSharedDirectory), alloc];
            let p = StrongPtr::new(msg_send![alloc, initWithURL:*url.0 readOnly:ro]);
            VZSharedDirectory(p)
        }
    }
}

// ─── Multiple Directory Share (macOS 12+) ───────────────────────────────

/// Multiple directories under one VirtioFS device (macOS 12+).
///
/// Maps multiple host directories to named sub-mounts under a single device.
/// Inside the guest, each directory appears as a subdirectory of the mount point.
///
/// # Examples
///
/// ```ignore
/// let dirs = vec![
///     ("config", VZSharedDirectory::new("/host/config", true)),
///     ("data", VZSharedDirectory::new("/host/data", false)),
/// ];
/// let share = VZMultipleDirectoryShare::new(&dirs);
/// let mut device = VZVirtioFileSystemDeviceConfiguration::new("shares");
/// device.set_share(share);
/// ```
pub struct VZMultipleDirectoryShare(StrongPtr);

impl VZMultipleDirectoryShare {
    /// Create a multiple directory share from tag → directory pairs.
    pub fn new(directories: &[(&str, VZSharedDirectory)]) -> Self {
        let pairs: Vec<(Id, Id)> = directories
            .iter()
            .map(|(tag, dir)| {
                let key = NSString::new(tag);
                // NSDictionary retains both keys and values, so these stay alive.
                (*key.0, *dir.0)
            })
            .collect();
        let dict = NSDictionary::from_pairs(&pairs);

        unsafe {
            let alloc: Id = msg_send![class!(VZMultipleDirectoryShare), alloc];
            let p = StrongPtr::new(msg_send![alloc, initWithDirectories:*dict.0]);
            VZMultipleDirectoryShare(p)
        }
    }
}

impl VZDirectoryShare for VZMultipleDirectoryShare {
    fn id(&self) -> Id {
        *self.0
    }
}

// ─── Rosetta x86_64 Translation (macOS 13+, Apple Silicon) ──────────────

/// Rosetta availability status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VZLinuxRosettaAvailability {
    /// Rosetta is not supported on this hardware (Intel Mac or older macOS).
    NotSupported,
    /// Rosetta is supported but not yet installed.
    NotInstalled,
    /// Rosetta is installed and available.
    Installed,
}

/// VirtioFS share that enables Rosetta x86_64-to-ARM64 translation (macOS 13+).
///
/// Mount this inside the Linux guest and register the Rosetta binary with binfmt_misc.
/// After setup, x86_64 ELF binaries run transparently on the ARM64 guest.
///
/// # Guest Setup
///
/// ```bash
/// mount -t virtiofs rosetta /mnt/rosetta
/// /usr/sbin/update-binfmts --install rosetta /mnt/rosetta/rosetta \
///     --magic '\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00' \
///     --mask '\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff' \
///     --credentials yes --preserve no --fix-binary yes
/// ```
pub struct VZLinuxRosettaDirectoryShare(StrongPtr);

impl Default for VZLinuxRosettaDirectoryShare {
    fn default() -> Self {
        Self::new()
    }
}

impl VZLinuxRosettaDirectoryShare {
    /// Create a new Rosetta directory share.
    ///
    /// Check `availability()` first to verify Rosetta is supported and installed.
    pub fn new() -> Self {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZLinuxRosettaDirectoryShare), new]);
            VZLinuxRosettaDirectoryShare(p)
        }
    }

    /// Check if Rosetta is available on this machine.
    ///
    /// Returns the availability status. Rosetta requires Apple Silicon
    /// and may need to be installed on first use.
    pub fn availability() -> VZLinuxRosettaAvailability {
        unsafe {
            let n: isize = msg_send![class!(VZLinuxRosettaDirectoryShare), availability];
            match n {
                1 => VZLinuxRosettaAvailability::NotInstalled,
                2 => VZLinuxRosettaAvailability::Installed,
                _ => VZLinuxRosettaAvailability::NotSupported,
            }
        }
    }
}

impl VZDirectoryShare for VZLinuxRosettaDirectoryShare {
    fn id(&self) -> Id {
        *self.0
    }
}
