//! Linux bridge and TAP device management.
//!
//! Creates Linux bridges for inter-VM networking and TAP devices for
//! connecting VMs to bridges. Also configures iptables NAT for internet access.
//!
//! These operations require root privileges.

use std::process::{Command, Stdio};

use crate::driver::VmError;

fn validate_interface_name(name: &str) -> Result<(), VmError> {
    if name.is_empty() || name.len() > 15 {
        return Err(VmError::InvalidConfig(format!(
            "interface name must be 1-15 characters, got {} characters",
            name.len()
        )));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(VmError::InvalidConfig(format!(
            "interface name '{}' contains invalid characters (only alphanumeric, hyphen, underscore allowed)",
            name
        )));
    }
    Ok(())
}

/// Ensure a Linux bridge exists with the given gateway IP.
///
/// Idempotent — if the bridge already exists, only ensures it's up with the right IP.
pub fn ensure_bridge(name: &str, gateway_ip: &str, subnet_cidr: &str) -> Result<(), VmError> {
    validate_interface_name(name)?;
    // Create bridge if it doesn't exist
    if !link_exists(name) {
        run_ip(&["link", "add", name, "type", "bridge"])?;
        tracing::info!(bridge = %name, "created bridge");
    }

    // Bring up and assign IP
    run_ip(&["link", "set", name, "up"])?;

    // Check if IP already assigned
    let addr_output = Command::new("ip")
        .args(["addr", "show", "dev", name])
        .output()
        .map_err(|e| VmError::Hypervisor(format!("failed to check bridge address: {}", e)))?;
    let addr_str = String::from_utf8_lossy(&addr_output.stdout);
    let cidr = format!(
        "{}/{}",
        gateway_ip,
        subnet_cidr.split('/').next_back().unwrap_or("24")
    );
    if !addr_str.contains(&cidr) {
        run_ip(&["addr", "add", &cidr, "dev", name])?;
    }

    // Enable IP forwarding
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").map_err(|e| {
        VmError::Hypervisor(format!(
            "failed to enable IPv4 forwarding for bridge '{}': {}",
            name, e
        ))
    })?;

    // Add iptables MASQUERADE for the subnet
    setup_nat(name, subnet_cidr)?;

    tracing::info!(bridge = %name, gateway = %gateway_ip, "bridge ready");
    Ok(())
}

/// Create a TAP device.
pub fn create_tap(name: &str) -> Result<(), VmError> {
    validate_interface_name(name)?;
    run_ip(&["tuntap", "add", "dev", name, "mode", "tap"])?;
    run_ip(&["link", "set", name, "up"])?;
    tracing::debug!(tap = %name, "TAP device created");
    Ok(())
}

/// Add a TAP device to a bridge.
pub fn add_to_bridge(tap: &str, bridge: &str) -> Result<(), VmError> {
    validate_interface_name(tap)?;
    run_ip(&["link", "set", tap, "master", bridge])?;
    tracing::debug!(tap = %tap, bridge = %bridge, "TAP added to bridge");
    Ok(())
}

/// Delete a TAP device (best-effort, does not error if device doesn't exist).
pub fn delete_tap(name: &str) {
    let status = Command::new("ip")
        .args(["tuntap", "del", "dev", name, "mode", "tap"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => tracing::debug!(tap = %name, "TAP device deleted"),
        _ => tracing::debug!(tap = %name, "TAP device cleanup (may not exist)"),
    }
}

/// Delete a bridge (best-effort).
pub fn delete_bridge(name: &str) {
    if let Err(e) = Command::new("ip")
        .args(["link", "set", name, "down"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        tracing::warn!(bridge = %name, "failed to bring bridge down: {}", e);
    }
    match Command::new("ip")
        .args(["link", "del", name, "type", "bridge"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {
            tracing::info!(bridge = %name, "bridge deleted");
        }
        Ok(s) => {
            tracing::warn!(bridge = %name, exit = %s, "bridge deletion failed (may not exist)");
        }
        Err(e) => {
            tracing::error!(bridge = %name, "failed to run ip command for bridge deletion: {}", e);
        }
    }
}

/// Set up iptables MASQUERADE for a subnet behind a bridge.
fn setup_nat(bridge: &str, subnet: &str) -> Result<(), VmError> {
    // Check if rule already exists
    let check = Command::new("iptables")
        .args([
            "-t", "nat", "-C", "POSTROUTING",
            "-s", subnet, "!", "-o", bridge,
            "-j", "MASQUERADE",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if let Ok(s) = check {
        if s.success() {
            return Ok(()); // Rule already exists
        }
    }

    // Add the rule
    let status = Command::new("iptables")
        .args([
            "-t", "nat", "-A", "POSTROUTING",
            "-s", subnet, "!", "-o", bridge,
            "-j", "MASQUERADE",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| VmError::Hypervisor(format!("failed to add iptables NAT rule: {}", e)))?;

    if !status.success() {
        return Err(VmError::Hypervisor(format!(
            "iptables MASQUERADE rule failed (exit {}). Are you running as root?",
            status
        )));
    }

    Ok(())
}

/// Check if a network link exists.
fn link_exists(name: &str) -> bool {
    match Command::new("ip")
        .args(["link", "show", "dev", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) => s.success(),
        Err(e) => {
            tracing::warn!(link = %name, "failed to check if link exists: {}", e);
            false
        }
    }
}

/// Run an `ip` command and return an error if it fails.
fn run_ip(args: &[&str]) -> Result<(), VmError> {
    let output = Command::new("ip")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| VmError::Hypervisor(format!("failed to run ip {}: {}", args.join(" "), e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "RTNETLINK answers: File exists" is fine for idempotent operations
        if stderr.contains("File exists") {
            return Ok(());
        }
        return Err(VmError::Hypervisor(format!(
            "ip {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}
