//! Userspace L2 Ethernet switch.
//!
//! Each "network" is a virtual broadcast domain. The switch reads raw Ethernet
//! frames from VM socketpairs and forwards them using learning-bridge logic:
//!
//! 1. Learn source MAC -> port mapping
//! 2. If destination MAC is known -> unicast to that port
//! 3. If unknown or broadcast -> flood to all ports on same network
//! 4. Never forward between different networks

use std::collections::HashMap;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Minimum Ethernet frame size (without FCS).
const MIN_FRAME_SIZE: usize = 14;
/// Maximum Ethernet frame size (jumbo not supported).
const MAX_FRAME_SIZE: usize = 1518;

/// A 6-byte MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    pub fn from_bytes(bytes: &[u8]) -> Option<MacAddress> {
        if bytes.len() < 6 {
            return None;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&bytes[..6]);
        Some(MacAddress(mac))
    }

    pub fn is_broadcast(&self) -> bool {
        self.0 == [0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    }

    pub fn is_multicast(&self) -> bool {
        self.0[0] & 0x01 != 0
    }
}

impl std::fmt::Display for MacAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// A port on the switch -- one end of a Unix datagram socketpair connected to a VM's NIC.
#[derive(Debug)]
pub struct SwitchPort {
    /// The switch's end of the socketpair. The other end goes to the VM.
    pub fd: RawFd,
    /// Which network this port belongs to.
    pub network_id: String,
    /// Human-readable label (service name).
    pub label: String,
}

impl AsRawFd for SwitchPort {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

/// MAC address table: maps MAC -> (port index, last-seen timestamp).
type MacTable = HashMap<MacAddress, (usize, Instant)>;

/// The L2 switch. Owns ports grouped by network, runs a forwarding loop.
pub struct NetworkSwitch {
    networks: Arc<Mutex<HashMap<String, Vec<SwitchPort>>>>,
    mac_tables: Arc<RwLock<HashMap<String, MacTable>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for NetworkSwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkSwitch {
    pub fn new() -> Self {
        Self {
            networks: Arc::new(Mutex::new(HashMap::new())),
            mac_tables: Arc::new(RwLock::new(HashMap::new())),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Add a port to a network. Returns the VM's end of the socketpair fd.
    ///
    /// Creates a Unix SOCK_DGRAM socketpair. One end is kept by the switch (for
    /// reading/writing Ethernet frames), the other is returned so the caller can
    /// pass it to VZFileHandleNetworkDeviceAttachment.
    pub fn add_port(&self, network_id: &str, label: &str) -> io::Result<RawFd> {
        let (switch_fd, vm_fd) = create_socketpair()?;

        let port = SwitchPort {
            fd: switch_fd,
            network_id: network_id.to_string(),
            label: label.to_string(),
        };

        let mut networks = self
            .networks
            .lock()
            .map_err(|e| io::Error::other(format!("lock poisoned: {}", e)))?;
        networks
            .entry(network_id.to_string())
            .or_default()
            .push(port);

        let mut mac_tables = self
            .mac_tables
            .write()
            .map_err(|e| io::Error::other(format!("lock poisoned: {}", e)))?;
        mac_tables.entry(network_id.to_string()).or_default();

        Ok(vm_fd)
    }

    /// Start the forwarding loop on a background thread.
    ///
    /// Uses poll(2) to watch all switch-side fds. When a frame arrives on any port,
    /// it's forwarded according to learning-bridge rules.
    pub fn start(&self) -> io::Result<()> {
        use std::sync::atomic::Ordering;

        if self.running.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.running.store(true, Ordering::SeqCst);

        let networks = Arc::clone(&self.networks);
        let mac_tables = Arc::clone(&self.mac_tables);
        let running = Arc::clone(&self.running);

        std::thread::Builder::new()
            .name("network-switch".to_string())
            .spawn(move || {
                forwarding_loop(&networks, &mac_tables, &running);
            })?;

        Ok(())
    }

    /// Stop the forwarding loop.
    pub fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for NetworkSwitch {
    fn drop(&mut self) {
        self.stop();
        match self.networks.lock() {
            Ok(networks) => {
                for ports in networks.values() {
                    for port in ports {
                        // SAFETY: Closing file descriptors we created in add_port().
                        let ret = unsafe { libc::close(port.fd) };
                        if ret != 0 {
                            // Log but don't panic in Drop — best we can do.
                            let err = std::io::Error::last_os_error();
                            eprintln!("vm-rs: failed to close switch port fd {}: {}", port.fd, err);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("vm-rs: network switch lock poisoned during drop: {}", e);
            }
        }
    }
}

/// Create a Unix SOCK_DGRAM socketpair. Returns (switch_fd, vm_fd).
fn create_socketpair() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: Standard POSIX socketpair call with valid fd array.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    // Set switch side non-blocking for the poll loop.
    // SAFETY: fcntl on file descriptors we just created in the socketpair above.
    unsafe {
        let flags = libc::fcntl(fds[0], libc::F_GETFL);
        if flags == -1 {
            let err = io::Error::last_os_error();
            libc::close(fds[0]);
            libc::close(fds[1]);
            return Err(err);
        }
        if libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            let err = io::Error::last_os_error();
            libc::close(fds[0]);
            libc::close(fds[1]);
            return Err(err);
        }
    }

    Ok((fds[0], fds[1]))
}

/// The main forwarding loop. Runs until `running` is set to false.
fn forwarding_loop(
    networks: &Mutex<HashMap<String, Vec<SwitchPort>>>,
    mac_tables: &RwLock<HashMap<String, MacTable>>,
    running: &std::sync::atomic::AtomicBool,
) {
    use std::sync::atomic::Ordering;

    const MAC_AGE_INTERVAL_SECS: u64 = 30;
    const MAC_ENTRY_LIFETIME_SECS: u64 = 120;

    let mut buf = [0u8; MAX_FRAME_SIZE];
    let mut last_aged = Instant::now();

    while running.load(Ordering::Relaxed) {
        // Periodic MAC aging: sweep entries older than 120s every 30s
        if last_aged.elapsed().as_secs() >= MAC_AGE_INTERVAL_SECS {
            if let Ok(mut tables) = mac_tables.write() {
                for table in tables.values_mut() {
                    table.retain(|_mac, (_port, ts)| {
                        ts.elapsed().as_secs() < MAC_ENTRY_LIFETIME_SECS
                    });
                }
            }
            last_aged = Instant::now();
        }

        // Build poll fds and snapshot port fds
        let nets = match networks.lock() {
            Ok(n) => n,
            Err(_) => break, // Lock poisoned, bail out
        };
        let mut pollfds: Vec<libc::pollfd> = Vec::new();
        let mut fd_map: Vec<(String, usize)> = Vec::new();
        let mut port_fds: HashMap<String, Vec<RawFd>> = HashMap::new();

        for (net_id, ports) in nets.iter() {
            let fds: Vec<RawFd> = ports.iter().map(|p| p.fd).collect();
            port_fds.insert(net_id.clone(), fds);
            for (idx, port) in ports.iter().enumerate() {
                pollfds.push(libc::pollfd {
                    fd: port.fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                fd_map.push((net_id.clone(), idx));
            }
        }
        drop(nets);

        if pollfds.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }

        // SAFETY: poll(2) on fds we own. pollfds array is valid and properly sized.
        let ready = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 50) };

        if ready <= 0 {
            continue;
        }

        for (i, pfd) in pollfds.iter().enumerate() {
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }

            let (ref net_id, src_port_idx) = fd_map[i];

            // Read one Ethernet frame
            // SAFETY: Reading from our own fd into a stack buffer of MAX_FRAME_SIZE.
            let n =
                unsafe { libc::recv(pfd.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };

            if n < MIN_FRAME_SIZE as isize {
                continue;
            }
            let frame = &buf[..n as usize];

            // Parse Ethernet header: dst MAC (6 bytes) + src MAC (6 bytes)
            let dst_mac = match MacAddress::from_bytes(&frame[0..6]) {
                Some(m) => m,
                None => continue,
            };
            let src_mac = match MacAddress::from_bytes(&frame[6..12]) {
                Some(m) => m,
                None => continue,
            };

            // Learn: src MAC -> src port
            if let Ok(mut tables) = mac_tables.write() {
                if let Some(table) = tables.get_mut(net_id.as_str()) {
                    table.insert(src_mac, (src_port_idx, Instant::now()));
                }
            }

            // Forward
            let fds = match port_fds.get(net_id.as_str()) {
                Some(f) => f,
                None => continue,
            };

            if dst_mac.is_broadcast() || dst_mac.is_multicast() {
                // Flood to all ports on same network except source
                for (idx, &fd) in fds.iter().enumerate() {
                    if idx == src_port_idx {
                        continue;
                    }
                    send_frame(fd, frame);
                }
            } else {
                // Unicast: check MAC table
                let dst_port = match mac_tables.read() {
                    Ok(tables) => tables
                        .get(net_id.as_str())
                        .and_then(|t| t.get(&dst_mac))
                        .map(|(port_idx, _ts)| *port_idx),
                    Err(_) => {
                        // Lock poisoned in the forwarding hot path — flood as fallback
                        // rather than crashing the switch thread.
                        tracing::error!("MAC table read lock poisoned, flooding frame");
                        None
                    }
                };

                if let Some(dst_idx) = dst_port {
                    if dst_idx != src_port_idx && dst_idx < fds.len() {
                        send_frame(fds[dst_idx], frame);
                    }
                } else {
                    // Unknown destination -- flood
                    for (idx, &fd) in fds.iter().enumerate() {
                        if idx == src_port_idx {
                            continue;
                        }
                        send_frame(fd, frame);
                    }
                }
            }
        }
    }
}

/// Send a single Ethernet frame to a socket fd. Best-effort (drops if buffer full).
fn send_frame(fd: RawFd, frame: &[u8]) {
    // SAFETY: Sending to our own fd. MSG_DONTWAIT prevents blocking on full buffer.
    unsafe {
        libc::send(
            fd,
            frame.as_ptr() as *const libc::c_void,
            frame.len(),
            libc::MSG_DONTWAIT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MacAddress ───────────────────────────────────────────────────────

    #[test]
    fn mac_from_bytes_valid() {
        let mac = MacAddress::from_bytes(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]).unwrap();
        assert_eq!(mac.0, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }

    #[test]
    fn mac_from_bytes_too_short() {
        assert!(MacAddress::from_bytes(&[0xaa, 0xbb]).is_none());
    }

    #[test]
    fn mac_from_bytes_empty() {
        assert!(MacAddress::from_bytes(&[]).is_none());
    }

    #[test]
    fn mac_from_bytes_extra_bytes_ignored() {
        let mac = MacAddress::from_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        assert_eq!(mac.0, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn mac_broadcast() {
        let mac = MacAddress([0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert!(mac.is_broadcast());
        assert!(mac.is_multicast()); // broadcast is also multicast
    }

    #[test]
    fn mac_not_broadcast() {
        let mac = MacAddress([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert!(!mac.is_broadcast());
    }

    #[test]
    fn mac_multicast() {
        // LSB of first octet set = multicast
        let mac = MacAddress([0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]);
        assert!(mac.is_multicast());
        assert!(!mac.is_broadcast());
    }

    #[test]
    fn mac_unicast() {
        // LSB of first octet clear = unicast
        let mac = MacAddress([0x02, 0x42, 0xac, 0x11, 0x00, 0x02]);
        assert!(!mac.is_multicast());
        assert!(!mac.is_broadcast());
    }

    #[test]
    fn mac_display_format() {
        let mac = MacAddress([0x02, 0x42, 0xac, 0x11, 0x00, 0x02]);
        assert_eq!(format!("{}", mac), "02:42:ac:11:00:02");
    }

    #[test]
    fn mac_display_zero() {
        let mac = MacAddress([0, 0, 0, 0, 0, 0]);
        assert_eq!(format!("{}", mac), "00:00:00:00:00:00");
    }

    // ── NetworkSwitch ────────────────────────────────────────────────────

    #[test]
    fn switch_add_port_returns_fd() {
        let switch = NetworkSwitch::new();
        let vm_fd = switch.add_port("net0", "web").unwrap();
        assert!(vm_fd >= 0);
    }

    #[test]
    fn switch_add_multiple_ports_same_network() {
        let switch = NetworkSwitch::new();
        let fd1 = switch.add_port("net0", "web").unwrap();
        let fd2 = switch.add_port("net0", "db").unwrap();
        assert_ne!(fd1, fd2);
    }

    #[test]
    fn switch_add_ports_different_networks() {
        let switch = NetworkSwitch::new();
        let fd1 = switch.add_port("frontend", "web").unwrap();
        let fd2 = switch.add_port("backend", "db").unwrap();
        assert_ne!(fd1, fd2);
    }

    #[test]
    fn switch_frame_delivery_same_network() {
        let switch = NetworkSwitch::new();
        let fd1 = switch.add_port("net0", "sender").unwrap();
        let fd2 = switch.add_port("net0", "receiver").unwrap();
        switch.start().unwrap();

        // Build a minimal Ethernet frame: dst(6) + src(6) + ethertype(2) = 14 bytes
        let mut frame = [0u8; 14];
        // Broadcast destination
        frame[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        // Source MAC
        frame[6..12].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        // EtherType (arbitrary)
        frame[12..14].copy_from_slice(&[0x08, 0x00]);

        // Send from fd1 (the VM's end of the socketpair)
        // SAFETY: Writing to our own socketpair fd.
        let sent =
            unsafe { libc::send(fd1, frame.as_ptr() as *const libc::c_void, frame.len(), 0) };
        assert_eq!(sent, 14);

        // Wait briefly for the switch forwarding loop
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Read from fd2 (the VM's end of the receiver socketpair)
        let mut buf = vec![0u8; 1518];
        // SAFETY: Reading from our own socketpair fd.
        let recvd = unsafe {
            libc::recv(
                fd2,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(
            recvd, 14,
            "broadcast frame should be forwarded to the other port"
        );
        assert_eq!(&buf[..14], &frame[..14]);

        switch.stop();
    }

    #[test]
    fn switch_no_cross_network_forwarding() {
        let switch = NetworkSwitch::new();
        let fd1 = switch.add_port("net-a", "sender").unwrap();
        let fd2 = switch.add_port("net-b", "isolated").unwrap();
        switch.start().unwrap();

        // Broadcast frame from net-a
        let mut frame = [0u8; 14];
        frame[0..6].copy_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        frame[6..12].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);

        // SAFETY: Writing to our own socketpair fd.
        unsafe {
            libc::send(fd1, frame.as_ptr() as *const libc::c_void, frame.len(), 0);
        }

        std::thread::sleep(std::time::Duration::from_millis(200));

        // net-b should NOT receive the frame
        let mut buf = vec![0u8; 1518];
        // SAFETY: Reading from our own socketpair fd.
        let recvd = unsafe {
            libc::recv(
                fd2,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert!(recvd <= 0, "frame should NOT cross network boundaries");

        switch.stop();
    }
}
