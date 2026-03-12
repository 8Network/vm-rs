# vm-rs Deep Review

Date: 2026-03-12

Scope:
- Reviewed the Rust implementation for VM lifecycle, driver behavior, networking, OCI image handling, setup helpers, and tests.
- Ran `cargo test --quiet` locally. Current test suite passed.

## Findings

### 1. Critical: OCI whiteout handling can escape the extraction root and delete host files

Files:
- `src/oci/store.rs:308-338`

Why this matters:
- Whiteout entries are handled before the code rejects `..` and absolute-path components.
- For entries such as `../.wh.ssh` or `../../etc/.wh.shadow`, `target.join(parent)` will walk outside the extraction root.
- In a VM/image pipeline, that turns a malicious layer into a host-side deletion primitive.

Details:
- The normal traversal check only happens later at `src/oci/store.rs:338-345`.
- The whiteout branch uses `target.join(parent)` immediately at `src/oci/store.rs:314` and `src/oci/store.rs:326`.
- The opaque whiteout path also uses `remove_dir_all` on every child at `src/oci/store.rs:318-320`, which fails to remove regular files and leaves stale lower-layer files behind.

Recommendation:
- Reject traversal and absolute paths before any whiteout processing.
- Canonicalize or component-validate the derived delete target before removing anything.
- Handle opaque whiteouts by deleting both files and directories.

### 2. High: Apple VZ lifecycle reporting is incorrect and `kill()` does not actually stop the VM

Files:
- `src/driver/apple_vz.rs:239-248`
- `src/driver/apple_vz.rs:289-329`

Why this matters:
- The driver leaks every `VZVirtualMachine` and uses registry presence as the source of truth for state.
- `stop()` only requests shutdown; it does not wait for shutdown completion and does not remove the VM from the registry.
- `kill()` only removes the registry entry. The leaked VM object remains alive, so the guest may keep running while the library reports it as stopped.

Details:
- Boot leaks the VM object permanently at `src/driver/apple_vz.rs:242`.
- `stop()` returns success without any state transition or cleanup at `src/driver/apple_vz.rs:289-306`.
- `state()` reports `Running` or `Starting` purely from registry membership plus the ready marker at `src/driver/apple_vz.rs:318-329`; it never asks Virtualization.framework whether the VM actually stopped.

Recommendation:
- Keep owned VM state in a real driver-owned structure instead of `Box::leak`.
- Observe Apple VZ stop/completion callbacks and remove the VM from tracking only after shutdown completes.
- Rename `kill()` if there is no force-stop primitive, or emulate it by terminating the hosting process instead of only hiding the handle.

### 3. High: `VmManager::start()` has a race that can boot the same VM name more than once

Files:
- `src/vm.rs:59-90`

Why this matters:
- The duplicate-name check is done under a read lock.
- The actual boot happens outside the lock.
- Tracking is written back only after boot completes.

Impact:
- Two concurrent callers can both pass the duplicate check, both boot separate VMs with the same logical name, and the second insert overwrites the first tracked handle.
- That leaves one VM orphaned from management and makes later stop/kill/state operations ambiguous.

Recommendation:
- Reserve the name under a write lock before booting.
- Store an intermediate `Starting` entry or use a state machine that prevents a second boot for the same name.

### 4. High: `NetworkSwitch` can use closed or reused file descriptors after `stop()` / drop

Files:
- `src/network/switch.rs:123-148`
- `src/network/switch.rs:151-172`

Why this matters:
- `start()` spawns a background forwarding thread but does not retain a `JoinHandle`.
- `stop()` only flips an atomic flag.
- `Drop` closes all port file descriptors immediately after setting the flag, without waiting for the thread to exit.

Impact:
- The forwarding thread can still be inside `poll()`, `recv()`, or `send()` when those descriptors are closed.
- Because file descriptor numbers are reusable, the thread can accidentally read from or write to unrelated resources opened later by the process.

Recommendation:
- Store the thread handle and join it during shutdown before closing ports.
- Consider an explicit wakeup fd/eventfd so shutdown is immediate instead of waiting on the poll timeout.

### 5. Medium-High: The persisted-handle lifecycle API is inconsistent on Linux and `kill_by_handle()` is unsafe

Files:
- `src/vm.rs:120-136`
- `src/driver/cloud_hv.rs:246-301`

Why this matters:
- `VmManager::stop_by_handle()` is documented for handles restored from persisted metadata.
- The Linux driver implementation of `stop()` only works for VMs still present in the in-memory map at `src/driver/cloud_hv.rs:247-252`.
- `kill()` falls back to `handle.pid` at `src/driver/cloud_hv.rs:280-285` and then sends `SIGKILL` to that raw PID.

Impact:
- `stop_by_handle()` does not fulfill its documented contract on Linux.
- `kill_by_handle()` can target an unrelated process if the stored PID has been reused.

Recommendation:
- Either remove the persisted-handle contract or implement it correctly.
- For Linux, track child ownership with `pidfd`, cgroup membership, or another verifiable identity instead of trusting stale PIDs.

### 6. Medium: Large image downloads are fully buffered in memory

Files:
- `src/setup/image.rs:327-352`

Why this matters:
- `download_file()` calls `resp.bytes().await` and only then writes to disk.
- VM images are large enough that this can create avoidable RAM spikes or OOMs under parallel downloads.

Recommendation:
- Stream downloads directly to a temp file with `bytes_stream()` and `tokio::fs::File`.
- Finalize with an atomic rename once the download completes.

## Design And Rust Practice Notes

What is already in good shape:
- Error types are explicit and readable.
- Unsafe blocks usually include a reason, which is good practice for Rust FFI and `libc` calls.
- The crate is small and the module boundaries are easy to follow.
- Tests cover a decent amount of behavior for the current size of the project.

Main design themes to improve:
- Lifecycle ownership is too implicit in several places. VM and network workers should have explicit ownership and shutdown paths.
- PID- and FD-based control paths need stronger identity guarantees. In critical systems, raw numeric handles are not enough.
- External data handling is the weakest area. OCI extraction and artifact downloads need stricter correctness and resource controls.

## Testing Gaps

Important gaps in the current suite:
- No test exercises malicious OCI whiteout entries or validates opaque whiteout semantics.
- No concurrency test for duplicate `VmManager::start()` calls.
- No shutdown-race test for `NetworkSwitch`.
- No Apple VZ state-transition test that proves `stop()` and `kill()` are truthful.
- No Linux test for `stop_by_handle()` / `kill_by_handle()` behavior after process restart.

## Suggested Fix Order

1. Fix OCI extraction safety first.
2. Fix Apple VZ lifecycle/state correctness.
3. Make `VmManager::start()` atomic for a VM name.
4. Make `NetworkSwitch` shutdown join the worker thread.
5. Redesign persisted-handle behavior around verified process identity.
6. Stream downloads instead of buffering them.
