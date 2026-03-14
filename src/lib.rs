//! vm-rs — Cross-platform VM lifecycle management.
//!
//! Provides a unified interface to create, boot, stop, and manage virtual machines
//! across macOS (Apple Virtualization.framework) and Linux (Cloud Hypervisor).
//!
//! # Architecture
//!
//! ```text
//! VmDriver trait        ← Platform-agnostic VM lifecycle
//!   ├── AppleVzDriver   ← macOS: Apple Virtualization.framework
//!   └── CloudHvDriver   ← Linux: Cloud Hypervisor REST API
//!
//! VmManager             ← Multi-VM orchestration (auto-selects driver)
//! NetworkSwitch         ← L2 userspace Ethernet switch with MAC learning
//! OciStore + OciPull    ← Content-addressable OCI image management
//! Setup                 ← Cloud-init seed ISO, disk CoW, asset download
//! ```

pub mod config;
pub mod driver;
pub mod ffi;
pub mod network;
pub mod oci;
pub mod setup;
pub mod vm;

// Re-exports for convenience
#[cfg(unix)]
pub use config::VmSocketEndpoint;
pub use config::{
    NetworkAttachment, SharedDir, VmConfig, VmHandle, VmState, VmmProcess,
    READY_MARKER,
};
pub use driver::{VmDriver, VmError};
pub use vm::VmManager;
