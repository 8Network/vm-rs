//! virtual machine module

use super::{
    base::{Id, NSArray, NSError, NIL},
    boot_loader::VZBootLoader,
    entropy_device::VZEntropyDeviceConfiguration,
    memory_device::VZMemoryBalloonDeviceConfiguration,
    network_device::VZNetworkDeviceConfiguration,
    serial_port::VZSerialPortConfiguration,
    shared_directory::VZDirectorySharingDeviceConfiguration,
    socket_device::VZSocketDeviceConfiguration,
    storage_device::VZStorageDeviceConfiguration,
};

use block::Block;
use objc::runtime::BOOL;
use objc::{class, msg_send, sel, sel_impl};
use objc::{rc::StrongPtr, runtime::YES};

/// Builder for VZVirtualMachineConfiguration.
///
/// # Examples
///
/// ```ignore
/// let conf = VZVirtualMachineConfigurationBuilder::new()
///     .boot_loader(boot_loader)
///     .cpu_count(4)
///     .memory_size(4 * 1024 * 1024 * 1024)
///     .entropy_devices(vec![entropy])
///     .memory_balloon_devices(vec![memory_balloon])
///     .network_devices(vec![network_device])
///     .serial_ports(vec![serial])
///     .storage_devices(vec![block_device])
///     .build();
/// ```
pub struct VZVirtualMachineConfigurationBuilder {
    conf: VZVirtualMachineConfiguration,
}

impl VZVirtualMachineConfigurationBuilder {
    pub fn new() -> Self {
        VZVirtualMachineConfigurationBuilder {
            conf: VZVirtualMachineConfiguration::new(),
        }
    }

    pub fn boot_loader<T: VZBootLoader>(mut self, boot_loader: T) -> Self {
        self.conf.set_boot_loader(boot_loader);
        self
    }

    pub fn cpu_count(mut self, cpu_count: usize) -> Self {
        self.conf.set_cpu_count(cpu_count);
        self
    }

    pub fn memory_size(mut self, memory_size: usize) -> Self {
        self.conf.set_memory_size(memory_size);
        self
    }

    pub fn entropy_devices<T: VZEntropyDeviceConfiguration>(
        mut self,
        entropy_devices: Vec<T>,
    ) -> Self {
        self.conf.set_entropy_devices(entropy_devices);
        self
    }

    pub fn memory_balloon_devices<T: VZMemoryBalloonDeviceConfiguration>(
        mut self,
        memory_balloon_devices: Vec<T>,
    ) -> Self {
        self.conf.set_memory_balloon_devices(memory_balloon_devices);
        self
    }

    pub fn network_devices<T: VZNetworkDeviceConfiguration>(
        mut self,
        network_devices: Vec<T>,
    ) -> Self {
        self.conf.set_network_devices(network_devices);
        self
    }

    pub fn serial_ports<T: VZSerialPortConfiguration>(mut self, serial_ports: Vec<T>) -> Self {
        self.conf.set_serial_ports(serial_ports);
        self
    }

    pub fn socket_devices<T: VZSocketDeviceConfiguration>(
        mut self,
        socket_devices: Vec<T>,
    ) -> Self {
        self.conf.set_socket_devices(socket_devices);
        self
    }

    pub fn storage_devices<T: VZStorageDeviceConfiguration>(
        mut self,
        storage_devices: Vec<T>,
    ) -> Self {
        self.conf.set_storage_devices(storage_devices);
        self
    }

    pub fn directory_sharing_devices<T: VZDirectorySharingDeviceConfiguration>(
        mut self,
        devices: Vec<T>,
    ) -> Self {
        self.conf.set_directory_sharing_devices(devices);
        self
    }

    pub fn build(self) -> VZVirtualMachineConfiguration {
        self.conf
    }
}

impl Default for VZVirtualMachineConfigurationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// configure of virtual machine
pub struct VZVirtualMachineConfiguration(StrongPtr);

impl VZVirtualMachineConfiguration {
    fn new() -> VZVirtualMachineConfiguration {
        unsafe {
            let obj = StrongPtr::new(msg_send![class!(VZVirtualMachineConfiguration), new]);
            VZVirtualMachineConfiguration(obj)
        }
    }

    fn set_boot_loader<T: VZBootLoader>(&mut self, boot_loader: T) {
        unsafe {
            let _: () = msg_send![*self.0, setBootLoader: boot_loader.id()];
        }
    }

    fn set_cpu_count(&mut self, cnt: usize) {
        unsafe {
            let _: () = msg_send![*self.0, setCPUCount: cnt];
        }
    }

    fn set_memory_size(&mut self, size: usize) {
        unsafe {
            let _: () = msg_send![*self.0, setMemorySize: size];
        }
    }

    fn set_entropy_devices<T: VZEntropyDeviceConfiguration>(&mut self, devices: Vec<T>) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setEntropyDevices:*arr.p];
        }
    }

    fn set_memory_balloon_devices<T: VZMemoryBalloonDeviceConfiguration>(
        &mut self,
        devices: Vec<T>,
    ) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setMemoryBalloonDevices:*arr.p];
        }
    }

    fn set_network_devices<T: VZNetworkDeviceConfiguration>(&mut self, devices: Vec<T>) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setNetworkDevices:*arr.p];
        }
    }

    fn set_serial_ports<T: VZSerialPortConfiguration>(&mut self, devices: Vec<T>) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setSerialPorts:*arr.p];
        }
    }

    fn set_socket_devices<T: VZSocketDeviceConfiguration>(&mut self, devices: Vec<T>) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setSocketDevices:*arr.p];
        }
    }

    fn set_storage_devices<T: VZStorageDeviceConfiguration>(&mut self, devices: Vec<T>) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setStorageDevices:*arr.p];
        }
    }

    fn set_directory_sharing_devices<T: VZDirectorySharingDeviceConfiguration>(
        &mut self,
        devices: Vec<T>,
    ) {
        let device_ids = devices.iter().map(|x| x.id()).collect();
        let arr: NSArray<T> = NSArray::array_with_objects(device_ids);
        unsafe {
            let _: () = msg_send![*self.0, setDirectorySharingDevices:*arr.p];
        }
    }

    pub fn validate_with_error(&self) -> Result<BOOL, NSError> {
        unsafe {
            let mut error: Id = NIL;
            let b: BOOL = msg_send![*self.0, validateWithError: &mut error];
            if !error.is_null() {
                Err(NSError(StrongPtr::retain(error)))
            } else {
                Ok(b)
            }
        }
    }
}

/// virtual machine
#[derive(Clone)]
pub struct VZVirtualMachine(StrongPtr);

// Safety: VZVirtualMachine wraps a reference-counted ObjC object.
// All mutations go through the VZ dispatch queue, making cross-thread sharing safe.
unsafe impl Send for VZVirtualMachine {}
unsafe impl Sync for VZVirtualMachine {}

/// state of virtual machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VZVirtualMachineState {
    /// Initial state before the virtual machine is started.
    VZVirtualMachineStateStopped,

    /// Running virtual machine.
    VZVirtualMachineStateRunning,

    /// A started virtual machine is paused. This state can only be transitioned from VZVirtualMachineStatePausing.
    VZVirtualMachineStatePaused,

    /// The virtual machine has encountered an internal error.
    VZVirtualMachineStateError,

    /// The virtual machine is configuring the hardware and starting.
    VZVirtualMachineStateStarting,

    /// The virtual machine is being paused. This is the intermediate state between VZVirtualMachineStateRunning and VZVirtualMachineStatePaused.
    VZVirtualMachineStatePausing,

    /// The virtual machine is being resumed. This is the intermediate state between VZVirtualMachineStatePaused and VZVirtualMachineStateRunning. */
    VZVirtualMachineStateResuming,

    /// Other
    Other,
}

impl VZVirtualMachine {
    pub fn new(conf: VZVirtualMachineConfiguration, queue: Id) -> VZVirtualMachine {
        unsafe {
            let i: Id = msg_send![class!(VZVirtualMachine), alloc];
            let p = StrongPtr::new(msg_send![i, initWithConfiguration:*conf.0 queue:queue]);
            VZVirtualMachine(p)
        }
    }

    pub fn start_with_completion_handler(&self, completion_handler: &Block<(Id,), ()>) {
        unsafe {
            let _: Id = msg_send![*self.0, startWithCompletionHandler: completion_handler];
        }
    }

    /// # Safety
    ///
    /// The underlying Objective-C `VZVirtualMachine` must still be valid and
    /// owned for the duration of the call.
    pub unsafe fn request_stop_with_error(&mut self) -> Result<bool, NSError> {
        let mut error: Id = NIL;
        let ret: BOOL = msg_send![*self.0, requestStopWithError: &mut error];
        if !error.is_null() {
            Err(NSError(StrongPtr::retain(error)))
        } else {
            Ok(ret)
        }
    }

    pub fn supported() -> bool {
        unsafe {
            let b: BOOL = msg_send![class!(VZVirtualMachine), isSupported];
            b == YES
        }
    }

    /// # Safety
    ///
    /// The underlying Objective-C `VZVirtualMachine` must still be valid and
    /// owned for the duration of the call.
    pub unsafe fn state(&self) -> VZVirtualMachineState {
        let n: isize = msg_send![*self.0, state];
        match n {
            0 => VZVirtualMachineState::VZVirtualMachineStateStopped,
            1 => VZVirtualMachineState::VZVirtualMachineStateRunning,
            2 => VZVirtualMachineState::VZVirtualMachineStatePaused,
            3 => VZVirtualMachineState::VZVirtualMachineStateError,
            4 => VZVirtualMachineState::VZVirtualMachineStateStarting,
            5 => VZVirtualMachineState::VZVirtualMachineStatePausing,
            6 => VZVirtualMachineState::VZVirtualMachineStateResuming,
            _ => VZVirtualMachineState::Other,
        }
    }
}
