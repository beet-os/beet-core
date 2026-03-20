// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS coreutils — multi-applet binary (like busybox).
//!
//! The kernel spawns this binary with an applet name (ls, cat, mkdir, rm, write,
//! blkinfo, mem, info, pid). The applet name is passed via x3. Command arguments
//! are passed via x4-x7 (packed path, up to 32 bytes).
//!
//! Each applet connects to the FS service via IPC and writes output to UART.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use beetos_api_fs::{FsError, FsOp, FS_SID, MAX_PATH_LEN};

// ============================================================================
// UART output
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 { return; }
        let base = UART_BASE;
        while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        if c == b'\n' {
            core::ptr::write_volatile((base + UART_DR) as *mut u32, b'\r' as u32);
            while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        }
        core::ptr::write_volatile((base + UART_DR) as *mut u32, c as u32);
    }
}

fn puts(s: &str) { for b in s.bytes() { putc(b); } }

struct UartWriter;
impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        puts(s);
        Ok(())
    }
}

// ============================================================================
// FS service connection
// ============================================================================

static mut FS_CID: u32 = 0;

fn get_fs_cid() -> u32 {
    unsafe {
        if FS_CID != 0 { return FS_CID; }
        let sid = xous::SID::from_array(FS_SID);
        match xous::rsyscall(xous::SysCall::Connect(sid)) {
            Ok(xous::Result::ConnectionID(cid)) => {
                FS_CID = cid;
                cid
            }
            _ => 0,
        }
    }
}

// ============================================================================
// Entry point
// ============================================================================

/// Boot parameters from kernel:
///   x0 = UART VA
///   x1 = unused (disk VA — only for shell/fs)
///   x2 = unused (disk size — only for shell/fs)
///   x3 = applet ID (0=ls, 1=cat, 2=mkdir, 3=rm, 4=write, 5=blkinfo, 6=mem, 7=info, 8=pid)
///   x4-x7 = argument (packed path, up to 32 bytes)
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let uart_base: usize;
    let applet_id: usize;
    let arg1: usize;
    let arg2: usize;
    let arg3: usize;
    let arg4: usize;
    unsafe {
        core::arch::asm!(
            "mov {0}, x0",
            "mov {1}, x3",
            "mov {2}, x4",
            "mov {3}, x5",
            "mov {4}, x6",
            "mov {5}, x7",
            out(reg) uart_base,
            out(reg) applet_id,
            out(reg) arg1,
            out(reg) arg2,
            out(reg) arg3,
            out(reg) arg4,
            options(nomem, nostack),
        );
        UART_BASE = uart_base;
    }

    let args = [arg1, arg2, arg3, arg4];

    let exit_code = match applet_id {
        0 => cmd_ls(&args),
        1 => cmd_cat(&args),
        2 => cmd_mkdir(&args),
        3 => cmd_rm(&args),
        4 => cmd_write(&args),
        5 => cmd_blkinfo(),
        6 => cmd_mem(),
        7 => cmd_info(),
        8 => cmd_pid(),
        _ => {
            let _ = write!(UartWriter, "coreutils: unknown applet {}\n", applet_id);
            1
        }
    };

    // Exit
    xous::rsyscall(xous::SysCall::TerminateProcess(exit_code as u32)).ok();

    loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }; }
}

// ============================================================================
// Applets
// ============================================================================

fn cmd_ls(args: &[usize; 4]) -> usize {
    let path = beetos_api_fs::unpack_path(args);
    let path = if path.is_empty() { "/" } else { path };

    let cid = get_fs_cid();
    if cid == 0 { puts("ls: fs service not available\n"); return 1; }

    // Allocate a page for the response
    let page = match xous::map_memory(None, None, 16384, xous::MemoryFlags::W) {
        Ok(range) => range,
        Err(_) => { puts("ls: alloc failed\n"); return 1; }
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(page.as_mut_ptr() as *mut u8, 16384) };

    // Write path into first 32 bytes
    beetos_api_fs::write_path_to_buf(buf, path);

    // Send MutableBorrow to FS service
    let mem_msg = xous::MemoryMessage {
        id: xous::MessageId::from(FsOp::List as usize),
        buf: page,
        offset: None,
        valid: None,
    };
    let _ = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::MutableBorrow(mem_msg),
    ));

    // Read result: first 8 bytes = data length
    let data_len = usize::from_le_bytes(buf[..8].try_into().unwrap_or([0; 8]));
    let data = &buf[MAX_PATH_LEN..MAX_PATH_LEN + data_len.min(16384 - MAX_PATH_LEN)];

    // Parse and display: "type name size\n" per line
    for line in core::str::from_utf8(data).unwrap_or("").lines() {
        if line.is_empty() { continue; }
        let mut parts = line.splitn(3, ' ');
        let ftype = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        let size = parts.next().unwrap_or("0");
        if ftype == "d" {
            let _ = write!(UartWriter, "  {}/\n", name);
        } else {
            let _ = write!(UartWriter, "  {} ({} bytes)\n", name, size);
        }
    }

    // Free the page
    xous::unmap_memory(page).ok();
    0
}

fn cmd_cat(args: &[usize; 4]) -> usize {
    let path = beetos_api_fs::unpack_path(args);
    if path.is_empty() {
        puts("usage: cat <path>\n");
        return 1;
    }

    let cid = get_fs_cid();
    if cid == 0 { puts("cat: fs service not available\n"); return 1; }

    let page = match xous::map_memory(None, None, 16384, xous::MemoryFlags::W) {
        Ok(range) => range,
        Err(_) => { puts("cat: alloc failed\n"); return 1; }
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(page.as_mut_ptr() as *mut u8, 16384) };

    beetos_api_fs::write_path_to_buf(buf, path);

    let mem_msg = xous::MemoryMessage {
        id: xous::MessageId::from(FsOp::Read as usize),
        buf: page,
        offset: None,
        valid: None,
    };
    let _ = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::MutableBorrow(mem_msg),
    ));

    let data_len = usize::from_le_bytes(buf[..8].try_into().unwrap_or([0; 8]));
    if data_len == 0 {
        let _ = write!(UartWriter, "cat: {}: not found\n", path);
        xous::unmap_memory(page).ok();
        return 1;
    }

    let data = &buf[MAX_PATH_LEN..MAX_PATH_LEN + data_len.min(16384 - MAX_PATH_LEN)];
    match core::str::from_utf8(data) {
        Ok(text) => {
            puts(text);
            if !text.ends_with('\n') { putc(b'\n'); }
        }
        Err(_) => {
            let _ = write!(UartWriter, "<binary: {} bytes>\n", data_len);
        }
    }

    xous::unmap_memory(page).ok();
    0
}

fn cmd_mkdir(args: &[usize; 4]) -> usize {
    let path = beetos_api_fs::unpack_path(args);
    if path.is_empty() {
        puts("usage: mkdir <path>\n");
        return 1;
    }

    let cid = get_fs_cid();
    if cid == 0 { puts("mkdir: fs service not available\n"); return 1; }

    let result = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: FsOp::Mkdir as usize,
            arg1: args[0], arg2: args[1], arg3: args[2], arg4: args[3],
        }),
    ));

    match result {
        Ok(xous::Result::Scalar1(code)) if code == FsError::Ok as usize => 0,
        Ok(xous::Result::Scalar1(code)) if code == FsError::AlreadyExists as usize => {
            let _ = write!(UartWriter, "mkdir: {}: already exists\n", path);
            1
        }
        Ok(xous::Result::Scalar1(code)) if code == FsError::ReadOnly as usize => {
            let _ = write!(UartWriter, "mkdir: {}: read-only filesystem\n", path);
            1
        }
        _ => {
            let _ = write!(UartWriter, "mkdir: error\n");
            1
        }
    }
}

fn cmd_rm(args: &[usize; 4]) -> usize {
    let path = beetos_api_fs::unpack_path(args);
    if path.is_empty() {
        puts("usage: rm <path>\n");
        return 1;
    }

    let cid = get_fs_cid();
    if cid == 0 { puts("rm: fs service not available\n"); return 1; }

    let result = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: FsOp::Remove as usize,
            arg1: args[0], arg2: args[1], arg3: args[2], arg4: args[3],
        }),
    ));

    match result {
        Ok(xous::Result::Scalar1(code)) if code == FsError::Ok as usize => 0,
        Ok(xous::Result::Scalar1(code)) if code == FsError::NotFound as usize => {
            let _ = write!(UartWriter, "rm: {}: not found\n", path);
            1
        }
        Ok(xous::Result::Scalar1(code)) if code == FsError::NotEmpty as usize => {
            let _ = write!(UartWriter, "rm: {}: directory not empty\n", path);
            1
        }
        Ok(xous::Result::Scalar1(code)) if code == FsError::ReadOnly as usize => {
            let _ = write!(UartWriter, "rm: {}: read-only filesystem\n", path);
            1
        }
        _ => {
            let _ = write!(UartWriter, "rm: error\n");
            1
        }
    }
}

fn cmd_write(args: &[usize; 4]) -> usize {
    // For write, args contain the path. The content needs to come from
    // somewhere — for now, we handle this in the shell which has the full
    // command line. The shell sends Write ops directly to the FS service.
    let path = beetos_api_fs::unpack_path(args);
    let _ = write!(UartWriter, "write: use shell builtin for now (path={})\n", path);
    1
}

fn cmd_blkinfo() -> usize {
    let cid = get_fs_cid();
    if cid == 0 { puts("blkinfo: fs service not available\n"); return 1; }

    let result = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: FsOp::Stats as usize,
            arg1: 0, arg2: 0, arg3: 0, arg4: 0,
        }),
    ));

    match result {
        Ok(xous::Result::Scalar5(_used, _total, _bytes, disk_size, disk_files)) => {
            if disk_size == 0 {
                puts("No block device\n");
            } else {
                let _ = write!(UartWriter, "Block device: {} bytes\n", disk_size);
                puts("Mounted at: /disk/ (read-only, tar)\n");
                let _ = write!(UartWriter, "Files: {}\n", disk_files);
            }
            0
        }
        _ => {
            puts("blkinfo: error\n");
            1
        }
    }
}

fn cmd_mem() -> usize {
    let cid = get_fs_cid();
    if cid == 0 { puts("mem: fs service not available\n"); return 1; }

    let result = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: FsOp::Stats as usize,
            arg1: 0, arg2: 0, arg3: 0, arg4: 0,
        }),
    ));

    match result {
        Ok(xous::Result::Scalar5(used, total, bytes, disk_size, _disk_files)) => {
            puts("RAM filesystem:\n");
            let _ = write!(UartWriter, "  Files: {}/{}\n", used, total);
            let _ = write!(UartWriter, "  Used:  {} bytes\n", bytes);
            if disk_size > 0 {
                let _ = write!(UartWriter, "Disk: {} bytes\n", disk_size);
            }
            0
        }
        _ => {
            puts("mem: error\n");
            1
        }
    }
}

fn cmd_info() -> usize {
    puts("BeetOS v0.1.0\n");
    puts("Kernel: Xous microkernel (AArch64)\n");
    puts("Platform: QEMU virt\n");
    let _ = write!(UartWriter, "Page size: {} bytes\n", 16384);
    puts("Shell: userspace process (EL0)\n");
    0
}

fn cmd_pid() -> usize {
    match xous::rsyscall(xous::SysCall::GetProcessId) {
        Ok(xous::Result::Scalar1(pid)) => {
            let _ = write!(UartWriter, "PID: {}\n", pid);
            0
        }
        _ => {
            puts("pid: syscall failed\n");
            1
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in coreutils!\n");
    loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }; }
}
