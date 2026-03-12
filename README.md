# vm-rs

Cross-platform VM lifecycle management for Rust.
Boot, stop, and orchestrate lightweight virtual machines on macOS, Linux, and Windows from a single API.

- **macOS**: Apple Virtualization.framework (in-process, via Objective-C FFI)
- **Linux**: Cloud Hypervisor (separate VMM process, CLI mode)
- **Windows**: WHP driver planned (compiles and tests today, VM driver coming)

## Quick Start

```rust
use vm_rs::{VmConfig, VmManager};
use std::path::PathBuf;

let manager = VmManager::new(PathBuf::from("/tmp/vms"))?;

let config = VmConfig {
    name: "my-vm".into(),
    namespace: "dev".into(),
    kernel: PathBuf::from("/path/to/vmlinuz"),
    initramfs: Some(PathBuf::from("/path/to/initramfs")),
    root_disk: None,
    data_disk: None,
    seed_iso: None,
    cpus: 2,
    memory_mb: 512,
    networks: vec![],
    shared_dirs: vec![],
    serial_log: PathBuf::from("/tmp/vms/my-vm/serial.log"),
    cmdline: None,
    netns: None,
    vsock: false,
    machine_id: None,
    efi_variable_store: None,
    rosetta: false,
};

let handle = manager.start(&config)?;
// VM is booting — poll state until Running
let state = manager.state("my-vm")?;
```

## Features

**Lifecycle**: boot, stop, kill, pause/resume, state queries, readiness detection

**Boot modes**: Direct Linux boot (kernel + initramfs), UEFI boot (macOS 13+)

**Storage**: VirtioBlock root/data disks, cloud-init seed ISOs, CoW disk cloning (APFS/reflink)

**Networking**: NAT (macOS), TAP devices (Linux), userspace L2 switch with MAC learning

**Shared directories**: VirtioFS (native on macOS, virtiofsd sidecar on Linux), Rosetta x86-on-ARM

**OCI images**: Pull from Docker Hub / GHCR / any OCI registry, content-addressable blob store, layer extraction

**Setup**: Cloud-init seed ISO generation, SSH key generation, image download and caching

See [docs/CAPABILITIES.md](docs/CAPABILITIES.md) for the full capability matrix with per-platform status.

## Requirements

### macOS

- macOS 13.0+ (Ventura)
- Binary must be signed with `com.apple.security.virtualization` entitlement
- Xcode Command Line Tools (for the Virtualization.framework headers)

### Linux

- [Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor/releases) on `$PATH`
- KVM access (`/dev/kvm`)
- Optional: `virtiofsd` for VirtioFS shared directories
- Optional: `genisoimage` or `mkisofs` for cloud-init seed ISOs

### Windows

- Windows 10/11 with Hyper-V enabled (WHP driver planned)
- OCI, setup, and core types work today; VM lifecycle driver coming

## Building

```bash
# All platforms
cargo build

# Run tests
cargo test

# Run only unit tests (no hypervisor needed)
cargo test --lib

# Run FFI smoke tests (macOS only, needs VZ entitlement)
cargo test --test ffi_smoke
```

## Architecture

```
VmManager              Multi-VM orchestration, auto-selects driver
  VmDriver trait       Platform-agnostic lifecycle interface
    AppleVzDriver      macOS: VZ framework via ObjC FFI + GCD queues
    CloudHvDriver      Linux: cloud-hypervisor process + signals
  NetworkSwitch        L2 userspace Ethernet switch (macOS)
  OciStore + pull()    Content-addressable OCI image store
  setup::              Cloud-init ISOs, image download, SSH keys
```

The Apple VZ driver dispatches all Objective-C calls to per-VM GCD serial queues.
The calling thread never makes ObjC calls directly — this prevents autorelease pool
corruption that causes SIGSEGV at thread exit.

The Cloud Hypervisor driver spawns `cloud-hypervisor` as a child process and manages
lifecycle via signals. The `Child` handle prevents PID reuse.

## Testing

156 tests across 8 test suites. CI runs on macOS (aarch64), Linux (x86_64), and Windows (x86_64).

```bash
cargo test                        # everything
cargo test --lib                  # unit tests only (fast, no hypervisor)
cargo test --test vm_manager      # VmManager with mock driver
cargo test --test network_switch  # L2 switch integration
cargo test --test oci_pull        # OCI registry (needs internet)
cargo test --test ffi_smoke       # Apple VZ FFI (macOS only)
```

## License

[MIT](LICENSE)

## Contributing

See [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md).
