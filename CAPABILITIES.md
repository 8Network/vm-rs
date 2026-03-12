# vm-rs Capability Matrix

Cross-platform VM lifecycle management.
macOS via Apple Virtualization.framework, Linux via Cloud Hypervisor.

## Platform Support

| Capability | macOS (Apple VZ) | Linux (Cloud Hypervisor) | Notes |
|---|---|---|---|
| **Lifecycle** | | | |
| Boot VM | Supported | Supported | Kernel + initramfs + root disk |
| Stop (graceful) | Supported | Supported | requestStopWithError / ACPI shutdown |
| Kill (force) | Supported | Supported | macOS: stopWithCompletionHandler (14+). Linux: SIGKILL |
| Query state | Supported | Supported | Starting → Running → Stopped / Failed |
| Reboot | Not wrapped | Planned | CH: `vm.reboot` or `ch-remote reboot` |
| Pause / Resume | Supported | Planned | VZ: pause/resume APIs. CH: `vm.pause` / `vm.resume` |
| Save / Restore | Supported (FFI) | Planned | VZ: saveMachineStateTo/restoreMachineStateFrom (14+). CH: snapshot |
| Delete/cleanup | Supported | Supported | Remove VM state and resources |
| **CPU** | | | |
| Set vCPU count | Supported | Supported | At boot time |
| CPU hotplug | Not possible | Planned | CH: `vm.resize --cpus N` (up to max_vcpus) |
| CPU topology | Not possible | Planned | Sockets / cores / threads |
| CPU affinity | Not possible | Planned | Pin vCPUs to host cores |
| **Memory** | | | |
| Set memory size | Supported | Supported | At boot time |
| Memory balloon | Supported (FFI wrapped) | Planned | Dynamic guest memory reclaim |
| Memory hotplug | Not possible | Planned | CH: `vm.resize --memory N` |
| Hugepages | Not possible | Planned | 2MB / 1GB pages |
| Memory zones (NUMA) | Not possible | Planned | Per-zone backing and pinning |
| **Storage** | | | |
| Root disk | Supported | Supported | VirtioBlock |
| Seed ISO (cloud-init) | Supported | Supported | Second disk, read-only |
| Data disk | Supported | Supported | Additional VirtioBlock |
| Disk hotplug | Not possible | Planned | CH: `vm.add-disk` |
| Disk resize | Not possible | Planned | CH: `vm.resize-disk` |
| Disk CoW clone | Supported (APFS) | Supported (reflink) | Instant base image cloning |
| Disk caching modes | Supported | Planned | Uncached / Cached / Auto |
| NVMe storage | Supported (FFI) | Planned | VZ: VZNVMExpressControllerDeviceConfiguration (14+). CH: `--disk` |
| NBD (network block) | Not wrapped | Not planned | VZ: VZNetworkBlockDeviceStorageDeviceAttachment (14+) |
| Raw block device | Not wrapped | Not planned | VZ: VZDiskBlockDeviceStorageDeviceAttachment (14+) |
| Disk rate limiting | Not possible | Planned | Token bucket throttle |
| USB mass storage | Not wrapped | Not planned | VZ: VZUSBMassStorageDeviceConfiguration (13+). Hot-plug (15+) |
| **Networking** | | | |
| NAT (internet access) | Supported | N/A | macOS: built-in NAT attachment |
| TAP devices | N/A | Supported | Linux: standard TAP networking |
| Socketpair (L2 switch) | Supported | N/A | macOS: userspace Ethernet switch |
| Custom MAC address | Supported | Supported | Per-VM unique MAC |
| Network hotplug | Not possible | Planned | CH: `vm.add-net` |
| Bridged networking | FFI broken | Planned | macOS: needs `com.apple.vm.networking` entitlement |
| vhost-user net | Not possible | Planned | Offload to external daemon |
| **Shared Directories** | | | |
| VirtioFS | Supported (in-process) | Supported | macOS: native VZ. Linux: virtiofsd sidecar |
| Read-only mounts | Supported | Supported | Immutable shared data |
| Multiple dir shares | Supported (FFI) | Supported | VZ: VZMultipleDirectoryShare (12+) |
| Rosetta (x86 on ARM) | Supported | N/A | VZLinuxRosettaDirectoryShare (13+). Apple Silicon only |
| **Serial Console** | | | |
| File output | Supported | Supported | Log serial to file |
| Readiness detection | Supported | Supported | Parse `VMRS_READY` marker from console |
| File attachment | Supported (FFI) | N/A | VZ: VZFileSerialPortAttachment (14+, simpler than FileHandle) |
| **Entropy** | | | |
| VirtIO RNG | Supported | Supported | /dev/random in guest |
| **Inter-VM Communication** | | | |
| vsock | Supported | Planned | VZ: VZVirtioSocketDevice (11+). CH: CID-based |
| L2 switch (userspace) | Supported | N/A | macOS: learning bridge via socketpairs |
| **Platform Configuration** | | | |
| Generic platform | Supported | N/A | VZGenericPlatformConfiguration (12+) |
| Machine identifier | Supported | N/A | VZGenericMachineIdentifier (13+). Persistent VM identity |
| Nested virtualization | Not wrapped | N/A | macOS 15+: VZGenericPlatformConfiguration.nestedVirtualizationEnabled |
| macOS guest boot | Not wrapped | N/A | VZMacOSBootLoader + VZMacPlatformConfiguration |
| **Boot Modes** | | | |
| Direct Linux boot | Supported | Supported | VZLinuxBootLoader / --kernel |
| UEFI boot | Supported | Planned | VZ: VZEFIBootLoader + VZEFIVariableStore (13+) |
| **Security** | | | |
| TPM | Not wrapped | Planned | Virtual Trusted Platform Module |
| Entitlement signing | Supported | N/A | `com.apple.security.virtualization` |
| **Other Devices** | | | |
| Clipboard sharing | Not wrapped | N/A | VZ: VZSpiceAgentPortAttachment (13+) |
| Watchdog | N/A | Planned | Guest hang recovery |
| Graphics/Display | Not wrapped | Not planned | Headless VMs only |
| Audio | Not wrapped | Not planned | Not needed for server VMs |

## Implementation Status Legend

**Supported** = works now, tested.
**Planned** = the underlying platform supports it; we haven't wired it yet.
**Not possible** = the platform does not offer this capability.
**Not wrapped** = Apple VZ framework supports it, but our FFI bindings don't expose it yet.
**FFI broken** = FFI exists but has known bugs (see REVIEW.md).
**Not planned** = technically possible but not in scope (server-oriented crate).

## What to Implement Next (Priority Order)

### P0 — Critical for reliability (RESOLVED)

All P0 items have been addressed in the current codebase:

1. ~~**VmManager::start() race condition**~~ — **Fixed**: write lock held across duplicate check + boot + insert.
2. ~~**NetworkSwitch FD use-after-close**~~ — **Fixed**: `stop()` joins forwarding thread before Drop closes FDs.
3. ~~**Linux kill_by_handle PID reuse**~~ — **Mitigated**: `Child` handle prevents PID reuse; raw PID fallback is documented and warned.
4. ~~**Image download OOM**~~ — **Fixed**: uses `bytes_stream()` for streaming downloads.

### P1 — Critical for usability (DONE — macOS)

1. ~~**vsock (VZVirtioSocketDevice)**~~ — **Done**: Full FFI (config + runtime + connect + listen + connection FD). Wired into driver via `VmConfig::vsock`. Enable with `vsock: true`.

2. **Cloud Hypervisor API mode** — Switch from CLI-only to API socket mode. Required for all "Planned" CH features (hotplug, resize, pause, reboot, metrics). Single biggest unlock for Linux capabilities. **Still TODO**.

3. ~~**VZGenericPlatformConfiguration + VZGenericMachineIdentifier**~~ — **Done**: Auto-configured on every boot. Machine ID persisted in `VmHandle::machine_id` for caller to save/restore via `VmConfig::machine_id`.

4. ~~**UEFI boot (VZEFIBootLoader)**~~ — **Done**: Set `VmConfig::efi_variable_store` to enable. Auto-creates or opens the variable store file.

### P2 — Valuable enhancements (DONE — macOS)

5. ~~**Rosetta (VZLinuxRosettaDirectoryShare)**~~ — **Done**: Enable with `VmConfig::rosetta: true`. Auto-checks availability, adds VirtioFS share at tag "rosetta".

6. ~~**Pause / Resume**~~ — **Done**: `VmDriver::pause()` / `resume()` + `VmManager::pause()` / `resume()`. Apple VZ fully implemented. `VmState::Paused` added.

7. ~~**Save / Restore (VM snapshots)**~~ — **Done (FFI)**: `VZVirtualMachine::save_machine_state_to_url()` / `restore_machine_state_from_url()` wrapped. Higher-level driver integration can be added when needed.

8. ~~**Multiple directory shares**~~ — **Done (FFI)**: `VZMultipleDirectoryShare` wrapped. Takes a map of tag → `VZSharedDirectory` pairs.

9. ~~**NVMe storage**~~ — **Done (FFI)**: `VZNVMExpressControllerDeviceConfiguration` wrapped. Same attachment API as VirtioBlock.

10. ~~**VZFileSerialPortAttachment**~~ — **Done (FFI)**: Wrapped. Takes a file path + append flag. Simpler than the FileHandle approach.

### Remaining work

1. **Cloud Hypervisor API mode** (P1) — the only P1 item not yet done. Requires HTTP-over-Unix-socket client for `ch-remote` equivalent.
2. **CH pause/resume** — needs API mode first.
3. **Wire NVMe, multiple dir shares, file serial attachment into driver** — FFI is done but not yet wired into the Apple VZ driver's boot path. Easy to add when needed.

## Driver Architecture

### macOS: Apple Virtualization.framework

In-process VM management. The VZ framework runs inside our process via Objective-C FFI.
VMs are created as `VZVirtualMachine` objects on a per-VM GCD serial dispatch queue.
We use `Box::leak` to keep them alive (ObjC reference counting requires the object to
outlive the queue).

**Thread safety model**: All ObjC/VZ calls are dispatched to the VM's GCD queue via
`dispatch_async`. The calling thread never makes ObjC calls directly. This prevents
autoreleased objects from accumulating in the caller's TLS autorelease pool (which
caused SIGSEGV during thread exit).

**Key constraint**: Most capabilities are fixed at boot. No hotplug, no resize.
The framework supports pause/resume and save/restore, but not dynamic device changes.

**Minimum macOS**: 13.0 (Ventura) recommended. Gets you UEFI, Rosetta, vsock,
virtiofs, generic machine identity, and console devices. 14.0 adds NVMe and NBD.
15.0 adds USB hot-plug.

### Linux: Cloud Hypervisor

Separate VMM process. We spawn `cloud-hypervisor` and manage the process lifecycle.

**Two modes:**

1. **CLI mode** (current) — boot with `cloud-hypervisor --kernel ... --cpus ... --disk ...`.
   Stop = SIGTERM (graceful ACPI), Kill = SIGKILL, State = process alive check.
   Simple, no dependencies, covers basic lifecycle.

2. **API mode** (future) — boot with `cloud-hypervisor --api-socket /path/to/sock`.
   Control via `ch-remote` CLI or direct HTTP over Unix socket.
   Required for: hotplug, resize, snapshot, restore, live migration, pause/resume.

We start with CLI mode and add API mode when advanced features are needed.
The `VmDriver` trait is designed to accommodate both.

## Cloud Hypervisor API Reference

For advanced features (API mode), Cloud Hypervisor exposes these endpoints
via HTTP over Unix socket. The `ch-remote` CLI wraps all of them.

| Endpoint | Method | Purpose |
|---|---|---|
| `vmm.ping` | GET | Health check |
| `vmm.shutdown` | PUT | Kill VMM process |
| `vm.create` | PUT | Create VM from JSON config |
| `vm.boot` | PUT | Start created VM |
| `vm.shutdown` | PUT | Graceful ACPI shutdown |
| `vm.reboot` | PUT | Guest OS restart |
| `vm.delete` | PUT | Remove VM |
| `vm.info` | GET | Query VM state and config |
| `vm.pause` | PUT | Suspend execution |
| `vm.resume` | PUT | Resume from pause |
| `vm.resize` | PUT | Change CPU count or memory size |
| `vm.resize-disk` | PUT | Expand disk |
| `vm.resize-zone` | PUT | Expand memory zone |
| `vm.add-disk` | PUT | Hotplug block device |
| `vm.add-net` | PUT | Hotplug NIC |
| `vm.add-fs` | PUT | Hotplug VirtioFS mount |
| `vm.add-device` | PUT | Hotplug VFIO device |
| `vm.add-vsock` | PUT | Hotplug vsock |
| `vm.add-pmem` | PUT | Hotplug persistent memory |
| `vm.add-vdpa` | PUT | Hotplug vDPA device |
| `vm.add-user-device` | PUT | Hotplug userspace device |
| `vm.remove-device` | PUT | Hot-unplug device |
| `vm.snapshot` | PUT | Snapshot full VM state |
| `vm.restore` | PUT | Resume from snapshot |
| `vm.send-migration` | PUT | Live migration (outbound) |
| `vm.receive-migration` | PUT | Live migration (inbound) |
| `vm.counters` | GET | Performance metrics |
| `vm.power-button` | PUT | Simulate power button |
| `vm.nmi` | PUT | Non-maskable interrupt |
| `vm.coredump` | PUT | Debug memory dump (x86_64) |

## Apple VZ FFI Coverage

Our FFI bindings (absorbed from virtualization-rs, MIT license) wrap:

| VZ Class | Wrapped | Used | Status |
|---|---|---|---|
| VZLinuxBootLoader | Yes | Yes | Working |
| VZVirtualMachineConfiguration | Yes | Yes | Working |
| VZVirtualMachine | Yes | Yes | Working (start, stop, kill, state, pause, resume, save, restore) |
| VZVirtioBlockDeviceConfiguration | Yes | Yes | Working |
| VZDiskImageStorageDeviceAttachment | Yes | Yes | Working (with caching/sync modes) |
| VZVirtioNetworkDeviceConfiguration | Yes | Yes | Working |
| VZNATNetworkDeviceAttachment | Yes | Yes | Working |
| VZFileHandleNetworkDeviceAttachment | Yes | Yes | Working |
| VZBridgedNetworkDeviceAttachment | Yes | No | FFI broken (see REVIEW.md) |
| VZVirtioConsoleDeviceSerialPortConfiguration | Yes | Yes | Working |
| VZFileHandleSerialPortAttachment | Yes | Yes | Working |
| VZVirtioEntropyDeviceConfiguration | Yes | Yes | Working |
| VZVirtioTraditionalMemoryBalloonDeviceConfiguration | Yes | Yes | Working |
| VZVirtioFileSystemDeviceConfiguration | Yes | Yes | Working |
| VZSingleDirectoryShare | Yes | Yes | Working |
| VZSharedDirectory | Yes | Yes | Working |
| VZMACAddress | Yes | Yes | Working (fixed retain/release) |
| VZSocketDeviceConfiguration (trait) | Yes | Yes | Working with concrete VZVirtioSocketDeviceConfiguration |
| VZVirtioSocketDeviceConfiguration | Yes | Yes | Working — vsock config |
| VZVirtioSocketDevice | Yes | Yes | Working — runtime connect/listen |
| VZVirtioSocketListener | Yes | Yes | Working — accept guest connections |
| VZVirtioSocketConnection | Yes | Yes | Working — FD-based bidirectional I/O |
| VZGenericPlatformConfiguration | Yes | Yes | Working — Linux VM platform (12+) |
| VZGenericMachineIdentifier | Yes | Yes | Working — persistent VM identity (13+) |
| VZEFIBootLoader | Yes | Yes | Working — UEFI boot (13+) |
| VZEFIVariableStore | Yes | Yes | Working — UEFI NVRAM (13+) |
| VZLinuxRosettaDirectoryShare | Yes | Yes | Working — x86 on ARM translation (13+) |
| VZMultipleDirectoryShare | Yes | No | Working — multi-dir VirtioFS (12+) |
| VZFileSerialPortAttachment | Yes | No | Working — simpler serial logging (14+) |
| VZNVMExpressControllerDeviceConfiguration | Yes | No | Working — NVMe storage (14+) |
| **Not yet wrapped** | | | |
| VZMacOSBootLoader | No | — | Not planned (macOS guest) |
| VZMacPlatformConfiguration | No | — | Not planned (macOS guest) |
| VZGraphicsDeviceConfiguration | No | — | Not planned (headless) |
| VZAudioDeviceConfiguration | No | — | Not planned (headless) |
| VZUSBControllerConfiguration | No | — | Not planned (15+ only) |
| VZSpiceAgentPortAttachment | No | — | Low priority (clipboard) |
