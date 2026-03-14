# vm-rs Combined Review & Remediation Plan

Combined findings from Codex review (2026-03-13), Claude review (2026-03-14), and
cross-review commentary. Deduplicated, severity-normalized, and organized by
category.

**Finding sources:**
- **CX** = Codex REVIEW.md (historical, most items remediated in prior pass)
- **CL** = Claude CLAUDE-REVIEW.md
- **CX2** = Codex comments on Claude review (2026-03-14)

Findings marked **(fixed)** were remediated during the Codex review pass.
All remaining findings are **open** in the current tree.

---

## 1. Lifecycle Correctness

These are the highest-impact findings. A VM lifecycle library that silently reports
success on failure, or diverges its internal state from reality, is fundamentally
broken for its users.

### 1.1 Apple VZ `boot()` returns `Ok` on async startup failure [HIGH]

**Source:** CX2 #3 (new finding), CL #44 (related sleep issue)

`boot()` dispatches VM startup to a GCD queue via `dispatch_async`, then sleeps
500ms and returns `Ok(VmHandle)`. The completion handler logs failure via
`tracing::error!` but has no channel back to the caller. Result: `boot()` can
return `Ok` for a VM that failed to start.

**Files:** `src/driver/apple_vz.rs:281-310`

**Impact:** Callers track a VM that never booted. `state()` eventually catches up,
but there is a window of silent state divergence.

**Fix:** Use a `std::sync::mpsc::channel` or `Condvar` in the completion handler.
`boot()` blocks on the receiver instead of sleeping. Returns `Err` if the
completion handler reports failure.

---

### 1.2 `VmManager::start()` ghost reservation on `create_dir_all` failure [MEDIUM]

**Source:** CX2 #4

The name is reserved (line 74) before `create_dir_all` (line 88). If directory
creation fails, the stale `Starting` placeholder is never removed. Subsequent
retries are blocked permanently (until process restart).

**Files:** `src/vm.rs:59-88`

**Fix:** Wrap the post-reservation code in a cleanup guard:
```rust
let handle = match self.driver.boot(config) { ... };
// On Err, already cleaned up at line 104
```
Move `create_dir_all` before the reservation, or add cleanup on its error path.

---

### 1.3 Restored-handle `stop()` returns `Ok` while process is alive [HIGH]

**Source:** CL #20, CX2 #5 (severity upgrade)

`cloud_hv::stop()` for restored handles calls `wait_for_pid_exit()`, which only
logs on timeout and returns. The caller returns `Ok(())`. The manager marks the VM
`Stopped` while the process may still be running.

**Files:** `src/driver/cloud_hv.rs:265-280`, `src/driver/cloud_hv.rs:567-577`

**Fix:** `wait_for_pid_exit` must escalate to SIGKILL on timeout and return an error
if the process is still alive after escalation. Unify with `wait_for_exit` which
already does this correctly.

---

### 1.4 `state()` removes VMs from registry as a side effect [MEDIUM]

**Source:** CL #18

Apple VZ `state()` removes stopped/failed VMs from the registry. This violates
Command-Query Separation and races under concurrent callers.

**Files:** `src/driver/apple_vz.rs:393-395`

**Fix:** Remove the `registry.remove()` from `state()`. Only `stop()`/`kill()`
should modify the registry.

---

### 1.5 Network switch liveness divergence [MEDIUM]

**Source:** CX2 #6

The forwarding thread exits on lock poison (`break` at line 262) but `running`
stays `true`. Subsequent `start()` calls return `Ok(())` without spawning a thread.
The switch appears running but forwards nothing.

**Files:** `src/network/switch.rs:260-262`, `src/network/switch.rs:129-131`

**Fix:** Set `running.store(false, ...)` at the end of `forwarding_loop`, or before
each `break`. Alternatively, check if the worker thread handle is still alive.

---

### 1.6 `clone_disk` silently succeeds on corrupt existing files [MEDIUM]

**Source:** CL #11

Returns `Ok(())` if target exists, regardless of integrity. A partial file from a
crashed clone is silently used.

**Files:** `src/vm.rs:249-252`

**Fix:** Compare file sizes with base image. Or remove skip-if-exists and always
clone (caller can check themselves).

---

### 1.7 Previously fixed lifecycle issues [FIXED]

- Apple VZ `kill()` not terminating VMs — **(fixed)** CX High #1
- Apple VZ state tracking incorrect after shutdown — **(fixed)** CX High #2
- Linux restored-handle lifecycle unsafe — **(fixed)** CX High #3
- Linux zombie misclassification — **(fixed)** CX High #4
- Duplicate-name race in `VmManager::start()` — **(fixed)** CX Medium #11

---

## 2. Security

### 2.1 VM name path traversal [HIGH]

**Source:** CL #9

`vm_dir()` joins user-supplied `name` into `base_dir` without validation. A name
like `"../../etc"` escapes the base directory. Used for VM directories, socket
paths, serial logs, CString labels.

**Files:** `src/vm.rs:54-56`

**Fix:** Validate name matches `^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$` in a
`VmConfig::validate()` method called at the top of `VmManager::start()`.

---

### 2.2 No `VmConfig` validation at manager level [HIGH]

**Source:** CL #8

No validation for: empty name, zero CPUs, zero memory, empty kernel path. Each
driver rediscovers these inconsistently or not at all.

**Files:** `src/vm.rs:59`

**Fix:** Add `VmConfig::validate() -> Result<(), VmError>` checking all invariants.
Call it as the first operation in `VmManager::start()`.

---

### 2.3 Bridge interface name not validated [MEDIUM]

**Source:** CL #21

`ensure_bridge(name, ...)` passes unvalidated strings to `ip` and `iptables`
commands.

**Files:** `src/network/bridge.rs`

**Fix:** Validate `name` matches `^[a-zA-Z0-9_-]{1,15}$`.

---

### 2.4 OCI opaque whiteout doesn't remove regular files [MEDIUM]

**Source:** CX High #6, CX2 #7

Opaque whiteout uses `remove_dir_all()` on each child, which may fail on regular
files (platform-dependent). Stale files produce an incorrect merged rootfs.

**Files:** `src/oci/store.rs:314-324`

**Fix:** Check `file_type()` and dispatch to `remove_file` for files,
`remove_dir_all` for directories.

---

### 2.5 Previously fixed security issues [FIXED]

- `virtiofsd` launched with `--sandbox=none` — **(fixed)** CX High #5
- Port forwarding binds 0.0.0.0 by default — **(fixed)** CX Medium #7
- Cloud-init injection surfaces — **(fixed)** CX Medium #8
- Image downloads not verified — **(fixed)** CX Medium #10
- Raw PID exposure in public API — **(fixed)** CX Unsafe Surfaces #3
- Raw FD in `NetworkAttachment` — **(fixed)** CX Unsafe Surfaces #1-2

---

## 3. Resource Management

### 3.1 Non-atomic file writes (crash corruption) [HIGH]

**Source:** CL #6

`download_file` (`setup/image.rs:303`) and `put_blob` (`oci/store.rs:114`) write
directly to the target path. A crash mid-write leaves a corrupt file that passes
existence checks on restart.

**Files:** `src/setup/image.rs:303`, `src/oci/store.rs:114`

**Fix:** Write to temp file in same directory, `fsync`, atomic `rename`.

---

### 3.2 `SwitchPort` FD leak on error paths [HIGH]

**Source:** CL #5

`create_socketpair()` returns raw FDs. If the lock acquisition fails before
insertion, the switch-side FD leaks. `SwitchPort` stores a bare `RawFd` with no
RAII.

**Files:** `src/network/switch.rs:96-120`

**Fix:** Wrap both FDs in `OwnedFd` immediately. Store `OwnedFd` in `SwitchPort`.

---

### 3.3 `NSFileHandle` FD leak (`closeOnDealloc: NO`) [LOW]

**Source:** CL #30, CX Unsafe Surfaces #4

FDs transferred via `into_raw_fd()` but NSFileHandle told not to close on dealloc.
FDs leak when the ObjC object is deallocated.

**Files:** `src/ffi/apple_vz/base.rs:171`

**Fix:** Pass `YES` for `closeOnDealloc`.

---

## 4. Correctness

### 4.1 WHP driver is dead/unwired code + test suite broken [HIGH]

**Source:** CL #1-4, CX2 #1-2

`whp.rs` and `boot.rs` exist as source files but are not declared as modules in
`driver/mod.rs`. `tests/vm_manager.rs` references WHP types (`VmState::Paused`,
wrong `VmHandle` fields, `pause`/`resume` methods), causing `cargo test --no-run`
to fail with 40 errors. The library itself compiles fine — the breakage is confined
to tests.

**Files:** `src/driver/mod.rs`, `src/driver/whp.rs`, `src/driver/boot.rs`,
`tests/vm_manager.rs`, `Cargo.toml`

**Fix (two options):**
- **Option A (wire it in):** Add module declarations, Cargo dependencies, platform
  factory entry, fix `VmHandle` field names, add `VmState::Paused`, extend
  `VmDriver` trait or use separate impl block for `pause`/`resume`.
- **Option B (defer):** Remove WHP references from `tests/vm_manager.rs` so tests
  compile. Keep `whp.rs`/`boot.rs` as dormant source for future integration.

---

### 4.2 Page tables only support 1 GB of guest memory [HIGH]

**Source:** CL #13

`setup_page_tables` creates a single PD with max 512 entries (1 GB). VMs with
`memory_mb > 1024` will crash on memory access above 1 GB.

**Files:** `src/driver/boot.rs:76`

**Fix:** Support multiple PD tables (one per PDPT entry). Or validate `memory_mb <=
1024` with a clear error explaining the current limit.

---

### 4.3 `DefaultHasher` produces unstable MAC addresses [HIGH]

**Source:** CL #12

`generate_mac()` uses `std::collections::hash_map::DefaultHasher`, which is
explicitly not stable across Rust versions. A toolchain update silently changes
MAC addresses, breaking networking and DHCP leases.

**Files:** `src/driver/cloud_hv.rs:443-456`

**Fix:** Use SHA-256 truncated to 3 bytes, or a fixed-seed SipHash. The result must
be deterministic and stable across compiler versions.

---

### 4.4 Integer overflow on `memory_mb * 1024 * 1024` [MEDIUM]

**Source:** CL #7

On 32-bit targets, `usize` overflows for `memory_mb >= 4096`. No validation that
values are non-zero.

**Files:** `src/driver/apple_vz.rs:96`, `src/driver/whp.rs:182`

**Fix:** Use `u64` for byte calculations. Validate `memory_mb > 0` and `cpus > 0`
in `VmConfig::validate()`.

---

### 4.5 `unsanitize_name` corrupts names with underscores [MEDIUM]

**Source:** CL #25

Fallback heuristic turns `"my_image"` into `"my/image"`.

**Files:** `src/oci/store.rs:375-381`

**Fix:** Remove the fallback heuristic. Only unsanitize via `_slash_`/`_colon_`.

---

### 4.6 Unbounded `serial_buffer` in WHP vCPU loop [MEDIUM]

**Source:** CL #23

Accumulates all serial output forever. Unbounded memory growth.

**Files:** `src/driver/whp.rs:687`

**Fix:** Stop accumulating after `VMRS_READY` is found, or use a ring buffer.

---

### 4.7 Duplicated `check_ready_marker` + O(n) re-read [MEDIUM]

**Source:** CL #10, CL #14

Identical function in both drivers reads entire serial log on every `state()` poll.

**Files:** `src/driver/apple_vz.rs:422-438`, `src/driver/cloud_hv.rs:519-536`

**Fix:** Extract to `driver/mod.rs`. Use incremental reading (track last offset).

---

## 5. Reliability

### 5.1 No HTTP timeouts [MEDIUM]

**Source:** CL #19

HTTP clients in `setup/image.rs` and `oci/registry.rs` have no timeouts. Downloads
can hang forever.

**Files:** `src/setup/image.rs:167`, `src/oci/registry.rs:84-87`

**Fix:** Set `connect_timeout(30s)` and `timeout(300s)` on both clients.

---

### 5.2 `wait_all_ready` blocks the thread [MEDIUM]

**Source:** CL #16

Uses `thread::sleep(1s)` in a loop. Blocks tokio runtime if called from async code.

**Files:** `src/vm.rs:196-242`

**Fix:** Add `async fn wait_all_ready_async()` using `tokio::time::sleep`. Add doc
comment on the sync version warning about blocking.

---

### 5.3 Seed ISO temp dir collision [LOW]

**Source:** CL #32

Two concurrent processes creating seed ISOs for the same hostname clobber each
other.

**Files:** `src/setup/seed.rs:95`

**Fix:** Use `tempfile::tempdir_in()`.

---

## 6. Observability

### 6.1 No manager-level logging for lifecycle operations [MEDIUM]

**Source:** CL #39-41, CX Missing Logging #1-2

`stop()`, `kill()`, `stop_by_handle()`, `kill_by_handle()` have no logging at the
manager layer. Boot failure cleanup is silent.

**Files:** `src/vm.rs`

**Fix:** Add `tracing::info!` for stop/kill at the manager level. Add
`tracing::warn!` for boot failure cleanup.

---

### 6.2 `wait_all_ready` doesn't log progress [LOW]

**Source:** CL #40

Multi-minute wait with no periodic status update.

**Files:** `src/vm.rs:196-242`

**Fix:** Log pending VMs every 10 seconds.

---

### 6.3 Logging inconsistency (structured vs format strings) [LOW]

**Source:** CL #29

Driver modules use structured fields; OCI/setup modules use format strings.

**Fix:** Standardize on structured tracing fields throughout.

---

## 7. Error Handling & Types

### 7.1 No `From` conversions between error types [MEDIUM]

**Source:** CL #22

`OciError` and `SetupError` can't convert to `VmError`. Awkward to compose
operations.

**Fix:** Add `From<OciError> for VmError` and `From<SetupError> for VmError`.

---

### 7.2 Stringly-typed error enums [LOW]

**Source:** CX Weak Error Handling #1, CL #22

`VmError`, `SetupError`, `OciError` use `String` detail fields. Weakens matching.

**Fix:** Long-term, add typed sub-variants. Short-term, accept as pragmatic.

---

### 7.3 `VmState` missing `Eq` derive [LOW]

**Source:** CL #26

**Fix:** Add `Eq` to the derive list.

---

### 7.4 Missing `#[must_use]` on key types [LOW]

**Source:** CL #31

`VmHandle`, `VmState`, `PreparedImage`, `ImageManifest`.

**Fix:** Add `#[must_use]` attribute.

---

## 8. Platform & Cross-Compilation

### 8.1 `config.rs` Unix-only types not cfg-gated [MEDIUM]

**Source:** CL #24

`VmSocketEndpoint` uses `std::os::fd::*` which doesn't exist on Windows.

**Files:** `src/config.rs:3`

**Fix:** Gate behind `#[cfg(unix)]`.

---

### 8.2 Network modules not platform-gated [LOW]

**Source:** CL #38

`switch` (macOS) and `bridge` (Linux) compiled on all platforms.

**Fix:** Gate with `#[cfg(target_os = "...")]`.

---

### 8.3 `clone_disk` no-op on unsupported platforms [LOW]

**Source:** CL #36

Silently returns `Ok(())` without doing anything.

**Fix:** Add fallback `std::fs::copy` or return error.

---

## 9. Design Debt

### 9.1 `VmDriver` trait is sync in an async-heavy crate [DESIGN]

**Source:** CL #42

Trait methods block. Mixed with tokio throughout.

**Action:** Document blocking nature. Consider `async_trait` in v0.2.

---

### 9.2 `ImageStore` blocks tokio from async context [DESIGN]

**Source:** CL #43

Sync filesystem I/O called from `async fn pull()`.

**Fix:** Wrap heavy I/O in `tokio::task::spawn_blocking`.

---

### 9.3 Lock poisoning boilerplate [DESIGN]

**Source:** CL #45

Same `.map_err(|e| VmError::Hypervisor(format!("lock poisoned: {}", e)))` pattern
15+ times.

**Fix:** Helper trait:
```rust
trait PoisonRecover<T> {
    fn or_poison(self) -> Result<T, VmError>;
}
```

---

### 9.4 Apple FFI constructors don't return `Result` [DESIGN]

**Source:** CX Unsafe Surfaces #5

ObjC `alloc`/`init` can return nil but wrappers don't check.

**Action:** Long-term, add nil checks and return `Option` or `Result`.

---

## 10. Testing & CI

### 10.1 Test suite doesn't compile [BLOCKING]

**Source:** CX2 #1

`cargo test --no-run` fails with 40 errors in `tests/vm_manager.rs`.

**Fix:** Remove or gate WHP-specific test code.

---

### 10.2 Tests silently self-skip [MEDIUM]

**Source:** CX Test Analysis #2

`vm_lifecycle`, `seed_iso`, `oci_pull` return early on missing assets. Green CI can
mean "not exercised".

**Status:** Partially addressed (workflow-dispatch gating added per CX remediation).

---

### 10.3 No adversarial input testing [MEDIUM]

**Source:** CX Test Analysis #7

Only partial coverage for env-name rejection. No tests for hostile process commands,
health checks, volume tags, NIC config, SSH keys, OCI whiteouts.

**Fix:** Add fuzz-style unit tests for `seed.rs` validators and `store.rs`
extraction.

---

---

# Remediation Plan

## Phase 0: Unblock CI (1 PR)

**Goal:** `cargo test` compiles and passes.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 0.1 | Remove or `#[cfg(windows)]`-gate WHP references in `tests/vm_manager.rs` | `tests/vm_manager.rs` | 10.1, 4.1 |
| 0.2 | Verify `cargo test` and `cargo clippy` pass | CI | — |

**Acceptance:** `cargo test --no-run` succeeds on macOS and Linux.

---

## Phase 1: Silent Success & State Divergence (1-2 PRs)

**Goal:** The library never returns `Ok` for an operation that failed.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 1.1 | Apple VZ: replace `sleep(500ms)` with `mpsc::channel` in completion handler; `boot()` blocks on result, returns `Err` on failure | `src/driver/apple_vz.rs` | 1.1 |
| 1.2 | Fix ghost reservation: clean up placeholder if `create_dir_all` fails | `src/vm.rs` | 1.2 |
| 1.3 | `wait_for_pid_exit`: escalate to SIGKILL on timeout, return `Err` if still alive | `src/driver/cloud_hv.rs` | 1.3 |
| 1.4 | Remove `registry.remove()` from `state()` in Apple VZ driver | `src/driver/apple_vz.rs` | 1.4 |
| 1.5 | Switch: set `running = false` when forwarding loop exits | `src/network/switch.rs` | 1.5 |
| 1.6 | `clone_disk`: validate target file size against base before skipping | `src/vm.rs` | 1.6 |

**Acceptance:** No API path returns `Ok` when the underlying operation failed.
Add regression tests for 1.1-1.3.

---

## Phase 2: Security Hardening (1 PR)

**Goal:** User-supplied strings cannot escape their intended scope.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 2.1 | Add `VmConfig::validate()` — name regex, non-zero cpus/memory, non-empty kernel | `src/config.rs`, `src/vm.rs` | 2.1, 2.2 |
| 2.2 | Validate bridge interface names | `src/network/bridge.rs` | 2.3 |
| 2.3 | Fix OCI opaque whiteout to use `remove_file` for files | `src/oci/store.rs` | 2.4 |
| 2.4 | Fix `unsanitize_name` heuristic | `src/oci/store.rs` | 4.5 |

**Acceptance:** Unit tests for path-traversal names rejected, whiteout on files,
`unsanitize_name("my_image") == "my_image"`.

---

## Phase 3: Crash Safety & Resource Management (1 PR)

**Goal:** No corrupt state after a crash. No FD leaks.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 3.1 | Atomic writes: temp file + fsync + rename in `download_file` and `put_blob` | `src/setup/image.rs`, `src/oci/store.rs` | 3.1 |
| 3.2 | `SwitchPort`: store `OwnedFd` instead of `RawFd` | `src/network/switch.rs` | 3.2 |
| 3.3 | `NSFileHandle`: pass `closeOnDealloc: YES` | `src/ffi/apple_vz/base.rs` | 3.3 |
| 3.4 | Add HTTP timeouts to both clients | `src/setup/image.rs`, `src/oci/registry.rs` | 5.1 |

**Acceptance:** Interrupted download leaves no partial file. Switch FDs cleaned up
on all paths.

---

## Phase 4: Correctness (1-2 PRs)

**Goal:** Boot protocol and MAC generation are correct.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 4.1 | Replace `DefaultHasher` with SHA-256-truncated MAC generation | `src/driver/cloud_hv.rs` | 4.3 |
| 4.2 | Extend page tables to support >1GB (multiple PDs via PDPT entries), or validate `memory_mb <= 1024` | `src/driver/boot.rs` | 4.2 |
| 4.3 | Use `u64` for memory byte calculations | `src/driver/apple_vz.rs`, `src/driver/whp.rs` | 4.4 |
| 4.4 | Cap or ring-buffer `serial_buffer` in WHP | `src/driver/whp.rs` | 4.6 |
| 4.5 | Extract shared `check_ready_marker` to `driver/mod.rs`, add incremental reading | `src/driver/mod.rs`, `src/driver/apple_vz.rs`, `src/driver/cloud_hv.rs` | 4.7 |

**Acceptance:** VMs with 2GB+ RAM boot on WHP. MAC addresses stable across rustc
versions (add test). Serial log polling is O(delta) not O(n).

---

## Phase 5: Observability & Ergonomics (1 PR)

**Goal:** Operational visibility and developer ergonomics.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 5.1 | Add manager-level logging for stop/kill/boot-failure | `src/vm.rs` | 6.1 |
| 5.2 | Log progress in `wait_all_ready` every 10s | `src/vm.rs` | 6.2 |
| 5.3 | Standardize structured tracing fields in OCI/setup modules | `src/oci/*.rs`, `src/setup/*.rs` | 6.3 |
| 5.4 | Add `From<OciError>` and `From<SetupError>` for `VmError` | `src/driver/mod.rs` | 7.1 |
| 5.5 | Add `Eq` derive to `VmState` | `src/config.rs` | 7.3 |
| 5.6 | Add `#[must_use]` to key types | `src/config.rs`, `src/oci/store.rs` | 7.4 |
| 5.7 | Lock poisoning helper trait | `src/driver/mod.rs` or utility module | 9.3 |
| 5.8 | Add `async fn wait_all_ready_async` + doc warning on sync version | `src/vm.rs` | 5.2 |

**Acceptance:** Operational logs cover full lifecycle. Error types compose cleanly.

---

## Phase 6: Platform Gating & WHP Integration (1-2 PRs)

**Goal:** Clean cross-platform compilation and Windows support foundation.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 6.1 | Gate `VmSocketEndpoint` and FD imports behind `#[cfg(unix)]` | `src/config.rs` | 8.1 |
| 6.2 | Gate `network::switch` behind `#[cfg(target_os = "macos")]`, `network::bridge` behind `#[cfg(target_os = "linux")]` | `src/network/mod.rs` | 8.2 |
| 6.3 | `clone_disk`: add `#[cfg(not(...))]` fallback with `std::fs::copy` | `src/vm.rs` | 8.3 |
| 6.4 | Wire WHP: add `pub mod boot;`, `#[cfg(windows)] pub mod whp;`, fix `VmHandle` fields, add `VmState::Paused`, add `windows` crate dep, update `create_platform_driver()` | Multiple | 4.1 |

**Acceptance:** `cargo check --target x86_64-pc-windows-msvc` succeeds (with
`--no-default-features` if needed). macOS and Linux unaffected.

---

## Phase 7: Testing & CI Hardening (1 PR)

**Goal:** Tests exercise the code that matters, CI catches regressions.

| # | Task | Files | Finding |
|---|------|-------|---------|
| 7.1 | Add unit tests for `VmConfig::validate()` — path traversal, empty name, zero resources | `src/config.rs` | 2.1, 2.2 |
| 7.2 | Add unit tests for `unsanitize_name` edge cases | `src/oci/store.rs` | 4.5 |
| 7.3 | Add unit test for OCI opaque whiteout on files | `src/oci/store.rs` | 2.4 |
| 7.4 | Add adversarial input tests for `seed.rs` — hostile commands, mount points, SSH keys | `src/setup/seed.rs` | 10.3 |
| 7.5 | Add MAC generation stability test (hash known input, assert fixed output) | `src/driver/cloud_hv.rs` | 4.3 |
| 7.6 | Add `cargo-deny` or `cargo-audit` step to CI | `.github/workflows/ci.yml` | CX Workflow #5 |

**Acceptance:** `cargo test` exercises validation, whiteout, MAC stability, and
hostile seed inputs.

---

## Summary

| Phase | PRs | Findings addressed | Risk reduced |
|-------|-----|--------------------|--------------|
| **0** | 1 | 1 | CI unblocked |
| **1** | 1-2 | 6 | Silent success / state divergence |
| **2** | 1 | 4 | Path traversal, injection, data corruption |
| **3** | 1 | 4 | Crash safety, FD leaks, hangs |
| **4** | 1-2 | 5 | Boot protocol, MAC stability, memory |
| **5** | 1 | 8 | Observability, ergonomics |
| **6** | 1-2 | 4 | Cross-platform, Windows foundation |
| **7** | 1 | 6 | Test coverage for security-critical paths |
| **Total** | **8-11** | **38** | — |

Phases 0-3 are the highest priority and should be done in order. Phases 4-7 can be
parallelized.
