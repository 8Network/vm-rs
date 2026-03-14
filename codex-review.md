# Codex Review

Date: 2026-03-14

Scope:
- Static review of the core abstraction, platform drivers, FFI boundary, setup, OCI, and networking modules under `src/`
- Build verification with `cargo check`
- Test/build verification with `cargo test --no-run`

Verification summary:
- `cargo check` succeeds.
- `cargo test --no-run` fails with compile errors in the shipped test suite, especially [`tests/vm_manager.rs`](/Users/soheilalizadeh/vm-rs/tests/vm_manager.rs) and [`tests/ffi_smoke.rs`](/Users/soheilalizadeh/vm-rs/tests/ffi_smoke.rs).

## Findings

### 1. Critical: the published API, FFI surface, and test suite are materially out of sync

This repository does not currently validate the module it claims to ship. The driver trait has only `boot/stop/kill/state`, but the test suite still expects `pause/resume` and extra config/handle fields such as `vsock`, `machine_id`, and `efi_variable_store` ([`src/driver/mod.rs#L20`](/Users/soheilalizadeh/vm-rs/src/driver/mod.rs#L20), [`tests/vm_manager.rs#L49`](/Users/soheilalizadeh/vm-rs/tests/vm_manager.rs#L49), [`tests/vm_manager.rs#L162`](/Users/soheilalizadeh/vm-rs/tests/vm_manager.rs#L162)). The Apple FFI tests also import modules and types that are not exported or no longer exist, for example `ffi::apple_vz::platform` is referenced by tests but not re-exported from the module root ([`src/ffi/apple_vz/mod.rs#L18`](/Users/soheilalizadeh/vm-rs/src/ffi/apple_vz/mod.rs#L18), [`tests/ffi_smoke.rs#L18`](/Users/soheilalizadeh/vm-rs/tests/ffi_smoke.rs#L18)).

Impact:
- CI cannot prove the crate still matches its own documented/tested behavior.
- Consumers cannot trust examples, tests, or type names to reflect the real public surface.
- This is a correctness and maintenance failure before runtime behavior is even considered.

### 2. High: Apple VZ leaks file descriptors at the FFI boundary

`NSFileHandle::file_handle_with_fd` creates Objective-C file handles with `closeOnDealloc: NO` ([`src/ffi/apple_vz/base.rs#L168`](/Users/soheilalizadeh/vm-rs/src/ffi/apple_vz/base.rs#L168)). The Apple driver then hands ownership away with `into_raw_fd()` for `/dev/null`, serial logs, and socket-backed NICs ([`src/driver/apple_vz.rs#L138`](/Users/soheilalizadeh/vm-rs/src/driver/apple_vz.rs#L138), [`src/driver/apple_vz.rs#L166`](/Users/soheilalizadeh/vm-rs/src/driver/apple_vz.rs#L166)). After `into_raw_fd()`, Rust no longer owns those descriptors, and the ObjC side is explicitly told not to close them either.

Impact:
- Long-lived processes can steadily leak FDs per VM start.
- Repeated VM lifecycle operations can eventually fail with `EMFILE`/resource exhaustion.
- This is a concrete unsafe-resource-management bug, not just a style issue.

### 3. High: Apple VZ reports success before it knows whether startup actually succeeded

The Apple driver inserts the VM into its registry before start, dispatches an async start block, logs any failure inside the completion callback, sleeps 500ms, and then returns `Ok(VmHandle { state: Starting, .. })` regardless of the callback result ([`src/driver/apple_vz.rs#L268`](/Users/soheilalizadeh/vm-rs/src/driver/apple_vz.rs#L268), [`src/driver/apple_vz.rs#L309`](/Users/soheilalizadeh/vm-rs/src/driver/apple_vz.rs#L309), [`src/vm.rs#L97`](/Users/soheilalizadeh/vm-rs/src/vm.rs#L97)).

Impact:
- `VmManager::start()` can report success even when entitlement errors or framework start failures already happened.
- The failure is demoted to logging instead of being surfaced synchronously to the caller.
- This creates a silent-fallback style UX problem: callers think the VM booted when it never did.

### 4. High: `VmManager::start` can leave a permanent ghost reservation on filesystem errors

`VmManager::start()` reserves the VM name in the in-memory map before creating the VM directory ([`src/vm.rs#L60`](/Users/soheilalizadeh/vm-rs/src/vm.rs#L60)). If `create_dir_all` fails, the function returns early without removing the `Starting` placeholder ([`src/vm.rs#L86`](/Users/soheilalizadeh/vm-rs/src/vm.rs#L86)). Cleanup only exists for driver boot failures, not for earlier I/O failures ([`src/vm.rs#L97`](/Users/soheilalizadeh/vm-rs/src/vm.rs#L97)).

Impact:
- One transient disk/permission error can make that VM name unusable until the manager process restarts.
- The user sees a misleading "already exists in state: starting" on retry.
- This is a correctness and UX bug in the abstraction layer itself.

### 5. High: stopping a restored Cloud Hypervisor VM can return success while the VM is still running

For restored handles, `CloudHvDriver::stop()` sends `SIGTERM`, waits with `wait_for_pid_exit`, and then returns `Ok(())` unconditionally ([`src/driver/cloud_hv.rs#L265`](/Users/soheilalizadeh/vm-rs/src/driver/cloud_hv.rs#L265)). But `wait_for_pid_exit()` only logs a warning on timeout and does not propagate failure ([`src/driver/cloud_hv.rs#L567`](/Users/soheilalizadeh/vm-rs/src/driver/cloud_hv.rs#L567)).

Impact:
- A caller may believe a restored VM shut down cleanly when the VMM process is still alive.
- Follow-up operations can race a still-running guest and corrupt operator state.
- This is especially dangerous because restored-handle flows are explicitly for daemon restarts and recovery.

### 6. Medium: the userspace switch can die permanently while `start()` keeps claiming it is running

The forwarding loop exits if the network lock is poisoned ([`src/network/switch.rs#L260`](/Users/soheilalizadeh/vm-rs/src/network/switch.rs#L260)), but it never clears the `running` flag. `start()` only checks that atomic flag and returns `Ok(())` if it is already set ([`src/network/switch.rs#L126`](/Users/soheilalizadeh/vm-rs/src/network/switch.rs#L126)). That means the switch thread can disappear permanently while the object still reports itself as started.

Impact:
- Inter-VM networking can silently blackhole traffic after an internal failure.
- The only panic detection happens in `stop()`, not when the failure occurs.
- This is both a reliability and observability gap.

### 7. Medium: OCI opaque whiteout handling is incorrect and can leave deleted files behind

When applying an opaque whiteout (`.wh..wh..opq`), the extractor iterates the target directory and calls `remove_dir_all` on every child ([`src/oci/store.rs#L314`](/Users/soheilalizadeh/vm-rs/src/oci/store.rs#L314)). That only works for directories. Regular files fail removal, are merely warned about, and remain in the merged rootfs.

Impact:
- Layer application can produce the wrong filesystem contents.
- Container images that rely on opaque whiteouts for correctness can boot with stale files present.
- This is a correctness bug in image materialization.

### 8. Medium: image and OCI downloads are unbounded in time and memory

The image prep client uses `reqwest::Client::new()` with no timeout ([`src/setup/image.rs#L167`](/Users/soheilalizadeh/vm-rs/src/setup/image.rs#L167)), and the OCI client also builds a client without any timeout configuration ([`src/oci/registry.rs#L84`](/Users/soheilalizadeh/vm-rs/src/oci/registry.rs#L84)). Both paths then read full responses into memory with `bytes()`/`text()` before writing them to disk or processing them ([`src/setup/image.rs#L276`](/Users/soheilalizadeh/vm-rs/src/setup/image.rs#L276), [`src/setup/image.rs#L339`](/Users/soheilalizadeh/vm-rs/src/setup/image.rs#L339), [`src/oci/registry.rs#L294`](/Users/soheilalizadeh/vm-rs/src/oci/registry.rs#L294)).

Impact:
- Large VM images and OCI layers can cause avoidable memory spikes.
- Slow or hung registries can stall operations indefinitely.
- For an infrastructure library, this is poor reliability and poor user experience.

### 9. Medium: Windows support exists as code but is unreachable from the actual product surface

The repository contains a WHP driver implementation ([`src/driver/whp.rs#L1`](/Users/soheilalizadeh/vm-rs/src/driver/whp.rs#L1)), but the driver module only exposes macOS and Linux backends ([`src/driver/mod.rs#L7`](/Users/soheilalizadeh/vm-rs/src/driver/mod.rs#L7)), and `create_platform_driver()` rejects every non-macOS/non-Linux target as unsupported ([`src/vm.rs#L310`](/Users/soheilalizadeh/vm-rs/src/vm.rs#L310)).

Impact:
- The codebase advertises more platform ambition than the shipped API actually supports.
- Windows support is effectively dead code from a consumer perspective.
- For a "create VMs on any OS" abstraction, this is a product-level correctness gap.

## Additional Notes

- Logging is inconsistent in library code. `NSError::dump()` prints directly with `println!` ([`src/ffi/apple_vz/base.rs#L252`](/Users/soheilalizadeh/vm-rs/src/ffi/apple_vz/base.rs#L252)), and `NetworkSwitch::drop()` uses `eprintln!` instead of structured tracing ([`src/network/switch.rs#L178`](/Users/soheilalizadeh/vm-rs/src/network/switch.rs#L178)). That bypasses the caller's logging pipeline.
- The Linux bridge helper mutates global host networking policy by forcing `/proc/sys/net/ipv4/ip_forward = 1` and adding iptables NAT rules ([`src/network/bridge.rs#L40`](/Users/soheilalizadeh/vm-rs/src/network/bridge.rs#L40), [`src/network/bridge.rs#L111`](/Users/soheilalizadeh/vm-rs/src/network/bridge.rs#L111)), but deletion does not roll those changes back. The abstraction leaks host-global side effects.
- `unsafe impl Send` and `unsafe impl Sync` for `VZVirtualMachine` rely on a very thin justification comment ([`src/ffi/apple_vz/virtual_machine.rs#L234`](/Users/soheilalizadeh/vm-rs/src/ffi/apple_vz/virtual_machine.rs#L234)). For ObjC framework objects, that deserves a much stronger proof or a stricter thread-affinity wrapper.

## Recommended Order Of Attack

1. Make `cargo test --no-run` green or delete/replace stale tests so the repo can validate itself again.
2. Fix the Apple FD ownership bug and the async-start success reporting path.
3. Fix lifecycle correctness in `VmManager::start()` and restored-handle `stop()`.
4. Harden long-running helpers: switch liveness, streamed downloads, and OCI whiteout application.
5. Decide whether Windows support is in or out, then align the code, docs, and module exports.
