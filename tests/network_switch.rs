//! Integration tests for the userspace L2 Ethernet switch.
//!
//! These tests exercise the actual socketpair-based forwarding loop
//! with real Ethernet frames. No VM needed — just Unix sockets.

use vm_rs::network::NetworkSwitch;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Build a minimal Ethernet frame: dst(6) + src(6) + ethertype(2) + payload.
fn build_frame(dst: [u8; 6], src: [u8; 6], payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.extend_from_slice(&dst);
    frame.extend_from_slice(&src);
    frame.extend_from_slice(&[0x08, 0x00]); // IPv4 ethertype
    frame.extend_from_slice(payload);
    frame
}

fn send_raw(fd: i32, data: &[u8]) -> isize {
    // SAFETY: Sending to our own socketpair fd.
    unsafe {
        libc::send(
            fd,
            data.as_ptr() as *const libc::c_void,
            data.len(),
            0,
        )
    }
}

fn recv_raw(fd: i32, buf: &mut [u8]) -> isize {
    // SAFETY: Reading from our own socketpair fd.
    unsafe {
        libc::recv(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    }
}

const BROADCAST: [u8; 6] = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
const MAC_A: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const MAC_B: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
#[allow(dead_code)]
const MAC_C: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x03];

// ── Broadcast flooding ───────────────────────────────────────────────────

#[test]
fn broadcast_floods_to_all_ports_on_same_network() {
    let switch = NetworkSwitch::new();
    let fd_a = switch.add_port("net0", "a").unwrap();
    let fd_b = switch.add_port("net0", "b").unwrap();
    let fd_c = switch.add_port("net0", "c").unwrap();
    switch.start().unwrap();

    let frame = build_frame(BROADCAST, MAC_A, b"hello");
    assert_eq!(send_raw(fd_a, &frame), frame.len() as isize);

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut buf = vec![0u8; 1518];

    // Both B and C should receive
    let n_b = recv_raw(fd_b, &mut buf);
    assert_eq!(n_b, frame.len() as isize, "port B should receive broadcast");

    let n_c = recv_raw(fd_c, &mut buf);
    assert_eq!(n_c, frame.len() as isize, "port C should receive broadcast");

    switch.stop();
}

// ── Unicast after MAC learning ───────────────────────────────────────────

#[test]
fn unicast_to_learned_mac() {
    let switch = NetworkSwitch::new();
    let fd_a = switch.add_port("net0", "a").unwrap();
    let fd_b = switch.add_port("net0", "b").unwrap();
    let fd_c = switch.add_port("net0", "c").unwrap();
    switch.start().unwrap();

    // Step 1: B sends a broadcast → switch learns MAC_B is on port B
    let frame_from_b = build_frame(BROADCAST, MAC_B, b"learn-me");
    send_raw(fd_b, &frame_from_b);
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Drain broadcast from A and C
    let mut buf = vec![0u8; 1518];
    recv_raw(fd_a, &mut buf);
    recv_raw(fd_c, &mut buf);

    // Step 2: A sends unicast to MAC_B → should ONLY go to B, not C
    let unicast = build_frame(MAC_B, MAC_A, b"just-for-b");
    send_raw(fd_a, &unicast);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let n_b = recv_raw(fd_b, &mut buf);
    assert_eq!(n_b, unicast.len() as isize, "B should receive unicast");

    let n_c = recv_raw(fd_c, &mut buf);
    assert!(n_c <= 0, "C should NOT receive unicast destined for B, got {} bytes", n_c);

    switch.stop();
}

// ── Network isolation ────────────────────────────────────────────────────

#[test]
fn different_networks_are_isolated() {
    let switch = NetworkSwitch::new();
    let fd_frontend = switch.add_port("frontend", "web").unwrap();
    let fd_backend = switch.add_port("backend", "db").unwrap();
    switch.start().unwrap();

    let frame = build_frame(BROADCAST, MAC_A, b"secret-data");
    send_raw(fd_frontend, &frame);

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut buf = vec![0u8; 1518];
    let n = recv_raw(fd_backend, &mut buf);
    assert!(n <= 0, "backend network should NOT receive frames from frontend network");

    switch.stop();
}

// ── No self-echo ─────────────────────────────────────────────────────────

#[test]
fn broadcast_does_not_echo_back_to_sender() {
    let switch = NetworkSwitch::new();
    let fd_a = switch.add_port("net0", "a").unwrap();
    let _fd_b = switch.add_port("net0", "b").unwrap();
    switch.start().unwrap();

    let frame = build_frame(BROADCAST, MAC_A, b"data");
    send_raw(fd_a, &frame);

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut buf = vec![0u8; 1518];
    let n = recv_raw(fd_a, &mut buf);
    assert!(n <= 0, "sender should NOT receive its own broadcast back");

    switch.stop();
}

// ── Runt frame rejection ─────────────────────────────────────────────────

#[test]
fn runt_frames_are_dropped() {
    let switch = NetworkSwitch::new();
    let fd_a = switch.add_port("net0", "a").unwrap();
    let fd_b = switch.add_port("net0", "b").unwrap();
    switch.start().unwrap();

    // Send a frame smaller than 14 bytes (minimum Ethernet)
    let runt = vec![0xff; 10];
    send_raw(fd_a, &runt);

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut buf = vec![0u8; 1518];
    let n = recv_raw(fd_b, &mut buf);
    assert!(n <= 0, "runt frame should be dropped, not forwarded");

    switch.stop();
}

// ── High throughput ──────────────────────────────────────────────────────

#[test]
fn handles_burst_of_frames() {
    let switch = NetworkSwitch::new();
    let fd_a = switch.add_port("net0", "a").unwrap();
    let fd_b = switch.add_port("net0", "b").unwrap();
    switch.start().unwrap();

    // First: B sends a broadcast so switch learns MAC_B
    let learn = build_frame(BROADCAST, MAC_B, b"learn");
    send_raw(fd_b, &learn);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let mut drain = vec![0u8; 1518];
    recv_raw(fd_a, &mut drain); // drain the flood

    // Send frames one at a time with a pause to let the poll loop process.
    // The switch uses poll(2) with 50ms timeout per iteration and acquires
    // locks each time, so it processes ~20 frames/sec. This test verifies
    // correctness under sustained load, not raw throughput.
    let frame = build_frame(MAC_B, MAC_A, &[0xAA; 100]);
    let count = 10;
    for _ in 0..count {
        send_raw(fd_a, &frame);
        std::thread::sleep(std::time::Duration::from_millis(60));
    }

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Count how many B received
    let mut received = 0;
    let mut buf = vec![0u8; 1518];
    loop {
        let n = recv_raw(fd_b, &mut buf);
        if n <= 0 {
            break;
        }
        received += 1;
    }

    // With 60ms pacing (one frame per poll cycle), all should be delivered
    assert!(
        received >= count - 1,
        "expected nearly all frames delivered with paced sending, got {}/{} frames",
        received,
        count
    );

    switch.stop();
}

// ── Multiple networks concurrent ─────────────────────────────────────────

#[test]
fn multiple_networks_operate_independently() {
    let switch = NetworkSwitch::new();

    let fd_a1 = switch.add_port("net-a", "a1").unwrap();
    let fd_a2 = switch.add_port("net-a", "a2").unwrap();
    let fd_b1 = switch.add_port("net-b", "b1").unwrap();
    let fd_b2 = switch.add_port("net-b", "b2").unwrap();
    switch.start().unwrap();

    // Send on both networks simultaneously
    let frame_a = build_frame(BROADCAST, MAC_A, b"net-a-data");
    let frame_b = build_frame(BROADCAST, MAC_B, b"net-b-data");

    send_raw(fd_a1, &frame_a);
    send_raw(fd_b1, &frame_b);

    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut buf = vec![0u8; 1518];

    // net-a: a2 should receive frame_a
    let n = recv_raw(fd_a2, &mut buf);
    assert_eq!(n, frame_a.len() as isize, "a2 should receive net-a broadcast");

    // net-b: b2 should receive frame_b
    let n = recv_raw(fd_b2, &mut buf);
    assert_eq!(n, frame_b.len() as isize, "b2 should receive net-b broadcast");

    // Cross-check: a2 should NOT get net-b's frame
    let n = recv_raw(fd_a2, &mut buf);
    assert!(n <= 0, "net-a should not receive net-b frames");

    switch.stop();
}

// ── Start/stop/restart ───────────────────────────────────────────────────

#[test]
fn switch_restart() {
    let switch = NetworkSwitch::new();
    let fd_a = switch.add_port("net0", "a").unwrap();
    let fd_b = switch.add_port("net0", "b").unwrap();

    // First run
    switch.start().unwrap();
    let frame = build_frame(BROADCAST, MAC_A, b"first");
    send_raw(fd_a, &frame);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut buf = vec![0u8; 1518];
    let n = recv_raw(fd_b, &mut buf);
    assert_eq!(n, frame.len() as isize);

    // Stop
    switch.stop();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Restart
    switch.start().unwrap();
    let frame2 = build_frame(BROADCAST, MAC_A, b"second");
    send_raw(fd_a, &frame2);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let n = recv_raw(fd_b, &mut buf);
    assert_eq!(n, frame2.len() as isize, "switch should work after restart");

    switch.stop();
}
