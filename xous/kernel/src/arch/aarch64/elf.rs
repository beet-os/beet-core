// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ELF64 loader for AArch64 binaries.
//!
//! Loads position-independent executables (PIE/ET_DYN) with ASLR.
//! Validates W^X: rejects binaries with simultaneously writable+executable segments.

use xous::{Error, MemoryFlags, MemoryRange, PID};

use crate::arch::mem::MemoryMapping;
use crate::mem::MemoryManager;

// ELF64 constants
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_AARCH64: u16 = 183;
const ET_DYN: u16 = 3; // PIE executable
const ET_EXEC: u16 = 2; // Static executable

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_GNU_RELRO: u32 = 0x6474_E552;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

const R_AARCH64_RELATIVE: u32 = 1027;

/// ELF64 file header.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// ELF64 program header.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

/// ELF64 dynamic table entry.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}

/// ELF64 relocation entry (with addend).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

/// Result of loading an ELF binary.
pub struct ElfLoadResult {
    /// Entry point address (with ASLR slide applied).
    pub entry_point: usize,
    /// ASLR slide applied to the binary.
    pub aslr_slide: usize,
}

/// Load an ELF64 binary into a process's address space.
///
/// # Safety
///
/// The `elf_data` range must contain a valid ELF64 binary.
pub unsafe fn load_elf(
    elf_data: MemoryRange,
    pid: PID,
    mapping: &mut MemoryMapping,
    mm: &mut MemoryManager,
) -> Result<ElfLoadResult, Error> {
    let base = elf_data.as_ptr() as *const u8;
    let len = elf_data.len();

    if len < core::mem::size_of::<Elf64Header>() {
        return Err(Error::BadAddress);
    }

    // Use read_unaligned throughout — ELF data may not be aligned to struct requirements
    let header = core::ptr::read_unaligned(base as *const Elf64Header);

    // Validate ELF magic
    if header.e_ident[0..4] != ELF_MAGIC {
        return Err(Error::BadAddress);
    }
    if header.e_ident[4] != ELFCLASS64 {
        return Err(Error::BadAddress);
    }
    if header.e_ident[5] != ELFDATA2LSB {
        return Err(Error::BadAddress);
    }
    if header.e_machine != EM_AARCH64 {
        return Err(Error::BadAddress);
    }
    if header.e_type != ET_DYN && header.e_type != ET_EXEC {
        return Err(Error::BadAddress);
    }

    // Calculate ASLR slide for PIE executables
    let aslr_slide = if header.e_type == ET_DYN {
        // Generate a random slide aligned to PAGE_SIZE
        let random = crate::arch::rand::get_u32() as usize;
        let range = beetos::ASLR_END - beetos::ASLR_START;
        let slide = beetos::ASLR_START + (random % (range / beetos::PAGE_SIZE)) * beetos::PAGE_SIZE;
        slide
    } else {
        0
    };

    // Validate W^X: no segment should be both writable and executable
    let phdr_base = base.add(header.e_phoff as usize) as *const Elf64Phdr;
    for i in 0..header.e_phnum as usize {
        let phdr = core::ptr::read_unaligned(phdr_base.add(i));
        if phdr.p_type == PT_LOAD {
            if (phdr.p_flags & PF_W != 0) && (phdr.p_flags & PF_X != 0) {
                return Err(Error::BadAddress); // W^X violation
            }
        }
    }

    // Load PT_LOAD segments
    for i in 0..header.e_phnum as usize {
        let phdr = core::ptr::read_unaligned(phdr_base.add(i));
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize + aslr_slide;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;

        // Convert ELF flags to Xous MemoryFlags
        let mut flags = MemoryFlags::empty();
        // Note: There is no MemoryFlags::R — on ARM, all mapped pages are readable.
        // PF_R is implicit; we only set W and X flags.
        if phdr.p_flags & PF_W != 0 { flags |= MemoryFlags::W; }
        if phdr.p_flags & PF_X != 0 { flags |= MemoryFlags::X; }

        // Map pages for this segment
        let page_start = vaddr & !(beetos::PAGE_SIZE - 1);
        let page_end = (vaddr + memsz + beetos::PAGE_SIZE - 1) & !(beetos::PAGE_SIZE - 1);
        let mut offset = 0;

        while page_start + offset < page_end {
            let (page_phys, _zeroed) = mm.alloc_range(1, pid).map_err(|_| Error::OutOfMemory)?;
            let page_virt = (page_start + offset) as *mut usize;
            let page_va = page_start + offset;

            // Zero the page via identity map (PA = VA for kernel blocks).
            core::ptr::write_bytes(page_phys as *mut u8, 0, beetos::PAGE_SIZE);

            // Copy file data if this page overlaps with filesz
            if page_va < vaddr + filesz {
                let src_start = if page_va >= vaddr {
                    phdr.p_offset as usize + (page_va - vaddr)
                } else {
                    phdr.p_offset as usize
                };
                let dst_offset = if page_va < vaddr { vaddr - page_va } else { 0 };
                let copy_len = core::cmp::min(
                    beetos::PAGE_SIZE - dst_offset,
                    (vaddr + filesz).saturating_sub(page_va + dst_offset),
                );
                if copy_len > 0 && src_start < len {
                    let actual_copy = core::cmp::min(copy_len, len - src_start);
                    core::ptr::copy_nonoverlapping(
                        base.add(src_start),
                        (page_phys as *mut u8).add(dst_offset),
                        actual_copy,
                    );
                }
            }

            mapping.map_page(mm, page_phys, page_virt, flags, true)?;

            offset += beetos::PAGE_SIZE;
        }
    }

    // Apply relocations for PIE
    if header.e_type == ET_DYN {
        apply_relocations(base, &header, phdr_base, aslr_slide)?;
    }

    Ok(ElfLoadResult {
        entry_point: header.e_entry as usize + aslr_slide,
        aslr_slide,
    })
}

/// Apply R_AARCH64_RELATIVE relocations.
unsafe fn apply_relocations(
    base: *const u8,
    header: &Elf64Header,
    phdr_base: *const Elf64Phdr,
    slide: usize,
) -> Result<(), Error> {
    // Find PT_DYNAMIC
    let mut rela_addr: Option<u64> = None;
    let mut rela_size: u64 = 0;
    let mut rela_ent: u64 = 0;

    for i in 0..header.e_phnum as usize {
        let phdr = core::ptr::read_unaligned(phdr_base.add(i));
        if phdr.p_type == PT_DYNAMIC {
            let dyn_base = base.add(phdr.p_offset as usize) as *const Elf64Dyn;
            let count = phdr.p_filesz as usize / core::mem::size_of::<Elf64Dyn>();
            for j in 0..count {
                let dyn_entry = core::ptr::read_unaligned(dyn_base.add(j));
                match dyn_entry.d_tag {
                    7 => rela_addr = Some(dyn_entry.d_val),    // DT_RELA
                    8 => rela_size = dyn_entry.d_val,           // DT_RELASZ
                    9 => rela_ent = dyn_entry.d_val,            // DT_RELAENT
                    0 => break,                                 // DT_NULL
                    _ => {}
                }
            }
        }
    }

    if let Some(rela_offset) = rela_addr {
        if rela_ent == 0 {
            return Ok(());
        }
        let count = rela_size / rela_ent;
        let rela_base = (rela_offset as usize + slide) as *const Elf64Rela;
        for i in 0..count as usize {
            let rela = core::ptr::read_unaligned(rela_base.add(i));
            let rtype = (rela.r_info & 0xFFFF_FFFF) as u32;
            if rtype == R_AARCH64_RELATIVE {
                let target = (rela.r_offset as usize + slide) as *mut u64;
                let value = (rela.r_addend as usize).wrapping_add(slide) as u64;
                core::ptr::write_unaligned(target, value);
            }
        }
    }

    Ok(())
}
