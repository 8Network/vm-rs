//! Integration tests for OCI registry pull.
//!
//! These tests pull real images from Docker Hub.
//! They require internet access and may be slow.
//!
//! Run with: cargo test --test oci_pull

use vm_rs::oci::registry::parse_image_ref;
use vm_rs::oci::store::ImageStore;

// ── Image ref parsing edge cases ─────────────────────────────────────────

#[test]
fn parse_docker_official_image() {
    let r = parse_image_ref("alpine:3.20");
    assert_eq!(r.registry, "registry-1.docker.io");
    assert_eq!(r.repository, "library/alpine");
    assert_eq!(r.tag, "3.20");
}

#[test]
fn parse_ghcr_image() {
    let r = parse_image_ref("ghcr.io/someorg/service:latest");
    assert_eq!(r.registry, "ghcr.io");
    assert_eq!(r.repository, "someorg/service");
    assert_eq!(r.tag, "latest");
}

#[test]
fn parse_ecr_image() {
    let r = parse_image_ref("123456789012.dkr.ecr.us-east-1.amazonaws.com/my-app:v1.0");
    assert_eq!(r.registry, "123456789012.dkr.ecr.us-east-1.amazonaws.com");
    assert_eq!(r.repository, "my-app");
    assert_eq!(r.tag, "v1.0");
}

// ── Real registry pull (requires internet) ───────────────────────────────

#[tokio::test]
async fn pull_alpine_image_from_dockerhub() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = ImageStore::new(tmp.path()).expect("store");

    // Pull a tiny image: alpine:3.20
    let result = vm_rs::oci::pull("alpine:3.20", &store).await;

    match result {
        Ok(manifest) => {
            assert!(
                !manifest.layer_digests.is_empty(),
                "alpine should have layers"
            );
            assert!(
                !manifest.config_digest.is_empty(),
                "should have config digest"
            );

            // Verify blobs exist on disk
            for digest in &manifest.layer_digests {
                let blob_path = store.blob_path(digest);
                assert!(
                    blob_path.exists(),
                    "blob {} should exist at {}",
                    digest,
                    blob_path.display()
                );
            }

            // Verify config exists
            let config_path = store.blob_path(&manifest.config_digest);
            assert!(config_path.exists(), "config blob should exist");

            // Parse the config
            let config_bytes = std::fs::read(&config_path).expect("read config");
            let config = ImageStore::parse_config(&config_bytes).expect("parse config");
            // Alpine has /bin/sh as default cmd
            assert!(
                !config.cmd.is_empty() || !config.entrypoint.is_empty(),
                "alpine should have cmd or entrypoint"
            );
        }
        Err(e) => {
            // Network errors are acceptable in CI — skip rather than fail
            if e.to_string().contains("HTTP") || e.to_string().contains("timeout") {
                eprintln!("skipping: network error pulling alpine: {}", e);
                return;
            }
            panic!("unexpected error pulling alpine: {}", e);
        }
    }
}

#[tokio::test]
async fn pull_busybox_and_verify_layers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = ImageStore::new(tmp.path()).expect("store");

    let result = vm_rs::oci::pull("busybox:latest", &store).await;

    match result {
        Ok(manifest) => {
            // busybox is typically 1 layer
            assert!(
                !manifest.layer_digests.is_empty(),
                "busybox should have at least 1 layer, got {}",
                manifest.layer_digests.len()
            );

            // Verify digest format
            for digest in &manifest.layer_digests {
                assert!(
                    digest.starts_with("sha256:"),
                    "digest should start with sha256: got {}",
                    digest
                );
            }
        }
        Err(e) => {
            if e.to_string().contains("HTTP") || e.to_string().contains("timeout") {
                eprintln!("skipping: network error: {}", e);
                return;
            }
            panic!("unexpected error: {}", e);
        }
    }
}

// ── Idempotent pull ──────────────────────────────────────────────────────

#[tokio::test]
async fn pull_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = ImageStore::new(tmp.path()).expect("store");

    // Pull twice — second should be fast (blobs already cached)
    let result1 = vm_rs::oci::pull("busybox:latest", &store).await;
    match result1 {
        Ok(_) => {
            let start = std::time::Instant::now();
            let result2 = vm_rs::oci::pull("busybox:latest", &store).await;
            let elapsed = start.elapsed();
            assert!(result2.is_ok(), "second pull should succeed");
            eprintln!("second pull took {:?}", elapsed);
        }
        Err(e) => {
            if e.to_string().contains("HTTP") || e.to_string().contains("timeout") {
                eprintln!("skipping: network error: {}", e);
                return;
            }
            panic!("unexpected error: {}", e);
        }
    }
}

// ── Invalid image ────────────────────────────────────────────────────────

#[tokio::test]
async fn pull_nonexistent_image_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = ImageStore::new(tmp.path()).expect("store");

    let result = vm_rs::oci::pull("library/this-image-does-not-exist-12345:v999", &store).await;

    match result {
        Err(e) => {
            let msg = e.to_string();
            // Should be an auth or HTTP error, not a panic
            assert!(
                msg.contains("HTTP")
                    || msg.contains("auth")
                    || msg.contains("404")
                    || msg.contains("UNAUTHORIZED")
                    || msg.contains("NAME_UNKNOWN"),
                "expected HTTP/auth error, got: {}",
                msg
            );
        }
        Ok(_) => panic!("pulling a nonexistent image should fail"),
    }
}
