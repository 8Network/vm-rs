# vm-rs Capability Matrix

Cross-platform VM lifecycle management.
macOS via Apple Virtualization.framework, Linux via Cloud Hypervisor.

## Platform Support

| Capability | macOS (Apple VZ) | Linux (Cloud Hypervisor) | Notes |
|---|---|---|---|
| **Lifecycle** | | | |
| Boot VM | Supported | Supported | Kernel + initramfs + root disk |
| Stop (graceful) | Supported | Supported | ACPI shutdown |
| Kill (force) | Supported | Supported | Immediate termination |
| Query state | Supported | Supported | Starting → Running → Stopped / Failed |
| Reboot | Not supported | Planned | CH: `vm.reboot` or `ch-remote reboot` |
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
| Disk rate limiting | Not possible | Planned | Token bucket throttle |
| **Networking** | | | |
| NAT (internet access) | Supported | N/A | macOS: built-in NAT attachment |
| TAP devices | N/A | Supported | Linux: standard TAP networking |
| Socketpair (L2 switch) | Supported | N/A | macOS: userspace Ethernet switch |
| Custom MAC address | Supported | Supported | Per-VM unique MAC |
| Network hotplug | Not possible | Planned | CH: `vm.add-net` |
| Bridged networking | Supported (FFI wrapped) | Planned | Direct host NIC bridge |
| vhost-user net | Not possible | Planned | Offload to external daemon |
| **Shared Directories** | | | |
| VirtioFS | Supported (in-process) | Planned | macOS: native VZ. Linux: virtiofsd sidecar |
| Read-only mounts | Supported | Planned | Immutable shared data |
| **Serial Console** | | | |
| File output | Supported | Supported | Log serial to file |
| Readiness detection | Supported | Supported | Parse `8STACK_READY` marker from console |
| **Entropy** | | | |
| VirtIO RNG | Supported | Supported | /dev/random in guest |
| **Advanced (API mode)** | | | |
| Pause / Resume | Not possible | Planned | CH: `vm.pause` / `vm.resume` |
| Snapshot | Not possible | Planned | CH: `vm.snapshot` — full state dump |
| Restore | Not possible | Planned | CH: `vm.restore` — resume from snapshot |
| Live migration | Not possible | Planned | CH: `vm.send-migration` / `vm.receive-migration` |
| VM counters/metrics | Not possible | Planned | CH: `vm.counters` |
| **Device Passthrough** | | | |
| VFIO (GPU, NIC, etc.) | Not possible | Planned | CH: `vm.add-device` |
| vDPA | Not possible | Planned | Hardware-accelerated virtio |
| **Inter-VM Communication** | | | |
| vsock | Not possible | Planned | CH: CID-based VM-to-VM / VM-to-host |
| L2 switch (userspace) | Supported | N/A | macOS: learning bridge via socketpairs |
| **Platform-Specific** | | | |
| Rosetta (x86 on ARM) | Not wrapped | N/A | Apple Silicon: run x86_64 Linux binaries |
| macOS guest boot | Not wrapped | N/A | VZMacOSBootLoader |
| UEFI boot | Not wrapped | Planned | EFI variable store + firmware |
| TPM | Not wrapped | Planned | Virtual Trusted Platform Module |
| Watchdog | N/A | Planned | Guest hang recovery |
| Graphics/Display | Not wrapped | Not planned | Headless VMs only |
| Audio | Not wrapped | Not planned | Not needed for server VMs |

## Implementation Status

**Supported** = works now, tested.
**Planned** = the underlying platform supports it; we haven't wired it yet.
**Not possible** = the platform does not offer this capability.
**Not wrapped** = Apple VZ framework supports it, but our FFI bindings don't expose it yet.
**Not planned** = technically possible but not in scope (server-oriented crate).

## Driver Architecture

### macOS: Apple Virtualization.framework

In-process VM management. The VZ framework runs inside our process via Objective-C FFI.
VMs are created as `VZVirtualMachine` objects on a dispatch queue. We use `Box::leak`
to keep them alive (ObjC reference counting requires the object to outlive the queue).

Capabilities are fixed at boot — no hotplug, no resize, no snapshot.
The framework is designed for desktop virtualization (macOS guests, Linux guests with GUI).
We use only the headless Linux server subset.

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

| VZ Class | Wrapped | Used by Driver |
|---|---|---|
| VZLinuxBootLoader | Yes | Yes |
| VZVirtualMachineConfiguration | Yes | Yes |
| VZVirtualMachine | Yes | Yes |
| VZVirtioBlockDeviceConfiguration | Yes | Yes |
| VZDiskImageStorageDeviceAttachment | Yes | Yes |
| VZVirtioNetworkDeviceConfiguration | Yes | Yes |
| VZNATNetworkDeviceAttachment | Yes | Yes |
| VZFileHandleNetworkDeviceAttachment | Yes | Yes |
| VZBridgedNetworkDeviceAttachment | Yes | No |
| VZVirtioConsoleDeviceSerialPortConfiguration | Yes | Yes |
| VZFileHandleSerialPortAttachment | Yes | Yes |
| VZVirtioEntropyDeviceConfiguration | Yes | Yes |
| VZVirtioTraditionalMemoryBalloonDeviceConfiguration | Yes | Yes |
| VZVirtioFileSystemDeviceConfiguration | Yes | Yes |
| VZSharedDirectory | Yes | Yes |
| VZMACAddress | Yes | Yes |
| VZEFIBootLoader | No | — |
| VZMacOSBootLoader | No | — |
| VZGraphicsDeviceConfiguration | No | — |
| VZAudioDeviceConfiguration | No | — |
| VZUSBDeviceConfiguration | No | — |
| VZTPMDeviceConfiguration | No | — |
| VZLinuxRosettaDirectoryShare | No | — |
