//! network device module

use super::base::{Id, NSString};

use objc::rc::StrongPtr;
use objc::{class, msg_send, sel, sel_impl};

/// common behaviors for network device attachment
pub trait VZNetworkDeviceAttachment {
    fn id(&self) -> Id;
}

/// configure of NAT network device attachment
pub struct VZNATNetworkDeviceAttachment(StrongPtr);

impl Default for VZNATNetworkDeviceAttachment {
    fn default() -> Self {
        Self::new()
    }
}

impl VZNATNetworkDeviceAttachment {
    pub fn new() -> VZNATNetworkDeviceAttachment {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZNATNetworkDeviceAttachment), new]);
            VZNATNetworkDeviceAttachment(p)
        }
    }
}

impl VZNetworkDeviceAttachment for VZNATNetworkDeviceAttachment {
    fn id(&self) -> Id {
        *self.0
    }
}

/// common behaviors for bridge network interface
pub trait VZBridgedNetworkInterface {
    fn id(&self) -> Id;
    fn localized_display_name(&self) -> NSString {
        let _obj = self.id();
        let p = unsafe { StrongPtr::retain(msg_send![class!(_obj), localizedDisplayName]) };
        NSString(p)
    }
    fn identifier(&self) -> NSString {
        let _obj = self.id();
        let p = unsafe { StrongPtr::retain(msg_send![class!(_obj), identifier]) };
        NSString(p)
    }
}

/// configure of bridge network device attachment
pub struct VZBridgedNetworkDeviceAttachment(StrongPtr);

impl VZBridgedNetworkDeviceAttachment {
    pub fn new<T: VZBridgedNetworkInterface>(interface: T) -> VZBridgedNetworkDeviceAttachment {
        unsafe {
            let obj: Id = msg_send![class!(VZBridgedNetworkDeviceAttachment), alloc];
            let p = StrongPtr::new(msg_send![obj, initWithInterface:interface.id()]);
            VZBridgedNetworkDeviceAttachment(p)
        }
    }
}

impl VZNetworkDeviceAttachment for VZBridgedNetworkDeviceAttachment {
    fn id(&self) -> Id {
        *self.0
    }
}

/// Network device attachment using file handles for custom networking.
///
/// Each VM gets a file handle (Unix datagram socket). The host process reads/writes
/// raw Ethernet frames through this socket, enabling userspace switching.
pub struct VZFileHandleNetworkDeviceAttachment(StrongPtr);

impl VZFileHandleNetworkDeviceAttachment {
    /// Create a file-handle-based network attachment from a file handle.
    ///
    /// The file handle should be one end of a Unix datagram socketpair.
    /// Each recv/send on the socket is one raw Ethernet frame.
    pub fn new(file_handle: super::base::NSFileHandle) -> VZFileHandleNetworkDeviceAttachment {
        unsafe {
            let obj: Id = msg_send![class!(VZFileHandleNetworkDeviceAttachment), alloc];
            let p = StrongPtr::new(msg_send![obj, initWithFileHandle:*file_handle.0]);
            VZFileHandleNetworkDeviceAttachment(p)
        }
    }
}

impl VZNetworkDeviceAttachment for VZFileHandleNetworkDeviceAttachment {
    fn id(&self) -> Id {
        *self.0
    }
}

/// MAC address
pub struct VZMACAddress(pub StrongPtr);

impl Default for VZMACAddress {
    fn default() -> Self {
        Self::new()
    }
}

impl VZMACAddress {
    pub fn new() -> VZMACAddress {
        let p = unsafe { StrongPtr::new(msg_send![class!(VZMACAddress), new]) };
        VZMACAddress(p)
    }
    pub fn random_locally_administered_address() -> VZMACAddress {
        let p = unsafe {
            StrongPtr::new(msg_send![
                class!(VZMACAddress),
                randomLocallyAdministeredAddress
            ])
        };
        VZMACAddress(p)
    }

    pub fn init_with_string(s: &str) -> VZMACAddress {
        let string = NSString::new(s);
        let p =
            unsafe { StrongPtr::new(msg_send![class!(VZMACAddress), initWithString:*string.0]) };
        VZMACAddress(p)
    }
}

/// common configure of network device
pub trait VZNetworkDeviceConfiguration {
    fn id(&self) -> Id;
}

/// configure of network device through the Virtio interface
pub struct VZVirtioNetworkDeviceConfiguration(StrongPtr);

impl VZVirtioNetworkDeviceConfiguration {
    pub fn new<T: VZNetworkDeviceAttachment>(attachment: T) -> VZVirtioNetworkDeviceConfiguration {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZVirtioNetworkDeviceConfiguration), new]);
            let _: Id = msg_send![*p, setAttachment:attachment.id()];
            VZVirtioNetworkDeviceConfiguration(p)
        }
    }

    pub fn set_attachment<T: VZNetworkDeviceAttachment>(&mut self, attachment: T) {
        unsafe {
            let _: Id = msg_send![*self.0, setAttachment:attachment.id()];
        }
    }

    pub fn set_mac_address(&mut self, mac: VZMACAddress) {
        unsafe {
            let _: Id = msg_send![*self.0, setMACAddress:*mac.0];
        }
    }
}

impl VZNetworkDeviceConfiguration for VZVirtioNetworkDeviceConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}
