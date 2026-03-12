//! Integration tests for disk CoW cloning.

use std::path::PathBuf;
use vm_rs::VmManager;

#[test]
fn clone_disk_creates_copy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("base.img");
    let target = tmp.path().join("clone.img");

    // Create a base image with known content
    let content = vec![0xABu8; 4096];
    std::fs::write(&base, &content).expect("write base");

    VmManager::clone_disk(&base, &target).expect("clone failed");

    assert!(target.exists(), "clone should exist");
    let cloned = std::fs::read(&target).expect("read clone");
    assert_eq!(cloned, content, "clone content should match base");
}

#[test]
fn clone_disk_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("base.img");
    let target = tmp.path().join("clone.img");

    std::fs::write(&base, b"original").expect("write base");
    VmManager::clone_disk(&base, &target).expect("first clone");

    // Modify base after first clone
    std::fs::write(&base, b"modified").expect("modify base");

    // Second clone should be a no-op (target already exists)
    VmManager::clone_disk(&base, &target).expect("second clone");

    let content = std::fs::read(&target).expect("read clone");
    assert_eq!(content, b"original", "idempotent clone should NOT overwrite");
}

#[test]
fn clone_disk_creates_parent_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path().join("base.img");
    let target = tmp.path().join("deep").join("nested").join("dir").join("clone.img");

    std::fs::write(&base, b"data").expect("write base");

    VmManager::clone_disk(&base, &target).expect("clone with nested dirs");
    assert!(target.exists());
}

#[test]
fn clone_nonexistent_base_fails() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = PathBuf::from("/nonexistent/base.img");
    let target = tmp.path().join("clone.img");

    let result = VmManager::clone_disk(&base, &target);
    assert!(result.is_err(), "cloning from nonexistent base should fail");
}
