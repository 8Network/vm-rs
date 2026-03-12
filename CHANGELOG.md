# Changelog

All notable changes to vm-rs will be documented in this file.

This project follows [Semantic Versioning](https://semver.org/). During `0.x`, minor versions may contain breaking changes.

## [Unreleased]

## [0.2.0] — 2026-03-12

### Added
- **Windows support**: Crate compiles and unit/integration tests pass on `x86_64-pc-windows-msvc`
- **CI/CD release pipeline**: Tag-triggered (`v*`) workflow — validates version, runs full test suite on all 3 platforms, publishes to crates.io, creates GitHub release with changelog
- **Windows CI**: Build, unit tests, and integration tests (vm_manager, disk_clone) run on `windows-latest`
- Platform-conditional compilation (`#[cfg(unix)]`) for Unix-only APIs (socketpairs, libc)
- `USERPROFILE` fallback for data directory on Windows

### Changed
- `NetworkAttachment::SocketPairFd` is now `#[cfg(unix)]` only
- `NetworkSwitch` module is `#[cfg(unix)]` only
- `libc` dependency moved to `[target.'cfg(unix)'.dependencies]`
- Integration tests (network_switch, seed_iso) skipped on Windows runners

## [0.1.0] — 2026-03-12

Initial release.

### Platforms
- **macOS**: Apple Virtualization.framework via Objective-C FFI
- **Linux**: Cloud Hypervisor (CLI mode, process management)

### Lifecycle
- Boot, stop, kill, pause/resume, state queries
- Boot completion detection via serial console marker (`VMRS_READY`)
- Multi-VM orchestration via `VmManager`

### Boot Modes
- Direct Linux boot (kernel + initramfs)
- Cloud-init boot (kernel + root disk + seed ISO)
- UEFI boot (macOS 13+, via EFI variable store)

### Storage
- VirtioBlock root and data disks
- NVMe storage (macOS, via Apple VZ)
- Cloud-init seed ISO generation (`hdiutil` / `genisoimage`)
- CoW disk cloning (APFS `cp -c` / reflink)

### Networking
- L2 userspace Ethernet switch with MAC learning (macOS)
- TAP device support (Linux)
- `SocketPairFd` and `Tap` network attachment types

### Shared Directories
- VirtioFS (native on macOS, virtiofsd sidecar on Linux)
- Rosetta x86-on-ARM translation support (macOS 13+ / Apple Silicon)
- Single and multiple directory shares

### OCI Images
- Pull from Docker Hub, GHCR, and any OCI-compliant registry
- Content-addressable blob store with SHA-256 verification
- Layer extraction (tar + gzip)

### Setup
- Image catalog: Ubuntu and Alpine resolvers
- Streaming download with progress, atomic rename, SHA-256 verification
- SSH key generation

### Security
- Pin<Box<>> for VM objects (no memory leaks)
- GCD serial queues for all ObjC calls (thread safety)
- Shell argument quoting for subprocess invocations
- Atomic downloads (temp file → fsync → rename)
