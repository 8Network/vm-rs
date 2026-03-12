# Contributing to vm-rs

## Getting Started

1. Fork and clone the repo
2. Install Rust (stable): https://rustup.rs
3. Run `cargo test --lib` to verify your setup

### macOS-specific

- Xcode Command Line Tools required
- FFI smoke tests need the `com.apple.security.virtualization` entitlement
- Full VM lifecycle tests need entitled hardware (not available on CI runners)

### Linux-specific

- Install Cloud Hypervisor: https://github.com/cloud-hypervisor/cloud-hypervisor/releases
- KVM access required for VM tests (`sudo chmod 666 /dev/kvm`)
- `genisoimage` for seed ISO tests: `sudo apt install genisoimage`

## Development Workflow

```bash
# Build
cargo build

# Run all tests
cargo test

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt
```

## Code Style

- **No `unwrap()` in library code.** Propagate errors with `?` or handle explicitly.
- **Typed structs over `serde_json::Value`.** All JSON has a corresponding Rust type.
- **`unsafe` blocks must have `// SAFETY:` comments** explaining the invariants.
- **Log levels match severity**: `error!` for data loss/crashes, `warn!` for degraded, `info!` for operations, `debug!` for diagnostics.

## Project Layout

```
src/
  lib.rs              Public API re-exports
  config.rs           VmConfig, VmHandle, VmState
  vm.rs               VmManager (multi-VM orchestration)
  driver/
    mod.rs            VmDriver trait + VmError
    apple_vz.rs       macOS driver (Apple Virtualization.framework)
    cloud_hv.rs       Linux driver (Cloud Hypervisor CLI)
  ffi/apple_vz/       Objective-C FFI bindings for VZ framework
  network/
    switch.rs         Userspace L2 Ethernet switch (macOS)
    bridge.rs         Linux bridge/TAP networking
    port_forward.rs   TCP port forwarding
  oci/
    registry.rs       OCI distribution client (pull images)
    store.rs          Content-addressable blob store
  setup/
    image.rs          Image download and caching
    seed.rs           Cloud-init seed ISO generation
    ssh.rs            SSH key generation
tests/                Integration tests (one file per concern)
initramfs/init        Custom PID 1 init script for VM guests
```

## Adding a New Capability

1. **FFI binding** (macOS): Add the ObjC wrapper in `src/ffi/apple_vz/`
2. **Config field**: Add to `VmConfig` in `src/config.rs`
3. **Wire into driver**: Use the new FFI/config in `apple_vz.rs` or `cloud_hv.rs`
4. **Test**: Add a smoke test in `tests/ffi_smoke.rs` and a mock test in `tests/vm_manager.rs`
5. **Document**: Update `CAPABILITIES.md` with the correct status

## Pull Requests

- Keep PRs focused — one concern per PR
- All tests must pass (`cargo test`)
- Clippy must be clean (`cargo clippy -- -D warnings`)
- Code must be formatted (`cargo fmt --check`)
- Update CAPABILITIES.md if you change what's supported

## Reporting Issues

Open an issue at https://github.com/8Network/vm-rs/issues with:

- Platform (macOS version / Linux distro)
- Rust version (`rustc --version`)
- Steps to reproduce
- Error output / panic backtrace
