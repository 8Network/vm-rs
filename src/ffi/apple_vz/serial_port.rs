//! serial port module

use super::base::{Id, NSError, NSFileHandle, NIL};

use objc::rc::StrongPtr;
use objc::runtime::Sel;
use objc::{class, msg_send, sel, sel_impl};

/// common configure for serial port attachment
pub trait VZSerialPortAttachment {
    fn id(&self) -> Id;
}

/// Builder for VZFileHandleSerialPortAttachment.
///
/// # Examples
///
/// ```ignore
/// let attachment = VZFileHandleSerialPortAttachmentBuilder::new()
///     .file_handle_for_reading(read_handle)
///     .file_handle_for_writing(write_handle)
///     .build();
/// ```
pub struct VZFileHandleSerialPortAttachmentBuilder<R, W> {
    file_handle_for_reading: R,
    file_handle_for_writing: W,
}

impl Default for VZFileHandleSerialPortAttachmentBuilder<(), ()> {
    fn default() -> Self {
        Self::new()
    }
}

impl VZFileHandleSerialPortAttachmentBuilder<(), ()> {
    pub fn new() -> Self {
        VZFileHandleSerialPortAttachmentBuilder {
            file_handle_for_reading: (),
            file_handle_for_writing: (),
        }
    }
}

impl<R, W> VZFileHandleSerialPortAttachmentBuilder<R, W> {
    pub fn file_handle_for_reading(
        self,
        file_handle_for_reading: NSFileHandle,
    ) -> VZFileHandleSerialPortAttachmentBuilder<NSFileHandle, W> {
        VZFileHandleSerialPortAttachmentBuilder {
            file_handle_for_reading,
            file_handle_for_writing: self.file_handle_for_writing,
        }
    }

    pub fn file_handle_for_writing(
        self,
        file_handle_for_writing: NSFileHandle,
    ) -> VZFileHandleSerialPortAttachmentBuilder<R, NSFileHandle> {
        VZFileHandleSerialPortAttachmentBuilder {
            file_handle_for_reading: self.file_handle_for_reading,
            file_handle_for_writing,
        }
    }
}

impl VZFileHandleSerialPortAttachmentBuilder<NSFileHandle, NSFileHandle> {
    pub fn build(self) -> VZFileHandleSerialPortAttachment {
        unsafe {
            VZFileHandleSerialPortAttachment::new(
                self.file_handle_for_reading,
                self.file_handle_for_writing,
            )
        }
    }
}


/// thie struct configure a serial port
pub struct VZFileHandleSerialPortAttachment(StrongPtr);

impl VZFileHandleSerialPortAttachment {
    unsafe fn new(
        file_handle_for_reading: NSFileHandle,
        file_handle_for_writing: NSFileHandle,
    ) -> VZFileHandleSerialPortAttachment {
        let i: Id = msg_send![class!(VZFileHandleSerialPortAttachment), alloc];
        let p = StrongPtr::new(
            msg_send![i, initWithFileHandleForReading:*file_handle_for_reading.0 fileHandleForWriting:*file_handle_for_writing.0],
        );
        VZFileHandleSerialPortAttachment(p)
    }
}

impl VZSerialPortAttachment for VZFileHandleSerialPortAttachment {
    fn id(&self) -> Id {
        *self.0
    }
}

/// configure of serial port
pub trait VZSerialPortConfiguration {
    fn id(&self) -> Id;
}

/// configure of serial port through the Virtio interface
pub struct VZVirtioConsoleDeviceSerialPortConfiguration(StrongPtr);

impl VZVirtioConsoleDeviceSerialPortConfiguration {
    pub fn new<T: VZSerialPortAttachment>(
        attachement: T,
    ) -> VZVirtioConsoleDeviceSerialPortConfiguration {
        unsafe {
            let p = StrongPtr::new(msg_send![
                class!(VZVirtioConsoleDeviceSerialPortConfiguration),
                new
            ]);
            let _: Id = msg_send![*p, setAttachment: attachement.id()];
            VZVirtioConsoleDeviceSerialPortConfiguration(p)
        }
    }
}

impl VZSerialPortConfiguration for VZVirtioConsoleDeviceSerialPortConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}

// ─── File-based Serial Port (macOS 14+) ─────────────────────────────────

/// File-based serial port attachment (macOS 14+).
///
/// Simpler than `VZFileHandleSerialPortAttachment` — just point at a file path.
/// No need to manage raw file descriptors or `NSFileHandle` objects.
///
/// # Examples
///
/// ```ignore
/// let attachment = VZFileSerialPortAttachment::new("/var/log/vm/serial.log", false)?;
/// let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new(attachment);
/// ```
pub struct VZFileSerialPortAttachment(StrongPtr);

impl VZFileSerialPortAttachment {
    /// Create a file serial port attachment.
    ///
    /// `path`: file path for serial output.
    /// `append`: if true, append to existing file; if false, overwrite.
    /// Handles macOS 14-15 (`initWithURL:shouldAppend:error:`) and
    /// macOS 16+ (`initWithURL:append:error:`) selectors.
    ///
    /// Returns `Err(NSError)` if the framework rejects the path (e.g. directory does
    /// not exist, permission denied).
    pub fn new(path: &str, append: bool) -> Result<Self, NSError> {
        unsafe {
            let url = super::base::NSURL::file_url_with_path(path, false);
            let append_bool = if append {
                objc::runtime::YES
            } else {
                objc::runtime::NO
            };
            let cls = class!(VZFileSerialPortAttachment);
            let alloc: Id = msg_send![cls, alloc];
            let mut error: Id = NIL;

            // macOS 16+ renamed shouldAppend → append
            let new_sel = Sel::register("initWithURL:append:error:");
            let has_new: bool =
                msg_send![cls, instancesRespondToSelector: new_sel];

            let p: Id = if has_new {
                msg_send![alloc, initWithURL:*url.0
                                 append:append_bool
                                 error:&mut error]
            } else {
                msg_send![alloc, initWithURL:*url.0
                                 shouldAppend:append_bool
                                 error:&mut error]
            };

            if p.is_null() {
                return Err(if error.is_null() {
                    NSError::from_description(
                        "VZFileSerialPortAttachment",
                        &format!("initWithURL failed for path: {path}"),
                    )
                } else {
                    NSError(StrongPtr::retain(error))
                });
            }

            Ok(VZFileSerialPortAttachment(StrongPtr::new(p)))
        }
    }
}

impl VZSerialPortAttachment for VZFileSerialPortAttachment {
    fn id(&self) -> Id {
        *self.0
    }
}
