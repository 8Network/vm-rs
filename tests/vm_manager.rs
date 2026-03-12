//! VmManager tests with a mock driver.
//!
//! These tests exercise the VmManager orchestration layer (concurrency,
//! state transitions, error handling) WITHOUT a hypervisor. They run on
//! all platforms and in CI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use vm_rs::config::{VmConfig, VmHandle, VmState};
use vm_rs::driver::{VmDriver, VmError};
use vm_rs::VmManager;

// ---------------------------------------------------------------------------
// Mock driver
// ---------------------------------------------------------------------------

/// A mock VmDriver that tracks boot/stop/kill calls and allows
/// controlling state responses.
struct MockDriver {
    /// How many times boot() has been called.
    boot_count: AtomicU32,
    /// VMs currently "running" in the mock.
    vms: Mutex<HashMap<String, MockVmState>>,
    /// If set, boot() will fail with this message.
    fail_boot: Mutex<Option<String>>,
}

#[derive(Clone)]
struct MockVmState {
    state: VmState,
}

impl MockDriver {
    fn new() -> Self {
        Self {
            boot_count: AtomicU32::new(0),
            vms: Mutex::new(HashMap::new()),
            fail_boot: Mutex::new(None),
        }
    }

    fn set_fail_boot(&self, msg: &str) {
        *self.fail_boot.lock().unwrap() = Some(msg.to_string());
    }

    fn set_vm_state(&self, name: &str, state: VmState) {
        if let Some(vm) = self.vms.lock().unwrap().get_mut(name) {
            vm.state = state;
        }
    }
}

impl VmDriver for MockDriver {
    fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
        self.boot_count.fetch_add(1, Ordering::SeqCst);

        if let Some(msg) = self.fail_boot.lock().unwrap().as_ref() {
            return Err(VmError::BootFailed {
                name: config.name.clone(),
                detail: msg.clone(),
            });
        }

        let handle = VmHandle {
            name: config.name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Running {
                ip: "10.0.0.99".into(),
            },
            pid: Some(99999),
            serial_log: config.serial_log.clone(),
        };

        self.vms.lock().unwrap().insert(
            config.name.clone(),
            MockVmState {
                state: VmState::Running {
                    ip: "10.0.0.99".into(),
                },
            },
        );

        Ok(handle)
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self.vms.lock().unwrap();
        match vms.get_mut(&handle.name) {
            Some(vm) => {
                vm.state = VmState::Stopped;
                Ok(())
            }
            None => Err(VmError::NotFound {
                name: handle.name.clone(),
            }),
        }
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self.vms.lock().unwrap();
        match vms.get_mut(&handle.name) {
            Some(vm) => {
                vm.state = VmState::Stopped;
                Ok(())
            }
            None => Err(VmError::NotFound {
                name: handle.name.clone(),
            }),
        }
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let vms = self.vms.lock().unwrap();
        match vms.get(&handle.name) {
            Some(vm) => Ok(vm.state.clone()),
            None => Ok(VmState::Stopped),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_manager(driver: MockDriver) -> VmManager {
    let tmp = tempfile::tempdir().expect("tempdir");
    VmManager::with_driver(Box::new(driver), tmp.into_path()).expect("manager")
}

fn make_config(name: &str) -> VmConfig {
    let tmp = std::env::temp_dir().join(format!("vmrs-mock-{}", name));
    std::fs::create_dir_all(&tmp).ok();
    VmConfig {
        name: name.to_string(),
        namespace: "test".to_string(),
        kernel: std::path::PathBuf::from("/dev/null"),
        initramfs: None,
        root_disk: None,
        data_disk: None,
        seed_iso: None,
        cpus: 1,
        memory_mb: 256,
        networks: vec![],
        shared_dirs: vec![],
        serial_log: tmp.join("serial.log"),
        cmdline: None,
        netns: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn boot_and_state_transitions() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-boot");
    let handle = manager.start(&config).expect("boot should succeed");
    assert_eq!(handle.name, "mock-boot");

    // State should be Running (mock returns Running immediately)
    let state = manager.state("mock-boot").expect("state query");
    assert!(
        matches!(state, VmState::Running { .. }),
        "expected Running, got: {}",
        state
    );
}

#[test]
fn stop_transitions_to_stopped() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-stop");
    manager.start(&config).expect("boot");

    manager.stop("mock-stop").expect("stop should succeed");
    let state = manager.state("mock-stop").expect("state");
    assert_eq!(state, VmState::Stopped);
}

#[test]
fn kill_transitions_to_stopped() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-kill");
    manager.start(&config).expect("boot");

    manager.kill("mock-kill").expect("kill should succeed");
    let state = manager.state("mock-kill").expect("state");
    assert_eq!(state, VmState::Stopped);
}

#[test]
fn duplicate_boot_rejected() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-dup");
    manager.start(&config).expect("first boot");

    let result = manager.start(&config);
    assert!(result.is_err(), "duplicate boot should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("already exists"),
        "error should say 'already exists': {}",
        err
    );
}

#[test]
fn boot_after_stop_succeeds() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-restart");
    manager.start(&config).expect("first boot");
    manager.stop("mock-restart").expect("stop");

    // Should be able to boot again after stop
    let handle = manager.start(&config).expect("second boot should succeed");
    assert_eq!(handle.name, "mock-restart");
}

#[test]
fn boot_failure_cleans_up_placeholder() {
    let driver = MockDriver::new();
    driver.set_fail_boot("simulated hardware fault");
    let manager = make_manager(driver);

    let config = make_config("mock-fail");
    let result = manager.start(&config);
    assert!(result.is_err(), "boot should fail");

    // The placeholder should be cleaned up — state query should return NotFound
    let state = manager.state("mock-fail");
    assert!(state.is_err(), "VM should not exist after failed boot");
}

#[test]
fn stop_nonexistent_vm() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let result = manager.stop("does-not-exist");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not found"));
}

#[test]
fn kill_nonexistent_vm() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let result = manager.kill("ghost");
    assert!(result.is_err());
}

#[test]
fn list_tracks_running_vms() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    assert_eq!(manager.list().expect("list").len(), 0);

    let config_a = make_config("mock-list-a");
    let config_b = make_config("mock-list-b");
    manager.start(&config_a).expect("boot a");
    manager.start(&config_b).expect("boot b");

    let list = manager.list().expect("list");
    assert_eq!(list.len(), 2);

    let names: Vec<&str> = list.iter().map(|h| h.name.as_str()).collect();
    assert!(names.contains(&"mock-list-a"));
    assert!(names.contains(&"mock-list-b"));
}

#[test]
fn get_ip_returns_ip_when_running() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-ip");
    manager.start(&config).expect("boot");

    let ip = manager.get_ip("mock-ip").expect("get_ip");
    assert_eq!(ip, Some("10.0.0.99".to_string()));
}

#[test]
fn get_ip_returns_none_when_stopped() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-ip-stopped");
    manager.start(&config).expect("boot");
    manager.stop("mock-ip-stopped").expect("stop");

    let ip = manager.get_ip("mock-ip-stopped").expect("get_ip");
    assert_eq!(ip, None);
}

#[test]
fn concurrent_boots_different_names() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    // Boot multiple VMs concurrently — different names should all succeed
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let config = make_config(&format!("mock-concurrent-{}", i));
                let mgr = &manager;
                s.spawn(move || mgr.start(&config))
            })
            .collect();

        let mut successes = 0;
        for h in handles {
            if h.join().unwrap().is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 5, "all 5 boots should succeed");
    });

    assert_eq!(manager.list().expect("list").len(), 5);
}

#[test]
fn wait_all_ready_succeeds_when_all_running() {
    let driver = MockDriver::new();
    let manager = make_manager(driver);

    let config = make_config("mock-ready");
    manager.start(&config).expect("boot");

    // Mock driver returns Running immediately, so wait should succeed fast
    manager
        .wait_all_ready(5)
        .expect("wait_all_ready should succeed");
}
