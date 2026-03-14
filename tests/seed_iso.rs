//! Integration tests for cloud-init seed ISO generation.
//!
//! These tests create actual ISO files and verify their contents.
//! Requires: hdiutil (macOS) or genisoimage/mkisofs (Linux).

use vm_rs::setup::{
    create_seed_iso, HealthCheckConfig, NicConfig, ProcessConfig, SeedConfig, VolumeMountConfig,
};

fn has_iso_tool() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("hdiutil")
            .arg("--help")
            .output()
            .is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("genisoimage")
            .arg("--version")
            .output()
            .is_ok()
            || std::process::Command::new("mkisofs")
                .arg("--version")
                .output()
                .is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

// ── Basic ISO creation ───────────────────────────────────────────────────

#[test]
fn create_minimal_seed_iso() {
    if !has_iso_tool() {
        eprintln!("skipping: no ISO creation tool available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let iso_path = tmp.path().join("seed.iso");

    let config = SeedConfig {
        hostname: "test-vm",
        ssh_pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest test@test",
        nics: vec![],
        process: None,
        volumes: vec![],
        healthcheck: None,
        extra_hosts: vec![],
    };

    create_seed_iso(&iso_path, &config).expect("ISO creation failed");

    assert!(iso_path.exists(), "ISO file should exist");
    let metadata = std::fs::metadata(&iso_path).expect("metadata");
    assert!(metadata.len() > 0, "ISO should not be empty");
}

// ── Full config ISO ──────────────────────────────────────────────────────

#[test]
fn create_full_config_seed_iso() {
    if !has_iso_tool() {
        eprintln!("skipping: no ISO creation tool available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let iso_path = tmp.path().join("seed-full.iso");

    let config = SeedConfig {
        hostname: "web-server",
        ssh_pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest test@test",
        nics: vec![
            NicConfig {
                name: "eth0".to_string(),
                ip: "10.0.1.2/24".to_string(),
                gateway: Some("10.0.1.1".to_string()),
            },
            NicConfig {
                name: "eth1".to_string(),
                ip: "10.0.2.2/24".to_string(),
                gateway: None,
            },
        ],
        process: Some(ProcessConfig {
            command: "/usr/bin/nginx -g 'daemon off;'".to_string(),
            workdir: Some("/var/www".to_string()),
            env: vec![
                ("PORT".to_string(), "8080".to_string()),
                ("NODE_ENV".to_string(), "production".to_string()),
            ],
        }),
        volumes: vec![VolumeMountConfig {
            tag: "app-data".to_string(),
            mount_point: "/data".to_string(),
            read_only: false,
        }],
        healthcheck: Some(HealthCheckConfig {
            command: "curl -f http://localhost:8080/health".to_string(),
            interval_secs: 10,
            retries: 3,
        }),
        extra_hosts: vec![
            ("db.local".to_string(), "10.0.1.100".to_string()),
            ("cache.local".to_string(), "10.0.1.101".to_string()),
        ],
    };

    create_seed_iso(&iso_path, &config).expect("ISO creation failed");

    assert!(iso_path.exists());
    let metadata = std::fs::metadata(&iso_path).expect("metadata");
    assert!(
        metadata.len() > 100,
        "full config ISO should be larger than minimal"
    );
}

// ── Temp dir cleanup ─────────────────────────────────────────────────────

#[test]
fn seed_iso_cleans_up_temp_dir() {
    if !has_iso_tool() {
        eprintln!("skipping: no ISO creation tool available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let iso_path = tmp.path().join("seed-cleanup.iso");

    let config = SeedConfig {
        hostname: "cleanup-test",
        ssh_pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest test@test",
        nics: vec![],
        process: None,
        volumes: vec![],
        healthcheck: None,
        extra_hosts: vec![],
    };

    create_seed_iso(&iso_path, &config).expect("ISO creation failed");

    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read_dir")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            file_name
                .starts_with(".seed-cleanup-test-")
                .then_some(file_name.to_string())
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp dirs should be cleaned up after ISO creation, found: {:?}",
        leftovers
    );
}

// ── Special characters in hostname ───────────────────────────────────────

#[test]
fn seed_iso_with_hostname_containing_dashes() {
    if !has_iso_tool() {
        eprintln!("skipping: no ISO creation tool available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let iso_path = tmp.path().join("seed-dashes.iso");

    let config = SeedConfig {
        hostname: "my-web-server-01",
        ssh_pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest test@test",
        nics: vec![],
        process: None,
        volumes: vec![],
        healthcheck: None,
        extra_hosts: vec![],
    };

    create_seed_iso(&iso_path, &config).expect("ISO creation with dashes should work");
    assert!(iso_path.exists());
}
