// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Filesystem service.
//!
//! Owns the in-memory ramfs and the read-only disk (tar archive).
//! Serves file operations to other processes via Xous IPC.
//! For data output (ls, cat), the FS service writes directly to UART.
//!
//! Boot parameters (set by kernel via x0-x2):
//!   x0 = UART MMIO VA
//!   x1 = disk data VA (0 if no disk)
//!   x2 = disk data size in bytes (0 if no disk)

#![no_std]
#![no_main]

mod ramfs;
mod tarfs;

use core::fmt::Write;
use core::panic::PanicInfo;

use beetos_api_fs::{BUF_STATUS_OFFSET, BUF_TEXT_OFFSET, FsError, FsOp, FS_SID};

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
// Disk data
// ============================================================================

static mut DISK_BASE: usize = 0;
static mut DISK_SIZE: usize = 0;

fn get_disk_archive() -> Option<tarfs::TarArchive<'static>> {
    unsafe {
        if DISK_SIZE == 0 || DISK_BASE == 0 { return None; }
        let data = core::slice::from_raw_parts(DISK_BASE as *const u8, DISK_SIZE);
        Some(tarfs::TarArchive::new(data))
    }
}

fn is_disk_path(path: &str) -> bool {
    let p = path.strip_prefix('/').unwrap_or(path);
    p == "disk" || p.starts_with("disk/")
}

fn disk_subpath(path: &str) -> &str {
    let p = path.strip_prefix('/').unwrap_or(path);
    p.strip_prefix("disk/").unwrap_or(p.strip_prefix("disk").unwrap_or(p))
}

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let uart_base: usize;
    let disk_base: usize;
    let disk_size: usize;
    unsafe {
        core::arch::asm!(
            "mov {0}, x0", "mov {1}, x1", "mov {2}, x2",
            out(reg) uart_base, out(reg) disk_base, out(reg) disk_size,
            options(nomem, nostack),
        );
        UART_BASE = uart_base;
        DISK_BASE = disk_base;
        DISK_SIZE = disk_size;
    }

    ramfs::init();
    let _ = ramfs::mkdir("/tmp");
    let _ = ramfs::mkdir("/etc");
    let _ = ramfs::write("/etc/motd", b"Welcome to BeetOS!\n");

    let _ = write!(UartWriter, "[fs] started, disk={} bytes\n", disk_size);

    let sid = xous::SID::from_array(FS_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));
        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                match &env.body {
                    xous::Message::BlockingScalar(scalar) => {
                        handle_blocking_scalar(env.sender, *scalar);
                    }
                    xous::Message::MutableBorrow(mem) => {
                        handle_mutable_borrow(env.sender, mem);
                        core::mem::forget(env);
                    }
                    _ => {}
                }
            }
            _ => { xous::yield_slice(); }
        }
    }
}

// ============================================================================
// Message handlers
// ============================================================================

fn handle_blocking_scalar(sender: xous::MessageSender, scalar: xous::ScalarMessage) {
    let args = [scalar.arg1, scalar.arg2, scalar.arg3, scalar.arg4];

    match scalar.id {
        id if id == FsOp::Cat as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_cat(path);
            xous::return_scalar(sender, result as usize).ok();
        }
        id if id == FsOp::Ls as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_ls(path);
            xous::return_scalar(sender, result as usize).ok();
        }
        id if id == FsOp::Mkdir as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_mkdir(path);
            xous::return_scalar(sender, result as usize).ok();
        }
        id if id == FsOp::Remove as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_remove(path);
            xous::return_scalar(sender, result as usize).ok();
        }
        id if id == FsOp::WriteShort as usize => {
            // arg1-arg2 = path (16 bytes), arg3-arg4 = content (16 bytes)
            let path_args = [scalar.arg1, scalar.arg2, 0, 0];
            let path = beetos_api_fs::unpack_path(&path_args);
            let content_args = [scalar.arg3, scalar.arg4];
            let content = unpack_short_content(&content_args);
            let result = do_write(path, content);
            xous::return_scalar(sender, result as usize).ok();
        }
        id if id == FsOp::Stats as usize => {
            let (used, total, bytes) = ramfs::stats();
            let disk_size = unsafe { DISK_SIZE };
            let disk_files = get_disk_archive().map(|a| a.count()).unwrap_or(0);
            xous::return_scalar5(sender, used, total, bytes, disk_size, disk_files).ok();
        }
        id if id == FsOp::IsDir as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_is_dir(path);
            xous::return_scalar(sender, result as usize).ok();
        }
        _ => {
            xous::return_scalar(sender, FsError::InvalidPath as usize).ok();
        }
    }
}

/// Unpack up to 16 bytes of content from 2 usize values.
fn unpack_short_content(args: &[usize; 2]) -> &[u8] {
    let word_size = core::mem::size_of::<usize>();
    let ptr = args.as_ptr() as *const u8;
    let max_len = 2 * word_size;
    let bytes = unsafe { core::slice::from_raw_parts(ptr, max_len) };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(max_len);
    &bytes[..len]
}

// ============================================================================
// FS operations
// ============================================================================

fn do_cat(path: &str) -> FsError {
    // Try disk path first
    if is_disk_path(path) {
        if let Some(archive) = get_disk_archive() {
            let subpath = disk_subpath(path);
            if let Some(data) = archive.find(subpath) {
                match core::str::from_utf8(data) {
                    Ok(text) => {
                        puts(text);
                        if !text.ends_with('\n') { putc(b'\n'); }
                    }
                    Err(_) => {
                        let _ = write!(UartWriter, "<binary: {} bytes>\n", data.len());
                    }
                }
                return FsError::Ok;
            }
        }
        return FsError::NotFound;
    }

    match ramfs::read(path) {
        Ok(data) => {
            match core::str::from_utf8(data) {
                Ok(text) => {
                    puts(text);
                    if !text.ends_with('\n') { putc(b'\n'); }
                }
                Err(_) => {
                    let _ = write!(UartWriter, "<binary: {} bytes>\n", data.len());
                }
            }
            FsError::Ok
        }
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::IsDirectory) => FsError::IsDirectory,
        Err(_) => FsError::InvalidPath,
    }
}

fn do_is_dir(path: &str) -> FsError {
    // /disk itself and any disk subpath: directory if disk is present.
    if is_disk_path(path) {
        return if get_disk_archive().is_some() { FsError::Ok } else { FsError::NotFound };
    }

    // Use list with a no-op callback — it returns NotFound or NotDirectory for non-dirs.
    match ramfs::list(path, |_name, _is_dir, _size| {}) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::NotDirectory) => FsError::NotDirectory,
        Err(_) => FsError::InvalidPath,
    }
}

fn do_ls(path: &str) -> FsError {
    let is_root = {
        let p = path.strip_prefix('/').unwrap_or(path);
        p.is_empty()
    };

    // Show virtual "disk/" in root listing
    if is_root && get_disk_archive().is_some() {
        puts("  disk/  (block device)\n");
    }

    // Disk path
    if is_disk_path(path) {
        if let Some(archive) = get_disk_archive() {
            let subpath = disk_subpath(path);
            archive.list(subpath, |name, is_dir, size| {
                if is_dir {
                    let _ = write!(UartWriter, "  {}/\n", name);
                } else {
                    let _ = write!(UartWriter, "  {} ({} bytes)\n", name, size);
                }
            });
            return FsError::Ok;
        }
        return FsError::NotFound;
    }

    // Ramfs
    match ramfs::list(path, |name, is_dir, size| {
        if is_dir {
            let _ = write!(UartWriter, "  {}/\n", name);
        } else {
            let _ = write!(UartWriter, "  {} ({} bytes)\n", name, size);
        }
    }) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::NotDirectory) => FsError::NotDirectory,
        Err(_) => FsError::InvalidPath,
    }
}

fn do_mkdir(path: &str) -> FsError {
    if is_disk_path(path) { return FsError::ReadOnly; }
    match ramfs::mkdir(path) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::AlreadyExists) => FsError::AlreadyExists,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(_) => FsError::NoSpace,
    }
}

fn do_remove(path: &str) -> FsError {
    if is_disk_path(path) { return FsError::ReadOnly; }
    match ramfs::remove(path) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::NotEmpty) => FsError::NotEmpty,
        Err(_) => FsError::InvalidPath,
    }
}

fn do_write(path: &str, content: &[u8]) -> FsError {
    if is_disk_path(path) { return FsError::ReadOnly; }
    match ramfs::write(path, content) {
        Ok(()) => {
            let _ = write!(UartWriter, "wrote {} bytes to {}\n", content.len(), path);
            FsError::Ok
        }
        Err(ramfs::FsError::IsDirectory) => FsError::IsDirectory,
        Err(_) => FsError::NoSpace,
    }
}

// ============================================================================
// Buffer-based output (MutableBorrow ops)
// ============================================================================

struct BufWrite<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Write for BufWrite<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.pos + 1 < self.buf.len() {
                self.buf[self.pos] = b;
                self.pos += 1;
            }
        }
        Ok(())
    }
}

fn handle_mutable_borrow(sender: xous::MessageSender, mem: &xous::MemoryMessage) {
    let op = mem.id;
    let full_len = mem.buf.len();

    // Copy path to stack before any mutable access to the buffer.
    let mut path_copy = [0u8; 32];
    {
        let src = unsafe { core::slice::from_raw_parts(mem.buf.as_ptr(), full_len.min(32)) };
        path_copy[..src.len()].copy_from_slice(src);
    }
    let path_len = path_copy.iter().position(|&b| b == 0).unwrap_or(32);
    let path = core::str::from_utf8(&path_copy[..path_len]).unwrap_or("");

    // Determine status and write output into the text area directly via raw pointer.
    let status: FsError = if full_len > BUF_TEXT_OFFSET {
        let text_ptr = unsafe { mem.buf.as_mut_ptr().add(BUF_TEXT_OFFSET) };
        let text_len = full_len - BUF_TEXT_OFFSET;
        // Zero text area.
        unsafe { core::ptr::write_bytes(text_ptr, 0, text_len); }
        let text_area = unsafe { core::slice::from_raw_parts_mut(text_ptr, text_len) };
        if op == FsOp::LsBuf as usize {
            do_ls_buf(path, text_area)
        } else if op == FsOp::CatBuf as usize {
            do_cat_buf(path, text_area)
        } else {
            FsError::InvalidPath
        }
    } else {
        FsError::InvalidPath
    };

    // Write status byte at BUF_STATUS_OFFSET via raw pointer (avoids slice aliasing issues).
    unsafe {
        mem.buf.as_mut_ptr().add(BUF_STATUS_OFFSET).write_volatile(status as u8);
    }

    xous::return_memory_offset_valid(sender, mem.buf, None, None).ok();
}

fn do_ls_buf(path: &str, output: &mut [u8]) -> FsError {
    let mut w = BufWrite { buf: output, pos: 0 };

    let is_root = {
        let p = path.strip_prefix('/').unwrap_or(path);
        p.is_empty()
    };

    if is_root && get_disk_archive().is_some() {
        let _ = write!(w, "  disk/  (block device)\n");
    }

    if is_disk_path(path) {
        if let Some(archive) = get_disk_archive() {
            let subpath = disk_subpath(path);
            archive.list(subpath, |name, is_dir, size| {
                if is_dir {
                    let _ = write!(w, "  {}/\n", name);
                } else {
                    let _ = write!(w, "  {} ({} bytes)\n", name, size);
                }
            });
            return FsError::Ok;
        }
        return FsError::NotFound;
    }

    match ramfs::list(path, |name, is_dir, size| {
        if is_dir {
            let _ = write!(w, "  {}/\n", name);
        } else {
            let _ = write!(w, "  {} ({} bytes)\n", name, size);
        }
    }) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::NotDirectory) => FsError::NotDirectory,
        Err(_) => FsError::InvalidPath,
    }
}

fn do_cat_buf(path: &str, output: &mut [u8]) -> FsError {
    let mut w = BufWrite { buf: output, pos: 0 };

    if is_disk_path(path) {
        if let Some(archive) = get_disk_archive() {
            let subpath = disk_subpath(path);
            if let Some(data) = archive.find(subpath) {
                match core::str::from_utf8(data) {
                    Ok(text) => {
                        let _ = write!(w, "{}", text);
                        if !text.ends_with('\n') {
                            let _ = write!(w, "\n");
                        }
                    }
                    Err(_) => {
                        let _ = write!(w, "<binary: {} bytes>\n", data.len());
                    }
                }
                return FsError::Ok;
            }
        }
        return FsError::NotFound;
    }

    match ramfs::read(path) {
        Ok(data) => {
            match core::str::from_utf8(data) {
                Ok(text) => {
                    let _ = write!(w, "{}", text);
                    if !text.ends_with('\n') {
                        let _ = write!(w, "\n");
                    }
                }
                Err(_) => {
                    let _ = write!(w, "<binary: {} bytes>\n", data.len());
                }
            }
            FsError::Ok
        }
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::IsDirectory) => FsError::IsDirectory,
        Err(_) => FsError::InvalidPath,
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in fs!\n");
    loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }; }
}
