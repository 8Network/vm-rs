//! VM networking — L2 virtual switch, Linux bridge, port forwarding.
//!
//! Two networking backends:
//! - **macOS**: `NetworkSwitch` — userspace L2 learning bridge via socketpairs
//! - **Linux**: `bridge` module — Linux kernel bridge + TAP devices + iptables NAT

pub mod switch;
pub mod port_forward;
pub mod bridge;

pub use switch::NetworkSwitch;
pub use port_forward::PortForwarder;
