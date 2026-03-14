use crate::driver::{VmDriver, VmError};

pub(super) fn create_platform_driver() -> Result<Box<dyn VmDriver>, VmError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(crate::driver::apple_vz::AppleVzDriver::new()))
    }

    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(crate::driver::cloud_hv::CloudHvDriver::new()))
    }

    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(crate::driver::whp::WhpDriver::new()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(VmError::Hypervisor(format!(
            "unsupported platform: {}",
            std::env::consts::OS
        )))
    }
}
