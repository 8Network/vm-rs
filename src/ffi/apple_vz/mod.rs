//! Apple Virtualization.framework FFI bindings.
//!
//! Based on virtualization-rs by Sotetsu Suzugamine (MIT License).
//! Original: https://github.com/suzusuzu/virtualization-rs
//! See LICENSE-virtualization-rs.md for the original license.
//!
//! # Safety
//!
//! All `unsafe` blocks in this module send Objective-C messages to VZ framework
//! objects via the `objc` crate's `msg_send!` macro. The safety invariants are:
//! - Object pointers (`Id`) are valid and retained via `StrongPtr`
//! - Selector names match the Apple VZ framework API exactly
//! - Argument types match the Objective-C method signatures
//! - Return types match the Objective-C method signatures

#![allow(improper_ctypes)]

pub mod base;
pub mod boot_loader;
pub mod entropy_device;
pub mod memory_device;
pub mod network_device;
pub mod serial_port;
pub mod shared_directory;
pub mod socket_device;
pub mod storage_device;
pub mod virtual_machine;
