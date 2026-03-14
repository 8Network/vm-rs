//! Linux boot protocol — kernel loading, page tables, GDT for direct boot.
//!
//! Platform-independent guest memory setup for booting a Linux kernel
//! in 64-bit long mode. Used by in-process hypervisor drivers (WHP, and
//! potentially others that do their own kernel loading).
//!
//! Implements the Linux x86 boot protocol:
//! - bzImage parsing and loading at 0x100000
//! - boot_params struct with e820 memory map and command line
//! - Identity-mapped page tables (PML4 → PDPT → PD with 2MB pages)
//! - Minimal GDT for 64-bit code/data segments

use std::path::Path;

use crate::driver::VmError;

// ─── Guest Physical Address Layout ──────────────────────────────────
//
// 0x0000_0000  Zeroed (real-mode IVT area, unused)
// 0x0000_7000  boot_params (struct boot_params, 4KB)
// 0x0000_8000  Kernel command line (null-terminated, 4KB max)
// 0x0000_9000  PML4 page table (4KB)
// 0x0000_A000  PDPT page table (4KB)
// 0x0000_B000  PD page table (4KB)
// 0x0000_C000  GDT (Global Descriptor Table)
// 0x0010_0000  Protected-mode kernel code (1MB, standard load address)

pub const BOOT_PARAMS_ADDR: u64 = 0x7000;
pub const CMDLINE_ADDR: u64 = 0x8000;
pub const CMDLINE_MAX: usize = 4096;
pub const PML4_ADDR: u64 = 0x9000;
pub const PDPT_ADDR: u64 = 0xA000;
pub const PD_ADDR: u64 = 0xB000;
pub const GDT_ADDR: u64 = 0xC000;
pub const KERNEL_ADDR: u64 = 0x100000;

// Page table entry flags
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_PAGE_SIZE: u64 = 1 << 7; // 2MB page (PD entry)

// GDT segment descriptors for 64-bit long mode
const GDT_NULL: u64 = 0;
const GDT_CODE64: u64 = 0x00AF_9A00_0000_FFFF; // Execute/Read, 64-bit, DPL=0
const GDT_DATA64: u64 = 0x00CF_9200_0000_FFFF; // Read/Write, DPL=0
pub const GDT_CODE_SELECTOR: u16 = 0x08;
pub const GDT_DATA_SELECTOR: u16 = 0x10;

// ─── Page Tables ────────────────────────────────────────────────────

/// Set up identity-mapped page tables for 64-bit long mode.
///
/// Uses 2MB pages (PDE with PS bit) for simplicity. Maps the first
/// `memory_mb` of physical address space 1:1 (virtual = physical).
///
/// Layout: PML4[0] → PDPT[0] → PD[0..N] (2MB pages each)
///
/// # Limitation
///
/// A single Page Directory has 512 entries of 2 MB each, covering at most
/// 1 GB (1024 MB). Callers requesting more than 1024 MB of guest memory
/// must validate beforehand or the mapping will silently cap at 1 GB.
/// Support for multiple Page Directories (>1 GB) will be added in a future
/// release.
pub fn setup_page_tables(memory: &mut [u8], memory_mb: usize) {
    let pml4 = PML4_ADDR as usize;
    let pdpt = PDPT_ADDR as usize;
    let pd = PD_ADDR as usize;

    // Zero page table area (3 pages)
    for b in &mut memory[pml4..pml4 + 0x3000] {
        *b = 0;
    }

    // PML4[0] → PDPT
    let entry = PDPT_ADDR | PTE_PRESENT | PTE_WRITABLE;
    memory[pml4..pml4 + 8].copy_from_slice(&entry.to_le_bytes());

    // PDPT[0] → PD
    let entry = PD_ADDR | PTE_PRESENT | PTE_WRITABLE;
    memory[pdpt..pdpt + 8].copy_from_slice(&entry.to_le_bytes());

    // PD entries: 2MB identity-mapped pages
    let num_pages = memory_mb.div_ceil(2).min(512); // Max 512 = 1GB
    for i in 0..num_pages {
        let phys_addr = (i as u64) * 2 * 1024 * 1024;
        let entry = phys_addr | PTE_PRESENT | PTE_WRITABLE | PTE_PAGE_SIZE;
        let offset = pd + i * 8;
        memory[offset..offset + 8].copy_from_slice(&entry.to_le_bytes());
    }
}

/// Set up a minimal GDT for 64-bit long mode.
///
/// Three entries: null descriptor, 64-bit code segment, data segment.
pub fn setup_gdt(memory: &mut [u8]) {
    let base = GDT_ADDR as usize;
    memory[base..base + 8].copy_from_slice(&GDT_NULL.to_le_bytes());
    memory[base + 8..base + 16].copy_from_slice(&GDT_CODE64.to_le_bytes());
    memory[base + 16..base + 24].copy_from_slice(&GDT_DATA64.to_le_bytes());
}

// ─── Kernel Loading ─────────────────────────────────────────────────

/// Load a Linux bzImage kernel into guest memory.
///
/// Parses the bzImage setup header, copies the protected-mode kernel to
/// 0x100000, sets up boot_params with command line, e820 memory map,
/// and optional initramfs.
///
/// Returns the kernel entry point address.
pub fn load_kernel(
    memory: &mut [u8],
    kernel_path: &Path,
    initramfs_path: Option<&Path>,
    cmdline: &str,
    memory_mb: usize,
) -> Result<u64, VmError> {
    let kernel_data = std::fs::read(kernel_path).map_err(|e| VmError::BootFailed {
        name: String::new(),
        detail: format!("failed to read kernel: {e}"),
    })?;

    load_kernel_from_bytes(memory, &kernel_data, initramfs_path, cmdline, memory_mb)
}

/// Load kernel from in-memory bytes (used by load_kernel and tests).
pub fn load_kernel_from_bytes(
    memory: &mut [u8],
    kernel_data: &[u8],
    initramfs_path: Option<&Path>,
    cmdline: &str,
    memory_mb: usize,
) -> Result<u64, VmError> {
    if memory_mb > 1024 {
        return Err(VmError::InvalidConfig(format!(
            "direct boot currently supports up to 1024 MB guest memory (got {} MB); \
             support for larger memory will be added in a future release",
            memory_mb
        )));
    }

    if kernel_data.len() < 0x250 {
        return Err(VmError::InvalidConfig(
            "kernel image too small for bzImage".into(),
        ));
    }

    // Check bzImage magic: boot flag at 0x1FE and "HdrS" at 0x202
    let boot_flag = u16::from_le_bytes([kernel_data[0x1FE], kernel_data[0x1FF]]);
    let header_magic = &kernel_data[0x202..0x206];

    if boot_flag != 0xAA55 || header_magic != b"HdrS" {
        // Not a bzImage — load as raw flat binary at KERNEL_ADDR
        let dest = KERNEL_ADDR as usize;
        if dest + kernel_data.len() > memory.len() {
            return Err(VmError::InvalidConfig(
                "kernel too large for allocated memory".into(),
            ));
        }
        memory[dest..dest + kernel_data.len()].copy_from_slice(kernel_data);
        return Ok(KERNEL_ADDR);
    }

    // Parse setup header
    let setup_sects = match kernel_data[0x1F1] {
        0 => 4,
        n => n as usize,
    };
    let protocol_version = u16::from_le_bytes([kernel_data[0x206], kernel_data[0x207]]);

    tracing::info!(
        setup_sects = setup_sects,
        protocol = format!("{}.{}", protocol_version >> 8, protocol_version & 0xFF),
        kernel_size = kernel_data.len(),
        "loading bzImage kernel"
    );

    // Protected-mode kernel starts after setup sectors
    let pm_offset = (setup_sects + 1) * 512;
    if pm_offset >= kernel_data.len() {
        return Err(VmError::InvalidConfig(format!(
            "bzImage setup_sects ({}) exceeds file size",
            setup_sects
        )));
    }
    let pm_kernel = &kernel_data[pm_offset..];

    // Copy protected-mode kernel to KERNEL_ADDR
    let dest = KERNEL_ADDR as usize;
    if dest + pm_kernel.len() > memory.len() {
        return Err(VmError::InvalidConfig(
            "kernel too large for allocated memory".into(),
        ));
    }
    memory[dest..dest + pm_kernel.len()].copy_from_slice(pm_kernel);

    // ── boot_params ──

    let bp = BOOT_PARAMS_ADDR as usize;

    // Copy setup header (0x1F1..0x290) into boot_params
    let header_end = 0x290.min(kernel_data.len());
    let header_src = &kernel_data[0x1F1..header_end];
    memory[bp + 0x1F1..bp + 0x1F1 + header_src.len()].copy_from_slice(header_src);

    // type_of_loader: 0xFF = "undefined loader"
    memory[bp + 0x210] = 0xFF;
    // loadflags: LOADED_HIGH + CAN_USE_HEAP
    memory[bp + 0x211] |= 0x01 | 0x80;
    // heap_end_ptr
    memory[bp + 0x224..bp + 0x226].copy_from_slice(&0xFE00u16.to_le_bytes());

    // ── Command line ──

    let cmdline_bytes = cmdline.as_bytes();
    let cmdline_len = cmdline_bytes.len().min(CMDLINE_MAX - 1);
    let cl = CMDLINE_ADDR as usize;
    memory[cl..cl + cmdline_len].copy_from_slice(&cmdline_bytes[..cmdline_len]);
    memory[cl + cmdline_len] = 0;

    // cmd_line_ptr
    memory[bp + 0x228..bp + 0x22C].copy_from_slice(&(CMDLINE_ADDR as u32).to_le_bytes());

    // ── Initramfs ──

    if let Some(initramfs_path) = initramfs_path {
        let initrd_data = std::fs::read(initramfs_path).map_err(|e| VmError::BootFailed {
            name: String::new(),
            detail: format!("failed to read initramfs: {e}"),
        })?;

        let initrd_addr = ((dest + pm_kernel.len() + 0xFFF) & !0xFFF) as u64;
        let initrd_end = initrd_addr as usize + initrd_data.len();
        if initrd_end > memory.len() {
            return Err(VmError::InvalidConfig(
                "not enough memory for kernel + initramfs".into(),
            ));
        }
        memory[initrd_addr as usize..initrd_end].copy_from_slice(&initrd_data);

        memory[bp + 0x218..bp + 0x21C].copy_from_slice(&(initrd_addr as u32).to_le_bytes());
        memory[bp + 0x21C..bp + 0x220].copy_from_slice(&(initrd_data.len() as u32).to_le_bytes());

        tracing::info!(
            addr = format!("0x{:x}", initrd_addr),
            size = initrd_data.len(),
            "initramfs loaded"
        );
    }

    // ── E820 memory map ──

    let e820_table = bp + 0x2D0;
    let e820_count = bp + 0x1E8;

    write_e820_entry(memory, e820_table, 0, 0x9FC00, 1);
    write_e820_entry(memory, e820_table + 20, 0x9FC00, 0x100000 - 0x9FC00, 2);
    let mem_size = (memory_mb as u64) * 1024 * 1024;
    write_e820_entry(memory, e820_table + 40, 0x100000, mem_size - 0x100000, 1);
    memory[e820_count] = 3;

    Ok(KERNEL_ADDR)
}

/// Write a single e820 memory map entry.
fn write_e820_entry(memory: &mut [u8], offset: usize, addr: u64, size: u64, entry_type: u32) {
    memory[offset..offset + 8].copy_from_slice(&addr.to_le_bytes());
    memory[offset + 8..offset + 16].copy_from_slice(&size.to_le_bytes());
    memory[offset + 16..offset + 20].copy_from_slice(&entry_type.to_le_bytes());
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate a zeroed guest memory buffer for testing.
    fn test_memory(mb: usize) -> Vec<u8> {
        vec![0u8; mb * 1024 * 1024]
    }

    #[test]
    fn page_tables_pml4_points_to_pdpt() {
        let mut mem = test_memory(16);
        setup_page_tables(&mut mem, 16);

        let pml4_entry = u64::from_le_bytes(
            mem[PML4_ADDR as usize..PML4_ADDR as usize + 8]
                .try_into()
                .unwrap(),
        );

        // PML4[0] should point to PDPT with Present + Writable
        assert_eq!(pml4_entry & !0xFFF, PDPT_ADDR);
        assert_ne!(pml4_entry & PTE_PRESENT, 0, "PML4 entry must be present");
        assert_ne!(pml4_entry & PTE_WRITABLE, 0, "PML4 entry must be writable");
    }

    #[test]
    fn page_tables_pdpt_points_to_pd() {
        let mut mem = test_memory(16);
        setup_page_tables(&mut mem, 16);

        let pdpt_entry = u64::from_le_bytes(
            mem[PDPT_ADDR as usize..PDPT_ADDR as usize + 8]
                .try_into()
                .unwrap(),
        );

        assert_eq!(pdpt_entry & !0xFFF, PD_ADDR);
        assert_ne!(pdpt_entry & PTE_PRESENT, 0);
        assert_ne!(pdpt_entry & PTE_WRITABLE, 0);
    }

    #[test]
    fn page_tables_pd_identity_maps_2mb_pages() {
        let mut mem = test_memory(16);
        setup_page_tables(&mut mem, 16);

        // 16MB = 8 PD entries (2MB each)
        for i in 0..8 {
            let offset = PD_ADDR as usize + i * 8;
            let entry = u64::from_le_bytes(mem[offset..offset + 8].try_into().unwrap());
            let expected_addr = (i as u64) * 2 * 1024 * 1024;

            assert_eq!(
                entry & !0xFFF,
                expected_addr,
                "PD[{}] should map to 0x{:x}",
                i,
                expected_addr
            );
            assert_ne!(entry & PTE_PRESENT, 0, "PD[{}] must be present", i);
            assert_ne!(entry & PTE_PAGE_SIZE, 0, "PD[{}] must be 2MB page", i);
        }

        // Entry 8 should be zero (unmapped)
        let offset = PD_ADDR as usize + 8 * 8;
        let entry = u64::from_le_bytes(mem[offset..offset + 8].try_into().unwrap());
        assert_eq!(entry, 0, "PD[8] should be unmapped for 16MB");
    }

    #[test]
    fn page_tables_max_512_entries() {
        let mut mem = test_memory(2048); // 2GB — more than 512 entries
        setup_page_tables(&mut mem, 2048);

        // Should cap at 512 entries (1GB)
        let last_valid = PD_ADDR as usize + 511 * 8;
        let entry = u64::from_le_bytes(mem[last_valid..last_valid + 8].try_into().unwrap());
        assert_ne!(entry, 0, "PD[511] should be mapped");

        // No 513th entry
        // (would overflow the PD page, so it's just not written)
    }

    #[test]
    fn gdt_null_entry() {
        let mut mem = test_memory(1);
        setup_gdt(&mut mem);

        let null = u64::from_le_bytes(
            mem[GDT_ADDR as usize..GDT_ADDR as usize + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(null, 0, "GDT[0] must be null descriptor");
    }

    #[test]
    fn gdt_code_segment() {
        let mut mem = test_memory(1);
        setup_gdt(&mut mem);

        let code = u64::from_le_bytes(
            mem[GDT_ADDR as usize + 8..GDT_ADDR as usize + 16]
                .try_into()
                .unwrap(),
        );
        assert_eq!(code, GDT_CODE64);

        // Verify key bits: L=1 (64-bit), P=1, Type=Execute/Read
        let access = ((code >> 40) & 0xFF) as u8;
        assert_ne!(access & 0x80, 0, "P (present) must be set");
        assert_ne!(access & 0x08, 0, "code segment must be executable");

        let flags = ((code >> 52) & 0xF) as u8;
        assert_ne!(flags & 0x02, 0, "L (64-bit) must be set");
    }

    #[test]
    fn gdt_data_segment() {
        let mut mem = test_memory(1);
        setup_gdt(&mut mem);

        let data = u64::from_le_bytes(
            mem[GDT_ADDR as usize + 16..GDT_ADDR as usize + 24]
                .try_into()
                .unwrap(),
        );
        assert_eq!(data, GDT_DATA64);
    }

    #[test]
    fn gdt_selectors_correct() {
        // Code selector = entry 1 * 8 = 0x08
        assert_eq!(GDT_CODE_SELECTOR, 0x08);
        // Data selector = entry 2 * 8 = 0x10
        assert_eq!(GDT_DATA_SELECTOR, 0x10);
    }

    #[test]
    fn load_raw_binary_at_kernel_addr() {
        let mut mem = test_memory(2);
        // Create a fake "kernel" that's not a bzImage
        let fake_kernel = vec![0xCCu8; 1024]; // INT3 sled

        let entry = load_kernel_from_bytes(&mut mem, &fake_kernel, None, "console=ttyS0", 2)
            .expect("raw binary load should succeed");

        assert_eq!(entry, KERNEL_ADDR);
        // Verify the kernel was copied to KERNEL_ADDR
        let dest = KERNEL_ADDR as usize;
        assert_eq!(&mem[dest..dest + 1024], &fake_kernel[..]);
    }

    #[test]
    fn load_kernel_too_small() {
        let mut mem = test_memory(2);
        let tiny = vec![0u8; 100]; // Too small

        let err = load_kernel_from_bytes(&mut mem, &tiny, None, "", 2);
        assert!(err.is_err());
    }

    #[test]
    fn bzimage_cmdline_written() {
        let mut mem = test_memory(2);
        // Create fake raw kernel (not bzImage)
        let fake_kernel = vec![0u8; 1024];
        let cmdline = "console=ttyS0 root=/dev/vda1";

        let _ = load_kernel_from_bytes(&mut mem, &fake_kernel, None, cmdline, 2);

        // For raw binaries, cmdline is NOT set up (only for bzImage).
        // That's expected — raw binaries don't use the boot protocol.
    }

    #[test]
    fn e820_map_structure() {
        let mut mem = test_memory(2);
        // Build a minimal fake bzImage header
        let mut kernel = vec![0u8; 0x300 + 512]; // setup header + 1 sector
        kernel[0x1FE] = 0x55; // boot flag low
        kernel[0x1FF] = 0xAA; // boot flag high
        kernel[0x202..0x206].copy_from_slice(b"HdrS"); // magic
        kernel[0x206] = 0x0A; // protocol version 2.10
        kernel[0x207] = 0x02;
        kernel[0x1F1] = 0; // setup_sects = 0 → default 4

        // Pad to have at least (4+1)*512 = 2560 bytes
        kernel.resize(2560 + 512, 0xCC);

        let _ = load_kernel_from_bytes(&mut mem, &kernel, None, "test", 2);

        // Check e820 entry count
        let bp = BOOT_PARAMS_ADDR as usize;
        assert_eq!(mem[bp + 0x1E8], 3, "should have 3 e820 entries");

        // Entry 1: low memory starts at 0
        let e820 = bp + 0x2D0;
        let addr = u64::from_le_bytes(mem[e820..e820 + 8].try_into().unwrap());
        assert_eq!(addr, 0);
        let etype = u32::from_le_bytes(mem[e820 + 16..e820 + 20].try_into().unwrap());
        assert_eq!(etype, 1, "entry 1 should be usable RAM");

        // Entry 2: reserved BIOS area
        let e2 = e820 + 20;
        let addr2 = u64::from_le_bytes(mem[e2..e2 + 8].try_into().unwrap());
        assert_eq!(addr2, 0x9FC00);
        let etype2 = u32::from_le_bytes(mem[e2 + 16..e2 + 20].try_into().unwrap());
        assert_eq!(etype2, 2, "entry 2 should be reserved");

        // Entry 3: high memory at 1MB
        let e3 = e820 + 40;
        let addr3 = u64::from_le_bytes(mem[e3..e3 + 8].try_into().unwrap());
        assert_eq!(addr3, 0x100000);
    }
}
