//! entropy device module

use super::base::Id;

use objc::rc::StrongPtr;
use objc::{class, msg_send, sel, sel_impl};

/// common configure of entropy device
pub trait VZEntropyDeviceConfiguration {
    fn id(&self) -> Id;
}

/// configure of entropy device
pub struct VZVirtioEntropyDeviceConfiguration(StrongPtr);

impl VZVirtioEntropyDeviceConfiguration {
    pub fn new() -> VZVirtioEntropyDeviceConfiguration {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZVirtioEntropyDeviceConfiguration), new]);
            VZVirtioEntropyDeviceConfiguration(p)
        }
    }
}

impl Default for VZVirtioEntropyDeviceConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

impl VZEntropyDeviceConfiguration for VZVirtioEntropyDeviceConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}
