//! VirtioFS shared directory module — share host directories with VMs.

use super::base::{Id, NSString};

use objc::rc::StrongPtr;
use objc::{class, msg_send, sel, sel_impl};
use objc::runtime::YES;

/// Trait for directory sharing configurations.
pub trait VZDirectorySharingDeviceConfiguration {
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

    /// Set the directory share for this device.
    pub fn set_share(&mut self, share: VZSingleDirectoryShare) {
        unsafe {
            let _: () = msg_send![*self.0, setShare:*share.0];
        }
    }
}

impl VZDirectorySharingDeviceConfiguration for VZVirtioFileSystemDeviceConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}

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

/// A shared directory path.
pub struct VZSharedDirectory(StrongPtr);

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
