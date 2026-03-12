//! Integration tests for the VM lifecycle.
//!
//! These tests boot real VMs using the platform driver (Apple VZ on macOS,
//! Cloud Hypervisor on Linux). They require:
//! - macOS: Apple Virtualization.framework (macOS 13+, Apple Silicon or x86_64)
//! - Linux: cloud-hypervisor binary on PATH, KVM enabled (/dev/kvm)
//!
//! Asset paths come from the VMRS_TEST_KERNEL, VMRS_TEST_INITRAMFS env vars.
//! If not set, the tests are skipped.
//!
//! Run with: VMRS_TEST_KERNEL=/path/to/vmlinuz VMRS_TEST_INITRAMFS=/path/to/initramfs cargo test --test vm_lifecycle

use std::path::PathBuf;
use vm_rs::config::{SharedDir, VmConfig, VmState};
use vm_rs::VmManager;

/// Load test assets from environment. Returns None if not configured.
fn test_assets() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var("VMRS_TEST_KERNEL").ok().map(PathBuf::from)?;
    let initramfs = std::env::var("VMRS_TEST_INITRAMFS").ok().map(PathBuf::from)?;

    if !kernel.exists() {
        eprintln!("VMRS_TEST_KERNEL={} does not exist, skipping", kernel.display());
        return None;
    }
    if !initramfs.exists() {
        eprintln!(
            "VMRS_TEST_INITRAMFS={} does not exist, skipping",
            initramfs.display()
        );
        return None;
    }
    Some((kernel, initramfs))
}

fn test_base_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vmrs-integration-tests");
    std::fs::create_dir_all(&dir).expect("failed to create test base dir");
    dir
}

fn make_config(name: &str, kernel: &PathBuf, initramfs: &PathBuf) -> VmConfig {
    let base = test_base_dir().join(name);
    std::fs::create_dir_all(&base).expect("failed to create VM dir");

    VmConfig {
        name: name.to_string(),
        namespace: "test".to_string(),
        kernel: kernel.clone(),
        initramfs: Some(initramfs.clone()),
        root_disk: None,
        data_disk: None,
        seed_iso: None,
        cpus: 1,
        memory_mb: 256,
        networks: vec![],
        shared_dirs: vec![],
        serial_log: base.join("serial.log"),
        cmdline: Some("console=hvc0".to_string()),
        netns: None,
    }
}

// ─── Boot + Ready + Stop ─────────────────────────────────────────────────

#[test]
fn boot_vm_reaches_running_state() {
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => {
            eprintln!("skipping: test assets not configured");
            return;
        }
    };

    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let config = make_config("test-boot", &kernel, &initramfs);

    let handle = manager.start(&config).expect("boot failed");
    assert_eq!(handle.name, "test-boot");

    // Poll for ready (up to 30s)
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);
    let mut final_state = VmState::Starting;
    while start.elapsed() < timeout {
        match manager.state("test-boot") {
            Ok(state @ VmState::Running { .. }) => {
                final_state = state;
                break;
            }
            Ok(VmState::Failed { reason }) => {
                panic!("VM boot failed: {}", reason);
            }
            Ok(state) => {
                final_state = state;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => panic!("state query failed: {}", e),
        }
    }

    assert!(
        matches!(final_state, VmState::Running { .. }),
        "VM did not reach Running state within 30s, stuck at: {}",
        final_state
    );

    // Stop gracefully
    manager.stop("test-boot").expect("stop failed");
    let stopped = manager.state("test-boot");
    // After stop, VM should be Stopped (or NotFound if already cleaned up)
    match stopped {
        Ok(VmState::Stopped) => {}
        Err(_) => {} // NotFound is acceptable after stop
        other => panic!("unexpected state after stop: {:?}", other),
    }
}

// ─── Duplicate boot rejection ────────────────────────────────────────────

#[test]
fn reject_duplicate_vm_name() {
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => return,
    };

    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let config = make_config("test-dup", &kernel, &initramfs);

    manager.start(&config).expect("first boot failed");

    // Second boot with same name should fail
    let result = manager.start(&config);
    assert!(result.is_err(), "duplicate boot should be rejected");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("already exists"),
        "error should mention duplicate: {}",
        err
    );

    manager.kill("test-dup").ok();
}

// ─── Force kill ──────────────────────────────────────────────────────────

#[test]
fn force_kill_running_vm() {
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => return,
    };

    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let config = make_config("test-kill", &kernel, &initramfs);

    manager.start(&config).expect("boot failed");

    // Don't wait for ready — kill immediately
    std::thread::sleep(std::time::Duration::from_secs(2));
    manager.kill("test-kill").expect("kill failed");
}

// ─── VM with shared directory ────────────────────────────────────────────

#[test]
fn boot_with_shared_directory() {
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => return,
    };

    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let shared_path = tmp.path().join("shared");
    std::fs::create_dir_all(&shared_path).expect("failed to create shared dir");
    std::fs::write(shared_path.join("test.txt"), "hello from host").expect("write failed");

    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let mut config = make_config("test-shared", &kernel, &initramfs);
    config.shared_dirs.push(SharedDir {
        host_path: shared_path.clone(),
        tag: "testshare".to_string(),
        read_only: true,
    });

    let handle = manager.start(&config).expect("boot with shared dir failed");
    assert_eq!(handle.name, "test-shared");

    // Wait for ready or timeout
    let result = manager.wait_all_ready(30);
    // Clean up regardless
    manager.kill("test-shared").ok();

    result.expect("VM with shared dir did not become ready");
}

// ─── VM list ─────────────────────────────────────────────────────────────

#[test]
fn list_tracks_running_vms() {
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => return,
    };

    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");

    let initial = manager.list().expect("list failed");
    let initial_count = initial.len();

    let config = make_config("test-list", &kernel, &initramfs);
    manager.start(&config).expect("boot failed");

    let after_boot = manager.list().expect("list failed");
    assert_eq!(
        after_boot.len(),
        initial_count + 1,
        "list should include the new VM"
    );
    assert!(
        after_boot.iter().any(|h| h.name == "test-list"),
        "list should contain our VM"
    );

    manager.kill("test-list").ok();
}

// ─── Non-existent VM operations ──────────────────────────────────────────

#[test]
fn stop_nonexistent_vm_fails() {
    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let result = manager.stop("this-vm-does-not-exist");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("not found"), "error: {}", err);
}

#[test]
fn kill_nonexistent_vm_fails() {
    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let result = manager.kill("ghost-vm");
    assert!(result.is_err());
}

#[test]
fn state_nonexistent_vm_fails() {
    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let result = manager.state("no-such-vm");
    assert!(result.is_err());
}

#[test]
fn get_ip_nonexistent_vm_fails() {
    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let result = manager.get_ip("no-such-vm");
    assert!(result.is_err());
}
