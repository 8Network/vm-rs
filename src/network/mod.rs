//! VM networking — L2 virtual switch, Linux bridge, port forwarding.
//!
//! Two networking backends:
//! - **macOS**: `NetworkSwitch` — userspace L2 learning bridge via socketpairs
//! - **Linux**: `bridge` module — Linux kernel bridge + TAP devices + iptables NAT

#[cfg(target_os = "linux")]
pub mod bridge;
pub mod port_forward;
#[cfg(unix)]
pub mod switch;

pub use port_forward::PortForwarder;
#[cfg(unix)]
pub use switch::NetworkSwitch;
