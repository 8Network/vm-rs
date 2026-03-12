//! Integration tests for the VM lifecycle.
//!
//! These tests boot real VMs using the platform driver (Apple VZ on macOS,
//! Cloud Hypervisor on Linux). They require:
//! - macOS: Apple Virtualization.framework (macOS 13+, Apple Silicon or x86_64)
//! - Linux: cloud-hypervisor binary on PATH, KVM enabled (/dev/kvm)
//!
//! Asset paths come from the VMRS_TEST_KERNEL, VMRS_TEST_INITRAMFS env vars.
//! If not set, the VM-booting tests are skipped.
//!
//! Run with: VMRS_TEST_KERNEL=/path/to/vmlinuz VMRS_TEST_INITRAMFS=/path/to/initramfs cargo test --test vm_lifecycle

use std::path::PathBuf;
use vm_rs::config::{SharedDir, VmConfig, VmState};
use vm_rs::VmManager;

/// Load test assets from environment. Returns None if not configured.
fn test_assets() -> Option<(PathBuf, PathBuf)> {
    let kernel = std::env::var("VMRS_TEST_KERNEL").ok().map(PathBuf::from)?;
    let initramfs = std::env::var("VMRS_TEST_INITRAMFS")
        .ok()
        .map(PathBuf::from)?;

    if !kernel.exists() {
        eprintln!(
            "VMRS_TEST_KERNEL={} does not exist, skipping",
            kernel.display()
        );
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

/// Check if virtiofsd is available (required for VirtioFS shared dir tests).
fn has_virtiofsd() -> bool {
    std::process::Command::new("virtiofsd")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn test_base_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vmrs-integration-tests");
    std::fs::create_dir_all(&dir).expect("failed to create test base dir");
    dir
}

fn make_config(name: &str, kernel: &std::path::Path, initramfs: &std::path::Path) -> VmConfig {
    let base = test_base_dir().join(name);
    std::fs::create_dir_all(&base).expect("failed to create VM dir");

    // Use ttyS0 for Cloud Hypervisor on Linux, hvc0 for Apple VZ on macOS
    let console = if cfg!(target_os = "linux") {
        "console=ttyS0"
    } else {
        "console=hvc0"
    };

    VmConfig {
        name: name.to_string(),
        namespace: "test".to_string(),
        kernel: kernel.to_path_buf(),
        initramfs: Some(initramfs.to_path_buf()),
        root_disk: None,
        data_disk: None,
        seed_iso: None,
        cpus: 1,
        memory_mb: 256,
        networks: vec![],
        shared_dirs: vec![],
        serial_log: base.join("serial.log"),
        cmdline: Some(console.to_string()),
        netns: None,
        vsock: false,
        machine_id: None,
        efi_variable_store: None,
        rosetta: false,
    }
}

// ─── Boot + Verify Alive + Stop ─────────────────────────────────────────

#[test]
fn boot_vm_stays_alive() {
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

    // Wait a few seconds, then verify the VM process is still alive
    // (not crashed immediately after boot). The VM state should be
    // Starting or Running — either is acceptable depending on whether
    // the initramfs writes VMRS_READY.
    std::thread::sleep(std::time::Duration::from_secs(5));

    let state = manager.state("test-boot").expect("state query failed");
    assert!(
        matches!(state, VmState::Starting | VmState::Running { .. }),
        "VM should be alive (Starting or Running), got: {}",
        state
    );

    // Verify serial log was created and has output
    let serial_log = test_base_dir().join("test-boot").join("serial.log");
    if serial_log.exists() {
        let content = std::fs::read_to_string(&serial_log).unwrap_or_default();
        assert!(
            !content.is_empty(),
            "serial log should have boot output from the kernel"
        );
    }

    // Clean up: kill the VM. On macOS this uses stopWithCompletionHandler
    // to ensure the VM is fully stopped before we return.
    manager.kill("test-boot").ok();
}

// ─── Boot with custom initramfs reaches Running (requires VMRS_READY) ──
// This test only passes with our custom initramfs, not vanilla Alpine.
// Set VMRS_TEST_CUSTOM_INITRAMFS=1 to run it.

#[test]
fn boot_vm_reaches_running_state() {
    if std::env::var("VMRS_TEST_CUSTOM_INITRAMFS").is_err() {
        eprintln!("skipping: VMRS_TEST_CUSTOM_INITRAMFS not set (needs custom init)");
        return;
    }
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => return,
    };

    let manager = VmManager::new(test_base_dir()).expect("failed to create VmManager");
    let config = make_config("test-ready", &kernel, &initramfs);
    manager.start(&config).expect("boot failed");

    // Poll for Running (requires VMRS_READY marker from custom init)
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(30);
    let mut final_state = VmState::Starting;
    while start.elapsed() < timeout {
        match manager.state("test-ready") {
            Ok(state @ VmState::Running { .. }) => {
                final_state = state;
                break;
            }
            Ok(VmState::Failed { reason }) => panic!("VM boot failed: {}", reason),
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

    manager.stop("test-ready").expect("stop failed");
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

    // Verify killed
    std::thread::sleep(std::time::Duration::from_secs(1));
    let state = manager.state("test-kill");
    match state {
        Ok(VmState::Stopped) => {}
        Err(_) => {} // NotFound acceptable
        other => panic!("expected Stopped after kill, got: {:?}", other),
    }
}

// ─── VM with shared directory ────────────────────────────────────────────

#[test]
fn boot_with_shared_directory() {
    let (kernel, initramfs) = match test_assets() {
        Some(a) => a,
        None => return,
    };

    // virtiofsd is required for VirtioFS on Linux
    if cfg!(target_os = "linux") && !has_virtiofsd() {
        eprintln!("skipping: virtiofsd not available");
        return;
    }

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

    // Verify VM started and stays alive
    std::thread::sleep(std::time::Duration::from_secs(3));
    let state = manager.state("test-shared").expect("state query");
    assert!(
        matches!(state, VmState::Starting | VmState::Running { .. }),
        "VM with shared dir should be alive, got: {}",
        state
    );

    manager.kill("test-shared").ok();
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
