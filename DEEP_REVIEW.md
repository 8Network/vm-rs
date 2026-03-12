# vm-rs Deep Repository Review

Reviewed on 2026-03-12 against the current workspace state.

Validation run:
- `cargo test --all-targets`: passed on this machine
- `cargo clippy --all-targets -- -D warnings`: failed

Important context:
- The workspace is dirty, so this review is against the current local tree, not a clean release tag.
- The real hypervisor lifecycle tests did not exercise a real guest on this machine because the required env vars were not configured.

## Executive Summary

The project has a strong overall shape: the `VmDriver` split is sensible, the OCI extraction path already contains meaningful security hardening, and the tests are broader than average for a young systems crate.

The main blockers for a critical open-source release are not cosmetic. They are:

1. unsafe command construction in guest bootstrap paths
2. Apple VZ lifecycle correctness problems
3. ownership/resource leaks across VM and networking boundaries
4. image preparation and supply-chain integrity gaps
5. some important doc and API mismatches

## Highest-Priority Findings

### 1. Critical: cloud-init user-data treats untrusted values as shell source

Files:
- `src/setup/seed.rs:138-180`

Problem:
- `vol.mount_point`, `vol.tag`, `proc.command`, and `healthcheck.command` are interpolated directly into shell commands under `runcmd`.
- Only env values and working directory get shell quoting. The command itself does not.

Impact:
- command injection
- malformed YAML/shell on spaces or special characters
- guest startup behavior that depends on caller string hygiene instead of crate guarantees

Required fix:
- stop modeling commands as free-form shell strings
- represent command + args as structured data
- generate either YAML arrays or a generated script with strict escaping

### 2. High: the Apple VZ driver can report boot success before Apple says the VM actually started

Files:
- `src/driver/apple_vz.rs:446-463`
- `src/driver/apple_vz.rs:487-492`
- `src/driver/apple_vz.rs:631-645`

Problem:
- `boot()` returns success after scheduling `startWithCompletionHandler`, not after the completion result arrives.
- The completion handler only logs failures.
- `VZVirtualMachineStateError` is flattened into `VmState::Stopped`.

Impact:
- callers can observe a successful boot for a VM that never really started
- startup failures lose their reason
- automation cannot distinguish "boot failed" from "stopped later"

Required fix:
- feed the start completion result back into `boot()`
- retain the last framework error per VM
- map framework error state to `VmState::Failed`

### 3. High: the Apple VZ path leaks runtime objects and has unclear fd ownership

Files:
- `src/driver/apple_vz.rs:64-68`
- `src/driver/apple_vz.rs:429-443`
- `src/driver/apple_vz.rs:554-605`
- `src/config.rs:85-90`
- `src/network/switch.rs:99-128`
- `src/network/switch.rs:179-200`
- `src/driver/apple_vz.rs:279-286`

Problem:
- `Box::leak` makes every `VZVirtualMachine` permanent.
- removing an entry from `VM_REGISTRY` does not free the leaked allocation.
- `SocketPairFd(RawFd)` has no ownership semantics.
- `NetworkSwitch` closes only the switch side; the Apple path wraps the VM side as a borrowed file handle, so the crate does not clearly own or close that fd.

Impact:
- memory/resource growth in long-lived daemons
- eventual fd exhaustion under churn
- ambiguous public API contract

Required fix:
- move to explicit owned VM state instead of leaked `'static` references
- use `OwnedFd` or an equivalent RAII type in the public networking config
- make ownership transfer explicit at the boundary

### 4. High: `prepare_image()` contradicts the advertised initramfs-only boot model

Files:
- `src/config.rs:14-26`
- `src/setup/image.rs:121-141`
- `src/setup/image.rs:167-215`

Problem:
- `VmConfig` explicitly supports initramfs-only boot with no root disk.
- `resolve_alpine()` returns only kernel + initramfs.
- `prepare_image()` still requires a disk image unconditionally and returns `PreparedImage { disk: PathBuf }`.

Impact:
- Alpine support is incomplete at the API level
- any diskless/initramfs-only image spec fails in `prepare_image()`
- the type model pushes callers toward a disk even when the runtime does not need one

Required fix:
- make `PreparedImage.disk` optional
- align `prepare_image()` with the boot modes documented in `VmConfig`
- add end-to-end tests for a diskless image spec

### 5. High: image downloads are neither atomic nor cryptographically verified

Files:
- `src/setup/image.rs:169-178`
- `src/setup/image.rs:228-265`
- `src/setup/image.rs:293-333`

Problem:
- cache hits are based on `path.exists()`
- downloads write directly to the final destination
- verification only checks non-zero size and logs the computed SHA-256; it does not compare against a trusted expected digest or signature

Impact:
- interrupted downloads can poison the cache
- a truncated artifact may survive future retries
- boot-critical assets have no authenticity guarantee

Required fix:
- write to temp files, verify, fsync, atomic rename
- keep or fetch expected digests/signatures

### 6. High: the custom initramfs is not production-safe by default

Files:
- `initramfs/init:178-185`
- `initramfs/init:321-333`

Problem:
- when no SSH key is present, the initramfs starts Dropbear with blank-password auth enabled
- workload execution falls back to `sh -c "$VM_WORKLOAD $VM_WORKLOAD_ARGS"`

Impact:
- dangerous default for an image intended to boot network-reachable guests
- command injection risk through kernel cmdline values

Required fix:
- fail closed when no key is configured, or gate insecure SSH behind an explicit debug mode
- stop concatenating workload strings into `sh -c`

### 7. Medium: the Linux driver destroys caller-provided TAP devices and weakens VirtioFS isolation

Files:
- `src/driver/cloud_hv.rs:121-128`
- `src/driver/cloud_hv.rs:157-168`
- `src/driver/cloud_hv.rs:299-323`
- `src/driver/cloud_hv.rs:538-557`

Problem:
- cleanup always deletes the TAP device name supplied by the caller
- `virtiofsd` is started with `--sandbox=none`

Impact:
- externally managed TAPs can be torn down unexpectedly
- host isolation is weaker than it should be for shared-directory workloads

Required fix:
- track ownership of created resources
- use a sandboxed `virtiofsd` mode by default

### 8. Medium: several parser/FFI boundaries still accept malformed inputs too quietly

Files:
- `src/oci/store.rs:220-226`
- `src/oci/registry.rs:153-163`
- `src/ffi/apple_vz/serial_port.rs:150-178`

Problem:
- OCI layers with missing digests become `""` instead of parse errors
- `localhost/repo` is misparsed as Docker Hub instead of a local registry
- `VZFileSerialPortAttachment::new()` allocates an `error` variable but ignores it completely

Impact:
- malformed inputs fail later and less clearly
- local-registry compatibility bug
- nil/error states can escape the FFI wrapper unchecked

Required fix:
- reject invalid manifests early
- special-case `localhost`
- make the serial-port FFI constructor return `Result`

### 9. Medium: CI/tooling posture is weaker than the repository claims

Files:
- `Cargo.toml:61-62`
- `CAPABILITIES.md:93-100`
- `src/vm.rs:92-93`
- `tests/vm_lifecycle.rs:96-101`
- `tests/vm_lifecycle.rs:144-150`

Problem:
- clippy does not pass under `-D warnings`
- `CAPABILITIES.md` claims the `VmManager::start()` race was fixed by holding the write lock across boot + insert, but the implementation explicitly drops the lock before boot
- real-driver tests are opt-in and skipped in the current run

Impact:
- documentation drift
- false confidence for outside contributors
- release quality depends on manual operator setup

Required fix:
- make the docs match the implementation
- add at least one required CI path that exercises a real driver
- either relax or actually satisfy the lint posture

## File-by-File Review

### Root / Packaging / Docs

| File | Review |
|---|---|
| `Cargo.toml` | Dependency set is reasonable. The main issue is process posture: `unwrap_used` is only `warn`, while strict clippy still fails on test code. |
| `Cargo.lock` | Present and normal for an application-like crate; no review issue on its own. |
| `CAPABILITIES.md` | Useful roadmap, but it has drift. The most concrete mismatch is the claim that `VmManager::start()` holds the write lock across boot + insert, which is not true in `src/vm.rs`. Also the matrix overstates "Supported" in a few places where failure reporting is still weak. |
| `LICENSE` | Standard MIT license, no issue. |
| `clippy.toml` | Minimal and fine. The practical issue is not this file, it is that clippy still fails repo-wide under a strict invocation. |
| `entitlements.plist` | Contains only `com.apple.security.virtualization`. Fine for core Apple VZ, but any future bridged networking work will also need the relevant Apple networking entitlement. |
| `DEEP_REVIEW.md` | This report file. |

### Core crate surface

| File | Review |
|---|---|
| `src/lib.rs` | Clean public surface. Re-exports are reasonable. No major issue. |
| `src/config.rs` | Good type centralization and boot-mode documentation. Main concern is `NetworkAttachment::SocketPairFd(RawFd)`, which exposes a raw-ownership API that the rest of the crate does not manage safely. |
| `src/vm.rs` | Overall manager design is good and the placeholder technique is sound. Main issues are doc drift around the write lock, plus the fact that manager behavior depends on drivers surfacing failure states correctly, which the Apple driver currently does not. |

### Drivers

| File | Review |
|---|---|
| `src/driver/mod.rs` | The trait boundary is good. `pause()`/`resume()` default methods are pragmatic. No major design issue here. |
| `src/driver/apple_vz.rs` | Highest-risk file in the crate. Boot failure propagation is incomplete, `Box::leak` makes lifecycle cleanup impossible, and fd ownership is unclear on socketpair networking. The queue confinement model is thoughtful, but the object-lifetime model needs redesign. |
| `src/driver/cloud_hv.rs` | Generally easier to reason about than the Apple path. Main problems are caller-owned TAP deletion, `virtiofsd --sandbox=none`, and some cleanup semantics that assume the driver owns all network resources. |

### Networking

| File | Review |
|---|---|
| `src/network/mod.rs` | Simple module surface, fine. |
| `src/network/switch.rs` | Functional and fairly well tested. Main issue is the API returns raw vm-side fds with no ownership model. Also the in-file tests use `unwrap()`, which is why strict clippy fails. |
| `src/network/bridge.rs` | Linux bridge setup is straightforward, but `delete_bridge()` is misnamed because it only removes NAT rules and does not actually delete the bridge device. That will surprise API consumers. |
| `src/network/port_forward.rs` | Small and readable. `stop()` aborts the task immediately, which is acceptable for now but not graceful; active connections are dropped rather than drained. |

### OCI

| File | Review |
|---|---|
| `src/oci/mod.rs` | Clean module surface, no issue. |
| `src/oci/registry.rs` | Good use of typed structs over raw `Value`. Main correctness bug is `localhost/...` parsing. Auth flow is pragmatic but still optimistic for some private-registry challenge flows. |
| `src/oci/store.rs` | Strongest security work in the repo: traversal and whiteout handling are better than average. Main bug is accepting empty layer digests, plus several test-only unwraps that break strict clippy. |

### Setup

| File | Review |
|---|---|
| `src/setup/mod.rs` | Fine as a module façade. No major issue. |
| `src/setup/image.rs` | One of the more important design gaps. The API shape still assumes a disk is mandatory, which contradicts initramfs-only boot. Also downloads are not atomic and not verified against trusted digests. |
| `src/setup/seed.rs` | The core problem is shell construction from raw strings. This file needs the biggest security redesign after the Apple driver. |
| `src/setup/ssh.rs` | Straightforward wrapper around `ssh-keygen`. Fine for now. For library polish, consider validating output file permissions explicitly after generation. |

### FFI module root

| File | Review |
|---|---|
| `src/ffi/mod.rs` | Minimal and correct. |
| `src/ffi/apple_vz/mod.rs` | Good high-level safety contract, but the submodules do not yet uniformly enforce it at the type level. |
| `src/ffi/apple_vz/LICENSE-virtualization-rs.md` | Proper attribution, no issue. |

### Apple VZ FFI: base and config objects

| File | Review |
|---|---|
| `src/ffi/apple_vz/base.rs` | A lot of careful retain/release handling is already present. The main rough edge is `NSString::as_str()` using `expect`, and more generally the wrappers still rely on panic paths where `Result` would be cleaner. |
| `src/ffi/apple_vz/boot_loader.rs` | The Linux bootloader builder is decent. `VZEFIVariableStore::{create,open}` are among the more robust wrappers in the FFI layer. No major issue beyond the general Objective-C wrapper surface being hard to validate automatically. |
| `src/ffi/apple_vz/entropy_device.rs` | Tiny and fine. |
| `src/ffi/apple_vz/memory_device.rs` | Tiny and fine. |
| `src/ffi/apple_vz/network_device.rs` | Reasonable wrappers. The bridged attachment path exists but is effectively not validated in tests and is still described elsewhere as broken, so this surface should be considered unstable. |
| `src/ffi/apple_vz/platform.rs` | Good addition. `from_data()`/`data_representation()` look sensible. No major issue. |
| `src/ffi/apple_vz/serial_port.rs` | The new file-based serial wrapper is incomplete because it ignores the NSError output path entirely. This should return `Result<Self, NSError>` instead of silently constructing even on failure. |
| `src/ffi/apple_vz/shared_directory.rs` | Overall good. The multiple-share dictionary construction is careful enough. No major issue. |
| `src/ffi/apple_vz/socket_device.rs` | Useful surface for future host-guest comms. The main gap is not in this file itself, but in the higher-level driver not exposing a complete safe vsock story yet. |
| `src/ffi/apple_vz/storage_device.rs` | Better than average builder/wrapper split. The main caveat is selector/version compatibility for newer constructors; if older macOS versions ever become supported, this will need explicit guards. |
| `src/ffi/apple_vz/virtual_machine.rs` | Core of the FFI safety story. `unsafe impl Send + Sync` is a big trust claim, so the driver must continue enforcing queue confinement. The wrapper surface is workable, but the driver currently wastes it by leaking objects and dropping failure reasons. |

### Initramfs support

| File | Review |
|---|---|
| `initramfs/build.sh` | Pragmatic build script. Fine for internal tooling, but this should not be treated as a hermetic or reproducible release path without pinning more of the toolchain and inputs. |
| `initramfs/default.nix` | Cleaner than the shell script in terms of reproducibility. No major issue, though it still inherits the security posture of `initramfs/init`. |
| `initramfs/init` | Security-sensitive and not release-ready as-is. Blank-password SSH fallback and `sh -c` workload execution are the main problems. The rest of the file is surprisingly capable, but it needs a strict security pass before it becomes the default story for end users. |

### Tests

| File | Review |
|---|---|
| `tests/disk_clone.rs` | Good coverage for basic clone semantics. Missing coverage for stderr/noise behavior and platform-specific command failures beyond "base missing". |
| `tests/network_switch.rs` | Good behavioral coverage. Main issue is quality-bar/CI: heavy `unwrap()` usage means strict clippy fails. |
| `tests/oci_pull.rs` | Useful smoke coverage, but not hermetic. These tests depend on real registries and network conditions. |
| `tests/seed_iso.rs` | Good basic file-creation coverage. Missing adversarial tests for shell metacharacters and path quoting, which is exactly where the current implementation is weak. |
| `tests/vm_lifecycle.rs` | Useful scenarios, but most important paths are opt-in and were skipped in the current run. This file currently gives more confidence than it should. |
| `tests/vm_manager.rs` | Strong logic coverage around the manager layer. Again, test ergonomics are fine, but strict clippy is not. |
| `tests/ffi_smoke.rs` | Useful macOS-only instantiation smoke tests. Coverage is breadth-first rather than correctness-focused, which is fine for FFI smoke tests. |

## Files With No Material Issue Beyond Normal Polish

- `src/ffi/mod.rs`
- `src/ffi/apple_vz/mod.rs`
- `src/ffi/apple_vz/entropy_device.rs`
- `src/ffi/apple_vz/memory_device.rs`
- `src/lib.rs`
- `src/network/mod.rs`
- `src/oci/mod.rs`
- `src/setup/mod.rs`
- `src/setup/ssh.rs`
- `LICENSE`
- `clippy.toml`

These still have normal cleanup opportunities, but nothing in them changed the overall risk profile.

## Validation Notes

### Test run

`cargo test --all-targets` passed.

Important limitation:
- the Apple/Cloud Hypervisor lifecycle coverage is conditional
- in the current environment, those tests passed without exercising a real guest boot path

### Clippy run

`cargo clippy --all-targets -- -D warnings` failed.

The failures are mostly test-only `unwrap()`/`unwrap_err()` usage in:
- `src/network/switch.rs`
- `src/oci/store.rs`
- `src/setup/image.rs`

That is not a runtime safety bug, but it does mean the repository is not clean under a strict reviewer or CI configuration.

## Recommended Fix Order

1. Redesign command handling in `src/setup/seed.rs` and `initramfs/init`.
2. Repair Apple VZ lifecycle correctness and remove leaked object/fd ownership.
3. Fix `prepare_image()` so diskless boot is a first-class path.
4. Add atomic verified downloads for all boot artifacts.
5. Tighten Linux resource ownership and VirtioFS sandbox defaults.
6. Fix the parser/FFI edges (`localhost`, missing digests, file serial `Result`).
7. Reconcile `CAPABILITIES.md` with the actual implementation and make real-driver CI mandatory before broad release.
