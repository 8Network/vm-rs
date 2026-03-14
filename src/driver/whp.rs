//! Windows driver — Windows Hypervisor Platform (WHP).
//!
//! In-process hypervisor using the same technology as WSL2 and Docker Desktop.
//! Creates WHP partitions with mapped memory, loads Linux kernels directly,
//! and runs vCPUs in dedicated threads with COM1 serial emulation.
//!
//! # Architecture
//!
//! Each VM gets:
//! - A WHP partition with configured vCPU count and memory
//! - Page-aligned host memory mapped to the guest physical address space
//! - Identity-mapped page tables for 64-bit long mode
//! - Minimal GDT with code/data segments
//! - bzImage kernel loaded at 0x100000 with boot_params
//! - A vCPU thread per processor that handles VM exits
//! - COM1 (0x3F8) serial port emulation writing to a log file
//!
//! # Safety
//!
//! All WHP API calls are unsafe FFI. Each call site documents its safety
//! invariants. The key lifetime guarantee: guest memory (VirtualAlloc'd)
//! outlives the WHP partition that maps it.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use windows::Win32::System::Hypervisor::*;
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};

use super::boot::{
    self, BOOT_PARAMS_ADDR, GDT_ADDR, GDT_CODE_SELECTOR, GDT_DATA_SELECTOR, PML4_ADDR,
};
use crate::config::{VmConfig, VmHandle, VmState};
use crate::driver::{VmDriver, VmError};

// COM1 serial port registers
const COM1_DATA: u16 = 0x3F8;
const COM1_LSR: u16 = 0x3FD;

// ─── Guest Memory ───────────────────────────────────────────────────

/// Page-aligned guest physical memory allocated via VirtualAlloc.
///
/// VirtualAlloc guarantees page alignment (4KB), which WHP requires
/// for WHvMapGpaRange. Freed via VirtualFree on drop.
struct GuestMemory {
    ptr: *mut u8,
    size: usize,
}

// SAFETY: GuestMemory is exclusively owned by a single WhpVm.
// The raw pointer is accessed by the boot thread (for setup) and then
// by the vCPU thread (for serial emulation). Access is synchronized:
// setup completes before the vCPU thread starts.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    fn allocate(size: usize) -> Result<Self, VmError> {
        // SAFETY: VirtualAlloc with MEM_COMMIT|MEM_RESERVE allocates
        // and commits `size` bytes of page-aligned virtual memory.
        // Returns null on failure.
        let ptr = unsafe { VirtualAlloc(None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if ptr.is_null() {
            return Err(VmError::Hypervisor(format!(
                "VirtualAlloc failed for {} MB",
                size / (1024 * 1024)
            )));
        }
        Ok(Self {
            ptr: ptr as *mut u8,
            size,
        })
    }

    /// Get a mutable slice over the entire guest memory.
    ///
    /// # Safety
    /// Caller must ensure no concurrent writes to the same region.
    unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.ptr, self.size)
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: ptr was allocated with VirtualAlloc(MEM_COMMIT|MEM_RESERVE).
            // Passing size=0 with MEM_RELEASE frees the entire allocation.
            unsafe {
                let _ = VirtualFree(self.ptr as *mut _, 0, MEM_RELEASE);
            }
        }
    }
}

// ─── Per-VM State ───────────────────────────────────────────────────

/// Internal state for a running WHP VM.
struct WhpVm {
    /// WHP partition handle.
    partition: WHV_PARTITION_HANDLE,
    /// Guest physical memory (VirtualAlloc'd, outlives the partition).
    _memory: GuestMemory,
    /// Shared VM state (read by driver, written by vCPU thread).
    state: Arc<RwLock<VmState>>,
    /// State to restore after a successful resume.
    resume_state: Option<VmState>,
    /// Stop flag — signals the vCPU thread to exit.
    stop_flag: Arc<AtomicBool>,
    /// vCPU execution thread handle.
    vcpu_thread: Option<std::thread::JoinHandle<()>>,
    /// Serial console log path.
    serial_log: std::path::PathBuf,
}

/// Wrapper to send WHV_PARTITION_HANDLE across threads.
///
/// WHP partition handles are kernel objects, safe to use from any thread.
/// The windows crate may not implement Send on the handle type.
#[derive(Clone, Copy)]
struct SendablePartition(WHV_PARTITION_HANDLE);

// SAFETY: WHP partition handles are kernel-managed objects.
// All WHP API calls are thread-safe — the kernel serializes access.
unsafe impl Send for SendablePartition {}

// ─── WHP Driver ─────────────────────────────────────────────────────

/// Windows Hypervisor Platform driver.
///
/// Uses WHP to run Linux VMs in-process. Each VM gets a dedicated WHP
/// partition with memory-mapped guest RAM and per-vCPU execution threads.
pub struct WhpDriver {
    vms: Mutex<HashMap<String, WhpVm>>,
}

impl Default for WhpDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl WhpDriver {
    pub fn new() -> Self {
        Self {
            vms: Mutex::new(HashMap::new()),
        }
    }

    /// Check if WHP is available on this machine.
    pub fn is_available() -> bool {
        // SAFETY: WHvGetCapability reads a capability value from the hypervisor.
        // WHvCapabilityCodeHypervisorPresent queries if the hypervisor is running.
        let mut capability = WHV_CAPABILITY {
            HypervisorPresent: windows::Win32::Foundation::BOOL(0),
        };
        let result = unsafe {
            WHvGetCapability(
                WHvCapabilityCodeHypervisorPresent,
                &mut capability as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WHV_CAPABILITY>() as u32,
                None,
            )
        };
        // SAFETY: HypervisorPresent is a union field — valid when
        // WHvGetCapability succeeded with WHvCapabilityCodeHypervisorPresent.
        result.is_ok() && unsafe { capability.HypervisorPresent.as_bool() }
    }
}

impl VmDriver for WhpDriver {
    fn boot(&self, config: &VmConfig) -> Result<VmHandle, VmError> {
        let name = &config.name;
        let memory_bytes = config.memory_mb * 1024 * 1024;

        tracing::info!(
            vm = %name,
            cpus = config.cpus,
            memory_mb = config.memory_mb,
            "booting VM via WHP"
        );

        // ── Step 1: Check WHP availability ──

        if !Self::is_available() {
            return Err(VmError::Hypervisor(
                "Windows Hypervisor Platform is not available. \
                 Enable Hyper-V in Windows Features."
                    .into(),
            ));
        }

        // ── Step 2: Create and configure partition ──

        // SAFETY: WHvCreatePartition allocates a new partition object.
        let partition = unsafe { WHvCreatePartition() }.map_err(|e| VmError::BootFailed {
            name: name.clone(),
            detail: format!("WHvCreatePartition failed: {e}"),
        })?;

        // Set processor count
        let mut prop: WHV_PARTITION_PROPERTY = unsafe { std::mem::zeroed() };
        prop.ProcessorCount = config.cpus as u32;
        // SAFETY: partition is valid, property buffer is correctly sized.
        unsafe {
            WHvSetPartitionProperty(
                partition,
                WHvPartitionPropertyCodeProcessorCount,
                &prop as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )
        }
        .map_err(|e| {
            // SAFETY: partition is valid.
            unsafe {
                let _ = WHvDeletePartition(partition);
            }
            VmError::BootFailed {
                name: name.clone(),
                detail: format!("WHvSetPartitionProperty(ProcessorCount) failed: {e}"),
            }
        })?;

        // Materialize partition in the hypervisor
        // SAFETY: partition is configured and ready to be set up.
        unsafe { WHvSetupPartition(partition) }.map_err(|e| {
            unsafe {
                let _ = WHvDeletePartition(partition);
            }
            VmError::BootFailed {
                name: name.clone(),
                detail: format!("WHvSetupPartition failed: {e}"),
            }
        })?;

        // ── Step 3: Allocate and map guest memory ──

        let mut memory = GuestMemory::allocate(memory_bytes).map_err(|e| {
            unsafe {
                let _ = WHvDeletePartition(partition);
            }
            e
        })?;

        // SAFETY: memory.as_ptr() is page-aligned (VirtualAlloc guarantee),
        // memory_bytes matches the allocation size, partition is set up.
        unsafe {
            WHvMapGpaRange(
                partition,
                memory.as_ptr() as *const std::ffi::c_void,
                0, // Map at GPA 0
                memory_bytes as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            )
        }
        .map_err(|e| {
            unsafe {
                let _ = WHvDeletePartition(partition);
            }
            VmError::BootFailed {
                name: name.clone(),
                detail: format!("WHvMapGpaRange failed: {e}"),
            }
        })?;

        // ── Step 4: Set up guest memory contents ──

        // SAFETY: We have exclusive access to memory during setup (no vCPU yet).
        let mem = unsafe { memory.as_mut_slice() };

        boot::setup_page_tables(mem, config.memory_mb);
        boot::setup_gdt(mem);

        let default_cmdline = if config.root_disk.is_some() {
            "console=ttyS0 root=/dev/vda1 rw"
        } else {
            "console=ttyS0"
        };
        let cmdline = config.cmdline.as_deref().unwrap_or(default_cmdline);

        let entry_point = boot::load_kernel(
            mem,
            &config.kernel,
            config.initramfs.as_deref(),
            cmdline,
            config.memory_mb,
        )
        .map_err(|mut e| {
            // Attach VM name to boot errors from load_kernel
            if let VmError::BootFailed { ref mut name, .. } = e {
                *name = config.name.clone();
            }
            unsafe {
                let _ = WHvDeletePartition(partition);
            }
            e
        })?;

        // ── Step 5: Create vCPU and set initial registers ──

        // SAFETY: partition is set up, vCPU index 0.
        unsafe { WHvCreateVirtualProcessor(partition, 0, 0) }.map_err(|e| {
            unsafe {
                let _ = WHvDeletePartition(partition);
            }
            VmError::BootFailed {
                name: name.clone(),
                detail: format!("WHvCreateVirtualProcessor failed: {e}"),
            }
        })?;

        setup_initial_registers(partition, entry_point).map_err(|e| {
            unsafe {
                let _ = WHvDeleteVirtualProcessor(partition, 0);
                let _ = WHvDeletePartition(partition);
            }
            e
        })?;

        // ── Step 6: Spawn vCPU thread ──

        let state = Arc::new(RwLock::new(VmState::Starting));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let serial_log = config.serial_log.clone();

        let sendable = SendablePartition(partition);
        let state_clone = Arc::clone(&state);
        let stop_clone = Arc::clone(&stop_flag);
        let log_path = serial_log.clone();
        let vm_name = name.clone();

        let vcpu_thread = std::thread::Builder::new()
            .name(format!("vcpu-{}", name))
            .spawn(move || {
                vcpu_loop(sendable, state_clone, stop_clone, &log_path, &vm_name);
            })
            .map_err(|e| {
                unsafe {
                    let _ = WHvDeleteVirtualProcessor(partition, 0);
                    let _ = WHvDeletePartition(partition);
                }
                VmError::BootFailed {
                    name: name.clone(),
                    detail: format!("failed to spawn vCPU thread: {e}"),
                }
            })?;

        // ── Step 7: Store VM state and return handle ──

        let vm = WhpVm {
            partition,
            _memory: memory,
            state: Arc::clone(&state),
            resume_state: None,
            stop_flag: Arc::clone(&stop_flag),
            vcpu_thread: Some(vcpu_thread),
            serial_log: serial_log.clone(),
        };

        {
            let mut vms = self
                .vms
                .lock()
                .map_err(|e| VmError::Hypervisor(format!("VM lock poisoned: {e}")))?;
            vms.insert(name.clone(), vm);
        }

        Ok(VmHandle {
            name: name.clone(),
            namespace: config.namespace.clone(),
            state: VmState::Starting,
            process: None, // In-process, no separate PID
            serial_log,
            machine_id: None,
        })
    }

    fn stop(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM lock poisoned: {e}")))?;

        let vm = vms.get_mut(&handle.name).ok_or_else(|| VmError::NotFound {
            name: handle.name.clone(),
        })?;

        // Signal vCPU thread to stop
        vm.stop_flag.store(true, Ordering::Release);

        // Cancel the blocking WHvRunVirtualProcessor call
        // SAFETY: partition is valid, vCPU 0 exists.
        let _ = unsafe { WHvCancelRunVirtualProcessor(vm.partition, 0, 0) };

        // Wait for vCPU thread to exit
        if let Some(thread) = vm.vcpu_thread.take() {
            let _ = thread.join();
        }

        // Update state
        if let Ok(mut state) = vm.state.write() {
            *state = VmState::Stopped;
        }

        // Clean up WHP resources
        // SAFETY: vCPU thread has exited, no concurrent access.
        unsafe {
            let _ = WHvDeleteVirtualProcessor(vm.partition, 0);
            let _ = WHvDeletePartition(vm.partition);
        }

        vms.remove(&handle.name);
        tracing::info!(vm = %handle.name, "VM stopped");
        Ok(())
    }

    fn kill(&self, handle: &VmHandle) -> Result<(), VmError> {
        // Kill is the same as stop — WHP doesn't have a graceful/forceful distinction
        // since we control the execution loop directly.
        self.stop(handle)
    }

    fn state(&self, handle: &VmHandle) -> Result<VmState, VmError> {
        let vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM lock poisoned: {e}")))?;

        match vms.get(&handle.name) {
            Some(vm) => {
                let state = vm
                    .state
                    .read()
                    .map_err(|e| VmError::Hypervisor(format!("state lock poisoned: {e}")))?;
                Ok(state.clone())
            }
            None => Ok(VmState::Stopped),
        }
    }

    fn pause(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM lock poisoned: {e}")))?;

        let vm = vms.get_mut(&handle.name).ok_or_else(|| VmError::NotFound {
            name: handle.name.clone(),
        })?;

        let current_state = vm
            .state
            .read()
            .map_err(|e| VmError::Hypervisor(format!("state lock poisoned: {e}")))?
            .clone();
        if !matches!(current_state, VmState::Running { .. }) {
            return Err(VmError::Hypervisor("can only pause a running VM".into()));
        }

        // Signal vCPU to stop running (but don't destroy partition)
        vm.stop_flag.store(true, Ordering::Release);

        // SAFETY: partition and vCPU are valid.
        let _ = unsafe { WHvCancelRunVirtualProcessor(vm.partition, 0, 0) };

        if let Some(thread) = vm.vcpu_thread.take() {
            let _ = thread.join();
        }

        vm.resume_state = Some(current_state);
        if let Ok(mut state) = vm.state.write() {
            *state = VmState::Paused;
        }

        tracing::info!(vm = %handle.name, "VM paused");
        Ok(())
    }

    fn resume(&self, handle: &VmHandle) -> Result<(), VmError> {
        let mut vms = self
            .vms
            .lock()
            .map_err(|e| VmError::Hypervisor(format!("VM lock poisoned: {e}")))?;

        let vm = vms.get_mut(&handle.name).ok_or_else(|| VmError::NotFound {
            name: handle.name.clone(),
        })?;

        let current_state = vm
            .state
            .read()
            .map_err(|e| VmError::Hypervisor(format!("state lock poisoned: {e}")))?
            .clone();
        if current_state != VmState::Paused {
            return Err(VmError::Hypervisor("can only resume a paused VM".into()));
        }
        if vm.vcpu_thread.is_some() {
            return Err(VmError::Hypervisor(format!(
                "VM '{}' already has an active vCPU thread",
                handle.name
            )));
        }

        // Reset stop flag and spawn new vCPU thread
        vm.stop_flag.store(false, Ordering::Release);
        let resumed_state = vm.resume_state.clone().unwrap_or(VmState::Starting);

        let sendable = SendablePartition(vm.partition);
        let state_clone = Arc::clone(&vm.state);
        let stop_clone = Arc::clone(&vm.stop_flag);
        let log_path = vm.serial_log.clone();
        let vm_name = handle.name.clone();

        let thread = std::thread::Builder::new()
            .name(format!("vcpu-{}", handle.name))
            .spawn(move || {
                vcpu_loop(sendable, state_clone, stop_clone, &log_path, &vm_name);
            })
            .map_err(|e| VmError::Hypervisor(format!("failed to spawn vCPU thread: {e}")))?;

        vm.vcpu_thread = Some(thread);
        vm.resume_state = None;
        if let Ok(mut state) = vm.state.write() {
            *state = resumed_state;
        }

        tracing::info!(vm = %handle.name, "VM resumed");
        Ok(())
    }
}

impl Drop for WhpVm {
    fn drop(&mut self) {
        // Ensure vCPU thread is stopped
        self.stop_flag.store(true, Ordering::Release);
        let _ = unsafe { WHvCancelRunVirtualProcessor(self.partition, 0, 0) };
        if let Some(thread) = self.vcpu_thread.take() {
            let _ = thread.join();
        }
        // Clean up WHP resources (vCPU before partition)
        // SAFETY: vCPU thread has exited, partition is valid.
        unsafe {
            let _ = WHvDeleteVirtualProcessor(self.partition, 0);
            let _ = WHvDeletePartition(self.partition);
        }
        // GuestMemory is freed by its own Drop impl
    }
}

// ─── CPU Register Setup ─────────────────────────────────────────────

/// Set initial vCPU registers for 64-bit long mode entry.
///
/// Sets up: RIP (kernel entry), RSP (stack), CR0/CR3/CR4/EFER (long mode),
/// segment registers (code/data), GDTR, and RFLAGS.
fn setup_initial_registers(
    partition: WHV_PARTITION_HANDLE,
    entry_point: u64,
) -> Result<(), VmError> {
    // Register names and values — must be parallel arrays
    let reg_names = [
        WHV_REGISTER_NAME(WHvX64RegisterRip.0),
        WHV_REGISTER_NAME(WHvX64RegisterRsp.0),
        WHV_REGISTER_NAME(WHvX64RegisterRflags.0),
        WHV_REGISTER_NAME(WHvX64RegisterCr0.0),
        WHV_REGISTER_NAME(WHvX64RegisterCr3.0),
        WHV_REGISTER_NAME(WHvX64RegisterCr4.0),
        WHV_REGISTER_NAME(WHvX64RegisterEfer.0),
        WHV_REGISTER_NAME(WHvX64RegisterRsi.0), // boot_params pointer
    ];

    let reg_values = [
        // RIP = kernel entry point
        WHV_REGISTER_VALUE { Reg64: entry_point },
        // RSP = top of low memory (below boot_params)
        WHV_REGISTER_VALUE {
            Reg64: BOOT_PARAMS_ADDR - 0x10,
        },
        // RFLAGS = reserved bit 1 set, interrupts disabled
        WHV_REGISTER_VALUE { Reg64: 0x2 },
        // CR0 = PE (protected mode) + PG (paging) + WP (write protect)
        WHV_REGISTER_VALUE { Reg64: 0x8001_0001 },
        // CR3 = PML4 page table base
        WHV_REGISTER_VALUE { Reg64: PML4_ADDR },
        // CR4 = PAE (required for long mode)
        WHV_REGISTER_VALUE { Reg64: 0x20 },
        // EFER = LME (long mode enable) + LMA (long mode active) + NXE
        WHV_REGISTER_VALUE { Reg64: 0xD00 },
        // RSI = pointer to boot_params (Linux boot protocol)
        WHV_REGISTER_VALUE {
            Reg64: BOOT_PARAMS_ADDR,
        },
    ];

    // SAFETY: partition is valid, vCPU 0 exists, arrays are correctly sized.
    unsafe {
        WHvSetVirtualProcessorRegisters(
            partition,
            0, // vCPU index
            reg_names.as_ptr(),
            reg_names.len() as u32,
            reg_values.as_ptr(),
        )
    }
    .map_err(|e| VmError::BootFailed {
        name: String::new(),
        detail: format!("WHvSetVirtualProcessorRegisters (general) failed: {e}"),
    })?;

    // Set segment registers and GDTR separately (they use the Segment/Table
    // union variants which need different construction)
    set_segment_registers(partition)?;

    Ok(())
}

/// Set segment registers (CS, DS, ES, SS) and GDTR for long mode.
fn set_segment_registers(partition: WHV_PARTITION_HANDLE) -> Result<(), VmError> {
    let code_segment = WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: 0xFFFF_FFFF,
        Selector: GDT_CODE_SELECTOR,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: 0xA09B, // Present, 64-bit code, execute/read
        },
    };

    let data_segment = WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: 0xFFFF_FFFF,
        Selector: GDT_DATA_SELECTOR,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 {
            Attributes: 0xC093, // Present, 32-bit data, read/write
        },
    };

    let gdt_table = WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Base: GDT_ADDR,
        Limit: 23, // 3 entries * 8 bytes - 1
    };

    // CS
    let names = [WHV_REGISTER_NAME(WHvX64RegisterCs.0)];
    let values = [WHV_REGISTER_VALUE {
        Segment: code_segment,
    }];
    // SAFETY: partition is valid, vCPU 0 exists.
    unsafe { WHvSetVirtualProcessorRegisters(partition, 0, names.as_ptr(), 1, values.as_ptr()) }
        .map_err(|e| VmError::BootFailed {
            name: String::new(),
            detail: format!("failed to set CS: {e}"),
        })?;

    // DS, ES, SS — all use data segment
    for reg in [WHvX64RegisterDs, WHvX64RegisterEs, WHvX64RegisterSs] {
        let names = [WHV_REGISTER_NAME(reg.0)];
        let values = [WHV_REGISTER_VALUE {
            Segment: data_segment,
        }];
        unsafe {
            WHvSetVirtualProcessorRegisters(partition, 0, names.as_ptr(), 1, values.as_ptr())
        }
        .map_err(|e| VmError::BootFailed {
            name: String::new(),
            detail: format!("failed to set segment register: {e}"),
        })?;
    }

    // GDTR
    let names = [WHV_REGISTER_NAME(WHvX64RegisterGdtr.0)];
    let values = [WHV_REGISTER_VALUE { Table: gdt_table }];
    unsafe { WHvSetVirtualProcessorRegisters(partition, 0, names.as_ptr(), 1, values.as_ptr()) }
        .map_err(|e| VmError::BootFailed {
            name: String::new(),
            detail: format!("failed to set GDTR: {e}"),
        })?;

    Ok(())
}

// ─── vCPU Execution Loop ────────────────────────────────────────────

/// Main vCPU execution loop.
///
/// Runs on a dedicated thread. Calls WHvRunVirtualProcessor in a loop,
/// handling VM exits for I/O (serial console), HLT (idle/shutdown),
/// and cancellation (stop/pause).
fn vcpu_loop(
    partition: SendablePartition,
    state: Arc<RwLock<VmState>>,
    stop_flag: Arc<AtomicBool>,
    serial_log_path: &Path,
    vm_name: &str,
) {
    let partition = partition.0;
    let mut serial_file = match std::fs::File::create(serial_log_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(vm = %vm_name, "failed to create serial log: {e}");
            update_state(
                &state,
                VmState::Failed {
                    reason: format!("failed to create serial log: {e}"),
                },
            );
            return;
        }
    };
    let mut serial_buffer = String::new();

    loop {
        if stop_flag.load(Ordering::Acquire) {
            break;
        }

        let mut exit_context: WHV_RUN_VP_EXIT_CONTEXT = unsafe { std::mem::zeroed() };

        // SAFETY: partition and vCPU 0 are valid. exit_context is correctly sized.
        let result = unsafe {
            WHvRunVirtualProcessor(
                partition,
                0,
                &mut exit_context as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            )
        };

        if let Err(e) = result {
            tracing::error!(vm = %vm_name, "WHvRunVirtualProcessor failed: {e}");
            update_state(
                &state,
                VmState::Failed {
                    reason: format!("vCPU execution failed: {e}"),
                },
            );
            break;
        }

        match exit_context.ExitReason {
            WHvRunVpExitReasonX64IoPortAccess => {
                // SAFETY: ExitReason is IoPortAccess, so the union field is valid.
                let io = unsafe { &exit_context.Anonymous.IoPortAccess };
                handle_io_port(
                    partition,
                    &exit_context,
                    io,
                    &mut serial_file,
                    &mut serial_buffer,
                    &state,
                    vm_name,
                );
            }

            WHvRunVpExitReasonX64Halt => {
                // Check RFLAGS.IF — if interrupts enabled, this is idle (HLT loop).
                // If disabled, the kernel has halted.
                let rflags = exit_context.VpContext.Rflags;
                if rflags & 0x200 != 0 {
                    // Idle — sleep briefly and resume
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    advance_rip(partition, &exit_context);
                } else {
                    // Halted with interrupts disabled — VM is done
                    tracing::info!(vm = %vm_name, "VM halted (interrupts disabled)");
                    update_state(&state, VmState::Stopped);
                    break;
                }
            }

            WHvRunVpExitReasonCanceled => {
                // Cancellation requested (stop or pause)
                tracing::debug!(vm = %vm_name, "vCPU execution cancelled");
                break;
            }

            WHvRunVpExitReasonMemoryAccess => {
                // Unmapped memory access — fatal
                let gpa = unsafe { exit_context.Anonymous.MemoryAccess.Gpa };
                let reason = format!("unmapped memory access at GPA 0x{:x}", gpa);
                tracing::error!(vm = %vm_name, "{}", reason);
                update_state(&state, VmState::Failed { reason });
                break;
            }

            other => {
                // Unknown exit — log and advance past the instruction
                tracing::debug!(
                    vm = %vm_name,
                    exit_reason = other.0,
                    "unhandled VM exit"
                );
                advance_rip(partition, &exit_context);
            }
        }
    }
}

/// Handle an I/O port access VM exit.
///
/// Emulates COM1 serial port for boot console output. All other ports
/// are ignored (reads return 0).
fn handle_io_port(
    partition: WHV_PARTITION_HANDLE,
    exit_context: &WHV_RUN_VP_EXIT_CONTEXT,
    io: &WHV_X64_IO_PORT_ACCESS_CONTEXT,
    serial_file: &mut std::fs::File,
    serial_buffer: &mut String,
    state: &Arc<RwLock<VmState>>,
    vm_name: &str,
) {
    let port = io.PortNumber;
    // SAFETY: AccessInfo is a union; AsUINT32 gives the raw bitfield.
    // Bit 0 = IsWrite per the WHP C header.
    let is_write = unsafe { io.AccessInfo.AsUINT32 } & 1 != 0;

    if is_write && port == COM1_DATA {
        // Serial output — write byte to log file and buffer
        let byte = (io.Rax & 0xFF) as u8;
        let _ = serial_file.write_all(&[byte]);
        let _ = serial_file.flush();

        if byte.is_ascii() {
            serial_buffer.push(byte as char);

            // Check for ready marker
            if let Some(pos) = serial_buffer.find(crate::config::READY_MARKER) {
                let after = &serial_buffer[pos + crate::config::READY_MARKER.len()..];
                if let Some(ip) = after.split_whitespace().next() {
                    let ip = ip.trim().to_string();
                    if !ip.is_empty() {
                        tracing::info!(vm = %vm_name, ip = %ip, "VM ready");
                        update_state(state, VmState::Running { ip });
                        serial_buffer.clear();
                    }
                }
            }
        }
    } else if !is_write && port == COM1_LSR {
        // Line status register: report transmitter empty + ready
        set_rax(partition, 0x60);
    } else if !is_write {
        // Unknown port read — return 0
        set_rax(partition, 0);
    }
    // For all cases, advance past the I/O instruction

    advance_rip(partition, exit_context);
}

/// Advance RIP past the current instruction.
fn advance_rip(partition: WHV_PARTITION_HANDLE, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) {
    // _bitfield lower 4 bits = InstructionLength per the WHP C header
    let new_rip = exit_context.VpContext.Rip + (exit_context.VpContext._bitfield & 0xF) as u64;

    let names = [WHV_REGISTER_NAME(WHvX64RegisterRip.0)];
    let values = [WHV_REGISTER_VALUE { Reg64: new_rip }];

    // SAFETY: partition and vCPU 0 are valid.
    let _ = unsafe {
        WHvSetVirtualProcessorRegisters(partition, 0, names.as_ptr(), 1, values.as_ptr())
    };
}

/// Set RAX register (for returning values from IN instructions).
fn set_rax(partition: WHV_PARTITION_HANDLE, value: u64) {
    let names = [WHV_REGISTER_NAME(WHvX64RegisterRax.0)];
    let values = [WHV_REGISTER_VALUE { Reg64: value }];

    // SAFETY: partition and vCPU 0 are valid.
    let _ = unsafe {
        WHvSetVirtualProcessorRegisters(partition, 0, names.as_ptr(), 1, values.as_ptr())
    };
}

/// Update shared VM state.
fn update_state(state: &Arc<RwLock<VmState>>, new_state: VmState) {
    if let Ok(mut s) = state.write() {
        *s = new_state;
    }
}
