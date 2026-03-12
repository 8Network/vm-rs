# vm-rs Capability Matrix

Cross-platform VM lifecycle management.
macOS via Apple Virtualization.framework, Linux via Cloud Hypervisor.

## Status Legend

| Status | Meaning |
|--------|---------|
| **Verified** | Implemented, wired into driver, tested |
| **Wired** | Implemented and used in driver boot path, not yet integration-tested on real hardware |
| **FFI only** | ObjC FFI binding exists but not wired into the driver |
| **Planned** | Platform supports it; not yet implemented |
| **N/A** | Platform does not offer this capability |

## Platform Support

| Capability | macOS (Apple VZ) | Linux (Cloud Hypervisor) | Notes |
|---|---|---|---|
| **Lifecycle** | | | |
| Boot VM | Verified | Verified | Direct Linux boot or UEFI |
| Stop (graceful) | Verified | Verified | VZ: requestStopWithError. CH: SIGTERM (ACPI) |
| Kill (force) | Verified | Verified | VZ: stopWithCompletionHandler. CH: SIGKILL |
| Query state | Verified | Verified | Starting → Running → Stopped / Failed |
| Pause / Resume | Wired | Planned | VZ: wired + mock-tested. CH: needs API mode |
| Save / Restore | FFI only | Planned | VZ: saveMachineStateTo/restoreMachineStateFrom (14+) |
| Delete/cleanup | Verified | Verified | Registry removal / process reap |
| **CPU** | | | |
| Set vCPU count | Verified | Verified | At boot time |
| CPU hotplug | N/A | Planned | CH: `vm.resize --cpus N` (needs API mode) |
| **Memory** | | | |
| Set memory size | Verified | Verified | At boot time |
| Memory balloon | Wired | Planned | VZ: device added at boot, no runtime inflate/deflate yet |
| **Storage** | | | |
| Root disk (VirtioBlock) | Verified | Verified | Optional — diskless boot supported |
| Seed ISO (cloud-init) | Verified | Verified | Second disk, read-only |
| Data disk | Wired | Wired | Additional VirtioBlock device |
| Disk CoW clone | Verified | Verified | macOS: APFS `cp -c`. Linux: `cp --reflink=auto` |
| Disk caching modes | Wired | Planned | VZ: Uncached / Cached / Auto |
| NVMe storage | FFI only | Planned | VZ: VZNVMExpressControllerDeviceConfiguration (14+) |
| **Networking** | | | |
| NAT (internet) | Verified | N/A | macOS: built-in VZ NAT attachment |
| TAP devices | N/A | Verified | Linux: standard TAP networking |
| Socketpair (L2 switch) | Verified | N/A | Userspace Ethernet switch with MAC learning |
| Custom MAC address | Verified | Verified | Per-VM unique MAC |
| Bridged networking | FFI only | Planned | macOS: needs `com.apple.vm.networking` entitlement |
| **Shared Directories** | | | |
| VirtioFS | Verified | Verified | macOS: native VZ. Linux: virtiofsd sidecar |
| Read-only mounts | Verified | Verified | Per-share flag |
| Multiple dir shares | FFI only | Verified | VZ FFI: VZMultipleDirectoryShare. Linux: multiple virtiofsd |
| Rosetta (x86 on ARM) | Wired | N/A | Availability check + VirtioFS share. Apple Silicon only |
| **Serial Console** | | | |
| File output | Verified | Verified | Serial log → file |
| Readiness detection | Verified | Verified | Parse `VMRS_READY <ip>` from serial log |
| File attachment | FFI only | N/A | VZ: VZFileSerialPortAttachment (14+) |
| **Entropy** | | | |
| VirtIO RNG | Wired | Verified | VZ: device added at boot. CH: built-in |
| **Inter-VM Communication** | | | |
| vsock | Wired | Planned | VZ: config + runtime FFI. CH: CID-based (needs API mode) |
| L2 switch (userspace) | Verified | N/A | MAC learning bridge via socketpairs |
| **Platform Configuration** | | | |
| Generic platform | Wired | N/A | VZGenericPlatformConfiguration (12+) |
| Machine identifier | Wired | N/A | Persistent VM identity, roundtrip via VmHandle |
| **Boot Modes** | | | |
| Direct Linux boot | Verified | Verified | Kernel + initramfs + optional cmdline |
| UEFI boot | Wired | Planned | VZ: VZEFIBootLoader + VZEFIVariableStore (13+) |
| **Security** | | | |
| Entitlement signing | Verified | N/A | `com.apple.security.virtualization` required |
| Boot completion check | Verified | N/A | Waits for start completion handler before returning |
| Atomic downloads | Verified | Verified | Temp file → fsync → rename. SHA-256 verification |
| Shell injection prevention | Verified | Verified | YAML arrays, `shell_quote()`, input validators |

## Apple VZ FFI Coverage

| VZ Class | Wrapped | Used in Driver | Tested |
|---|---|---|---|
| VZLinuxBootLoader | Yes | Yes | Smoke |
| VZEFIBootLoader | Yes | Yes | Smoke |
| VZEFIVariableStore | Yes | Yes | Smoke |
| VZVirtualMachineConfiguration | Yes | Yes | Smoke |
| VZVirtualMachine | Yes | Yes | Smoke |
| VZVirtioBlockDeviceConfiguration | Yes | Yes | - |
| VZDiskImageStorageDeviceAttachment | Yes | Yes | - |
| VZVirtioNetworkDeviceConfiguration | Yes | Yes | - |
| VZNATNetworkDeviceAttachment | Yes | Yes | - |
| VZFileHandleNetworkDeviceAttachment | Yes | Yes | - |
| VZVirtioConsoleDeviceSerialPortConfiguration | Yes | Yes | - |
| VZFileHandleSerialPortAttachment | Yes | Yes | - |
| VZVirtioEntropyDeviceConfiguration | Yes | Yes | - |
| VZVirtioTraditionalMemoryBalloonDeviceConfiguration | Yes | Yes | - |
| VZVirtioFileSystemDeviceConfiguration | Yes | Yes | - |
| VZSingleDirectoryShare | Yes | Yes | Smoke |
| VZSharedDirectory | Yes | Yes | - |
| VZMACAddress | Yes | Yes | - |
| VZVirtioSocketDeviceConfiguration | Yes | Yes | Smoke |
| VZVirtioSocketDevice | Yes | No (runtime) | Smoke |
| VZVirtioSocketListener | Yes | No (runtime) | Smoke |
| VZVirtioSocketConnection | Yes | No (runtime) | - |
| VZGenericPlatformConfiguration | Yes | Yes | Smoke |
| VZGenericMachineIdentifier | Yes | Yes | Smoke |
| VZLinuxRosettaDirectoryShare | Yes | Yes | Smoke |
| VZMultipleDirectoryShare | Yes | No | Smoke |
| VZFileSerialPortAttachment | Yes | No | Smoke |
| VZNVMExpressControllerDeviceConfiguration | Yes | No | Smoke |
| VZBridgedNetworkDeviceAttachment | Yes | No | - |

**Not wrapped** (out of scope): VZMacOSBootLoader, VZMacPlatformConfiguration,
VZGraphicsDeviceConfiguration, VZAudioDeviceConfiguration, VZUSBControllerConfiguration,
VZSpiceAgentPortAttachment.

## Test Coverage

| Test Suite | Tests | What It Covers |
|---|---|---|
| Unit tests (`cargo test --lib`) | 78 | Config, OCI parsing, seed generation, network switch, image resolution |
| `tests/ffi_smoke.rs` | 18 | FFI object creation on real macOS VZ framework (macOS only) |
| `tests/vm_manager.rs` | 27 | VmManager orchestration with mock driver (cross-platform) |
| `tests/vm_lifecycle.rs` | 10 | Mock driver lifecycle transitions |
| `tests/network_switch.rs` | 8 | L2 switch: forwarding, learning, isolation, broadcast |
| `tests/oci_pull.rs` | 7 | Docker Hub / GHCR pull, layer extraction, idempotency |
| `tests/seed_iso.rs` | 4 | Cloud-init ISO creation with hdiutil/genisoimage |
| `tests/disk_clone.rs` | 4 | APFS/reflink CoW cloning |

**Total: 156 tests.** CI runs on both macOS and Linux.

## Roadmap

### Next up

1. **Cloud Hypervisor API mode** — HTTP-over-Unix-socket for `ch-remote` equivalent.
   Unlocks: pause/resume, hotplug, resize, snapshot, metrics on Linux.

2. **Wire remaining FFI into driver** — NVMe, multiple directory shares, file serial
   attachment. FFI is done; just needs plumbing in the Apple VZ boot path.

3. **vsock runtime API** — Expose connect/listen/accept at the VmManager level
   for host-guest communication without network setup.

## Architecture

### macOS: Apple Virtualization.framework

In-process VM management via Objective-C FFI. VMs are `VZVirtualMachine` objects
stored in `Pin<Box<>>` on a per-VM GCD serial dispatch queue. All ObjC calls are
dispatched to the queue via `dispatch_async` — the calling thread never touches
ObjC directly (prevents autorelease pool SIGSEGV at thread exit).

**Key constraint**: Most devices are fixed at boot. No hotplug, no resize.
Pause/resume and save/restore are supported.

**Minimum macOS**: 13.0 (Ventura). Gets UEFI, Rosetta, vsock, VirtioFS,
machine identity. 14.0 adds NVMe. 15.0 adds USB hot-plug.

### Linux: Cloud Hypervisor

Separate VMM process. Currently CLI mode (`cloud-hypervisor --kernel ...`).
Process lifecycle via signals: SIGTERM (graceful ACPI), SIGKILL (force).
State via `Child::try_wait()` (safe from PID reuse).

VirtioFS via virtiofsd sidecar processes (one per shared directory).

**Future**: API socket mode for advanced features.
