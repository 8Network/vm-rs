# Changelog

All notable changes to vm-rs will be documented in this file.

This project follows [Semantic Versioning](https://semver.org/). During `0.x`, minor versions may contain breaking changes.

## [Unreleased]

## [0.2.3] — 2026-03-14

### Windows
- Exclude readiness-marker helpers from Windows builds where they are unused
- Resolve remaining WHP clippy issues on the Windows release runner

## [0.2.2] — 2026-03-14

### Windows
- Fix WHP capability probing on the Windows release runner by avoiding a `BOOL` constructor that is not exposed by the selected `windows` bindings

## [0.2.1] — 2026-03-14

### CI and release
- Fix the Windows release build by declaring the WHP `windows` crate dependency explicitly
- Avoid Windows-only clippy failures from readiness helper re-exports and seed ISO helper arguments

## [0.2.0] — 2026-03-14

### Lifecycle and state
- Split VM lifecycle reporting into `Starting`, `Running`, and `Ready`
- Make readiness detection explicit instead of conflating hypervisor execution with guest readiness
- Add incremental serial-log readiness caching for Apple VZ and Cloud Hypervisor backends

### Reliability
- Harden Cloud Hypervisor cleanup and stop/kill failure handling
- Reap `virtiofsd` sidecars consistently and clean them up on boot failures
- Improve logging around readiness detection and lifecycle transitions

### CI and platform support
- Add Apple VZ FFI smoke coverage to normal macOS CI
- Keep the crate building and testing on `x86_64-pc-windows-msvc`
- Clarify the project's experimental status and current verification surface in the docs

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
