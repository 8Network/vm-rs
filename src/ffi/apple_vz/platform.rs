//! Platform configuration — persistent VM identity and platform settings.
//!
//! `VZGenericPlatformConfiguration` (macOS 12+) provides a stable hardware
//! identity for Linux VMs. Combined with `VZGenericMachineIdentifier` (macOS 13+),
//! it ensures the guest OS sees the same machine across reboots.

use super::base::Id;

use objc::rc::StrongPtr;
use objc::{class, msg_send, sel, sel_impl};

/// Platform configuration trait.
pub trait VZPlatformConfiguration {
    fn id(&self) -> Id;
}

/// Generic platform configuration for Linux VMs (macOS 12+).
///
/// Provides a machine identifier that persists across reboots,
/// ensuring the guest OS sees a stable hardware identity.
pub struct VZGenericPlatformConfiguration(StrongPtr);

impl Default for VZGenericPlatformConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

impl VZGenericPlatformConfiguration {
    /// Create a new generic platform configuration.
    pub fn new() -> Self {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZGenericPlatformConfiguration), new]);
            VZGenericPlatformConfiguration(p)
        }
    }

    /// Set the machine identifier for persistent VM identity (macOS 13+).
    pub fn set_machine_identifier(&mut self, identifier: &VZGenericMachineIdentifier) {
        unsafe {
            let _: () = msg_send![*self.0, setMachineIdentifier:*identifier.0];
        }
    }
}

impl VZPlatformConfiguration for VZGenericPlatformConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}

/// Persistent machine identifier for generic platform VMs (macOS 13+).
///
/// Each VM should have a unique, persistent identifier. Save the
/// `data_representation()` bytes to disk and restore with `from_data()`
/// across reboots to maintain stable VM identity.
pub struct VZGenericMachineIdentifier(StrongPtr);

impl Default for VZGenericMachineIdentifier {
    fn default() -> Self {
        Self::new()
    }
}

impl VZGenericMachineIdentifier {
    /// Create a new random machine identifier.
    pub fn new() -> Self {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZGenericMachineIdentifier), new]);
            VZGenericMachineIdentifier(p)
        }
    }

    /// Restore a machine identifier from previously saved data.
    ///
    /// Returns `None` if the data is invalid.
    /// Use `data_representation()` to get the bytes to save.
    pub fn from_data(data: &[u8]) -> Option<Self> {
        unsafe {
            let nsdata_alloc: Id = msg_send![class!(NSData), alloc];
            let nsdata: Id = msg_send![nsdata_alloc, initWithBytes:data.as_ptr() length:data.len()];
            if nsdata.is_null() {
                return None;
            }
            let nsdata = StrongPtr::new(nsdata);
            let alloc: Id = msg_send![class!(VZGenericMachineIdentifier), alloc];
            let p: Id = msg_send![alloc, initWithDataRepresentation:*nsdata];
            if p.is_null() {
                None
            } else {
                Some(VZGenericMachineIdentifier(StrongPtr::new(p)))
            }
        }
    }

    /// Serialize the machine identifier for persistence.
    ///
    /// Save these bytes to disk and restore with `from_data()` across reboots.
    pub fn data_representation(&self) -> Vec<u8> {
        unsafe {
            let data: Id = msg_send![*self.0, dataRepresentation];
            if data.is_null() {
                return Vec::new();
            }
            let length: usize = msg_send![data, length];
            let bytes: *const u8 = msg_send![data, bytes];
            std::slice::from_raw_parts(bytes, length).to_vec()
        }
    }
}
