//! storage device module

use super::base::{Id, NIL, NSError, NSURL};

use objc::runtime::BOOL;
use objc::{class, msg_send, sel, sel_impl};
use objc::{rc::StrongPtr, runtime::NO, runtime::YES};

/// Disk image caching mode (macOS 12.3+).
#[repr(isize)]
#[derive(Debug, Clone, Copy)]
pub enum VZDiskImageCachingMode {
    /// Let the system choose (default).
    Automatic = 0,
    /// Uncached — writes go directly to disk.
    Uncached = 1,
    /// Cached — writes may be buffered.
    Cached = 2,
}

/// Disk image synchronization mode (macOS 12.3+).
#[repr(isize)]
#[derive(Debug, Clone, Copy)]
pub enum VZDiskImageSynchronizationMode {
    /// Full synchronization.
    Full = 1,
    /// No explicit sync — relies on caching mode.
    None = 2,
}

/// common configure of storage device attachment
pub trait VZStorageDeviceAttachment {
    fn id(&self) -> Id;
}

/// Builder for VZDiskImageStorageDeviceAttachment.
///
/// # Examples
///
/// ```ignore
/// let block_attachment = VZDiskImageStorageDeviceAttachmentBuilder::new()
///     .path("/path/to/disk.img")
///     .read_only(false)
///     .build()
///     .expect("failed to create disk attachment");
/// ```
pub struct VZDiskImageStorageDeviceAttachmentBuilder<Path, ReadOnly> {
    path: Path,
    read_only: ReadOnly,
    caching_mode: Option<VZDiskImageCachingMode>,
    sync_mode: Option<VZDiskImageSynchronizationMode>,
}

impl VZDiskImageStorageDeviceAttachmentBuilder<(), bool> {
    pub fn new() -> Self {
        VZDiskImageStorageDeviceAttachmentBuilder {
            path: (),
            read_only: true,
            caching_mode: None,
            sync_mode: None,
        }
    }
}

impl<Path, ReadOnly> VZDiskImageStorageDeviceAttachmentBuilder<Path, ReadOnly> {
    pub fn path<T: Into<String>>(
        self,
        path: T,
    ) -> VZDiskImageStorageDeviceAttachmentBuilder<String, ReadOnly> {
        VZDiskImageStorageDeviceAttachmentBuilder {
            path: path.into(),
            read_only: self.read_only,
            caching_mode: self.caching_mode,
            sync_mode: self.sync_mode,
        }
    }

    pub fn read_only(
        self,
        read_only: bool,
    ) -> VZDiskImageStorageDeviceAttachmentBuilder<Path, bool> {
        VZDiskImageStorageDeviceAttachmentBuilder {
            path: self.path,
            read_only,
            caching_mode: self.caching_mode,
            sync_mode: self.sync_mode,
        }
    }

    pub fn caching_mode(mut self, mode: VZDiskImageCachingMode) -> Self {
        self.caching_mode = Some(mode);
        self
    }

    pub fn sync_mode(mut self, mode: VZDiskImageSynchronizationMode) -> Self {
        self.sync_mode = Some(mode);
        self
    }
}

impl VZDiskImageStorageDeviceAttachmentBuilder<String, bool> {
    pub fn build(self) -> Result<VZDiskImageStorageDeviceAttachment, NSError> {
        let read_only = if self.read_only { YES } else { NO };
        if self.caching_mode.is_some() || self.sync_mode.is_some() {
            let caching = self.caching_mode.unwrap_or(VZDiskImageCachingMode::Automatic) as isize;
            let sync = self.sync_mode.unwrap_or(VZDiskImageSynchronizationMode::Full) as isize;
            unsafe {
                VZDiskImageStorageDeviceAttachment::new_with_modes(
                    self.path.as_str(),
                    read_only,
                    caching,
                    sync,
                )
            }
        } else {
            unsafe { VZDiskImageStorageDeviceAttachment::new(self.path.as_str(), read_only) }
        }
    }
}

impl Default for VZDiskImageStorageDeviceAttachmentBuilder<(), bool> {
    fn default() -> Self {
        Self::new()
    }
}

/// configure of disk image storage device attachment
pub struct VZDiskImageStorageDeviceAttachment(StrongPtr);

impl VZDiskImageStorageDeviceAttachment {
    unsafe fn new(
        path: &str,
        read_only: BOOL,
    ) -> Result<VZDiskImageStorageDeviceAttachment, NSError> {
        let i: Id = msg_send![class!(VZDiskImageStorageDeviceAttachment), alloc];
        let path_nsurl = NSURL::file_url_with_path(path, false);
        let mut error: Id = NIL;
        let p = StrongPtr::new(
            msg_send![i, initWithURL:*path_nsurl.0 readOnly:read_only error:&mut error],
        );
        if !error.is_null() {
            Err(NSError(StrongPtr::retain(error)))
        } else {
            Ok(VZDiskImageStorageDeviceAttachment(p))
        }
    }

    /// Create with explicit caching and synchronization modes (macOS 12.3+).
    unsafe fn new_with_modes(
        path: &str,
        read_only: BOOL,
        caching_mode: isize,
        sync_mode: isize,
    ) -> Result<VZDiskImageStorageDeviceAttachment, NSError> {
        let i: Id = msg_send![class!(VZDiskImageStorageDeviceAttachment), alloc];
        let path_nsurl = NSURL::file_url_with_path(path, false);
        let mut error: Id = NIL;
        let p = StrongPtr::new(
            msg_send![i, initWithURL:*path_nsurl.0 readOnly:read_only cachingMode:caching_mode synchronizationMode:sync_mode error:&mut error],
        );
        if !error.is_null() {
            Err(NSError(StrongPtr::retain(error)))
        } else {
            Ok(VZDiskImageStorageDeviceAttachment(p))
        }
    }
}

impl VZStorageDeviceAttachment for VZDiskImageStorageDeviceAttachment {
    fn id(&self) -> Id {
        *self.0
    }
}

/// configure of storage device
pub trait VZStorageDeviceConfiguration {
    fn id(&self) -> Id;
}

/// configure of storage device through the Virtio interface
pub struct VZVirtioBlockDeviceConfiguration(StrongPtr);

impl VZVirtioBlockDeviceConfiguration {
    pub fn new<T: VZStorageDeviceAttachment>(attachment: T) -> VZVirtioBlockDeviceConfiguration {
        unsafe {
            let i: Id = msg_send![class!(VZVirtioBlockDeviceConfiguration), alloc];
            let p = StrongPtr::new(msg_send![i, initWithAttachment:attachment.id()]);
            VZVirtioBlockDeviceConfiguration(p)
        }
    }
}

impl VZStorageDeviceConfiguration for VZVirtioBlockDeviceConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}
