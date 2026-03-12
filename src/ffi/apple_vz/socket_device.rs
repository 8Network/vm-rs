//! Virtio socket device — host↔guest communication via vsock (macOS 11+).
//!
//! vsock provides bidirectional, POSIX FD-based I/O between host and guest
//! without requiring any network configuration. Primary use cases:
//! - Agent commands and health checks
//! - File transfer
//! - Service proxying
//!
//! # Usage
//!
//! 1. Add `VZVirtioSocketDeviceConfiguration` to the VM configuration at boot.
//! 2. After the VM starts, get the runtime `VZVirtioSocketDevice` from the VM.
//! 3. Use `connect_to_port()` (host→guest) or `set_socket_listener_for_port()` (guest→host).
//! 4. Read/write on the resulting `VZVirtioSocketConnection::file_descriptor()`.

use super::base::Id;

use block::Block;
use objc::rc::StrongPtr;
use objc::{class, msg_send, sel, sel_impl};

/// Configuration trait for socket devices.
pub trait VZSocketDeviceConfiguration {
    fn id(&self) -> Id;
}

/// VirtIO socket device configuration (macOS 11+).
///
/// Add to VM configuration to enable vsock communication.
/// After the VM boots, access the runtime device via
/// `VZVirtualMachine::first_socket_device()`.
pub struct VZVirtioSocketDeviceConfiguration(StrongPtr);

impl Default for VZVirtioSocketDeviceConfiguration {
    fn default() -> Self {
        Self::new()
    }
}

impl VZVirtioSocketDeviceConfiguration {
    pub fn new() -> Self {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZVirtioSocketDeviceConfiguration), new]);
            VZVirtioSocketDeviceConfiguration(p)
        }
    }
}

impl VZSocketDeviceConfiguration for VZVirtioSocketDeviceConfiguration {
    fn id(&self) -> Id {
        *self.0
    }
}

/// Runtime vsock device — obtained from a running VM's `socketDevices` array.
///
/// Provides methods to connect to guest ports and listen for guest connections.
/// All methods must be called on the VM's dispatch queue.
pub struct VZVirtioSocketDevice(pub StrongPtr);

// SAFETY: VZVirtioSocketDevice wraps a reference-counted ObjC object.
// All access goes through the VZ dispatch queue.
unsafe impl Send for VZVirtioSocketDevice {}

impl VZVirtioSocketDevice {
    /// Connect to a port on the guest (host-initiated connection).
    ///
    /// The completion handler receives `(VZVirtioSocketConnection?, NSError?)`.
    /// On success, the first argument is a valid connection object.
    /// On failure, the second argument is a non-nil NSError.
    ///
    /// # Safety
    /// Must be called on the VM's dispatch queue.
    pub fn connect_to_port(&self, port: u32, completion_handler: &Block<(Id, Id), ()>) {
        unsafe {
            let _: () =
                msg_send![*self.0, connectToPort:port completionHandler:completion_handler];
        }
    }

    /// Register a socket listener for a specific port (guest-initiated connections).
    ///
    /// When the guest connects to this port, the listener's delegate
    /// determines whether to accept the connection.
    ///
    /// # Safety
    /// Must be called on the VM's dispatch queue.
    pub fn set_socket_listener_for_port(&self, listener: &VZVirtioSocketListener, port: u32) {
        unsafe {
            let _: () = msg_send![*self.0, setSocketListener:*listener.0 forPort:port];
        }
    }

    /// Remove the socket listener for a specific port.
    ///
    /// # Safety
    /// Must be called on the VM's dispatch queue.
    pub fn remove_socket_listener_for_port(&self, port: u32) {
        unsafe {
            let _: () = msg_send![*self.0, removeSocketListenerForPort:port];
        }
    }
}

/// Listener for incoming guest vsock connections.
///
/// Create a listener and register it on a `VZVirtioSocketDevice` for a specific port.
/// By default, all incoming connections are accepted. Set a delegate to control
/// which connections to accept (requires Objective-C runtime class registration).
pub struct VZVirtioSocketListener(pub StrongPtr);

impl Default for VZVirtioSocketListener {
    fn default() -> Self {
        Self::new()
    }
}

impl VZVirtioSocketListener {
    pub fn new() -> Self {
        unsafe {
            let p = StrongPtr::new(msg_send![class!(VZVirtioSocketListener), new]);
            VZVirtioSocketListener(p)
        }
    }
}

/// An individual vsock connection.
///
/// Wraps a POSIX file descriptor for bidirectional I/O.
/// The FD is owned by the VZ framework — do NOT close it manually.
/// Use standard `read(2)` / `write(2)` on the file descriptor.
pub struct VZVirtioSocketConnection(pub StrongPtr);

// SAFETY: The connection's FD is thread-safe for read/write.
unsafe impl Send for VZVirtioSocketConnection {}

impl VZVirtioSocketConnection {
    /// Get the raw file descriptor for this connection.
    ///
    /// Standard POSIX `read(2)` / `write(2)` calls work on this FD.
    /// The FD is owned by the VZ framework — do NOT close it manually.
    pub fn file_descriptor(&self) -> i32 {
        unsafe { msg_send![*self.0, fileDescriptor] }
    }

    /// The destination port (guest-side port number).
    pub fn destination_port(&self) -> u32 {
        unsafe { msg_send![*self.0, destinationPort] }
    }

    /// The source port (host-side port number).
    pub fn source_port(&self) -> u32 {
        unsafe { msg_send![*self.0, sourcePort] }
    }
}
