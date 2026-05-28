// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Filesystem service.
//!
//! Owns the in-memory ramfs and a snapshot of the read-only disk
//! (tar archive). Serves file operations to other processes via Xous
//! IPC. For data output (ls, cat), the FS service writes directly to
//! UART.
//!
//! Disk access goes through the block service (`api/block::BlockClient`)
//! — no direct MMIO or kernel-mapped disk region here. At startup we
//! read the whole tar image into [`DISK_CACHE`] via IPC; subsequent
//! `find` / `list` calls hit the cache. Once we grow past one cache
//! page the populate path can chunk reads, but the protocol is the
//! same.
//!
//! Boot parameters (set by kernel via x0):
//!   x0 = UART MMIO VA

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
// Disk cache — populated once at startup from the block service via IPC.
// ============================================================================

/// Maximum disk image size the fs will mirror locally. The QEMU test
/// disk is 10 KiB and Apple's tar payloads we ship at boot will fit
/// comfortably; anything bigger would need a real block cache rather
/// than a single static slab.
const MAX_DISK_SIZE: usize = 64 * 1024;

static mut DISK_CACHE: [u8; MAX_DISK_SIZE] = [0; MAX_DISK_SIZE];
static mut DISK_CACHE_LEN: usize = 0;

fn get_disk_archive() -> Option<tarfs::TarArchive<'static>> {
    unsafe {
        if DISK_CACHE_LEN == 0 { return None; }
        let data = &DISK_CACHE[..DISK_CACHE_LEN];
        Some(tarfs::TarArchive::new(data))
    }
}

/// Connect to the block service and slurp the entire disk into
/// [`DISK_CACHE`]. Returns the number of bytes cached on success, 0 if
/// the block service is unavailable or the disk is empty/too large.
fn populate_disk_cache_via_ipc() -> usize {
    use beetos_api_block::BlockClient;

    const CONNECT_ATTEMPTS: u32 = 32;

    let client = match BlockClient::connect_with_retries(CONNECT_ATTEMPTS) {
        Ok(c) => c,
        Err(e) => {
            let _ = write!(UartWriter,
                "[fs] no block service available (connect: {:?} after {} attempts)\n",
                e, CONNECT_ATTEMPTS);
            return 0;
        }
    };

    let info = match client.info() {
        Ok(i) => i,
        Err(e) => {
            let _ = write!(UartWriter, "[fs] block info FAILED: {:?}\n", e);
            return 0;
        }
    };

    let block_size = info.block_size as usize;
    let total_bytes = (info.capacity_blocks as usize) * block_size;
    if total_bytes == 0 { return 0; }
    if total_bytes > MAX_DISK_SIZE {
        let _ = write!(UartWriter, "[fs] disk {} B exceeds cache {} B\n",
            total_bytes, MAX_DISK_SIZE);
        return 0;
    }

    // One page is the IPC buffer; (PAGE_SIZE - BUF_DATA_OFFSET)/block_size
    // tells us how many blocks we can move per round-trip.
    let page_size = match xous::MemorySize::new(beetos::PAGE_SIZE) {
        Some(s) => s,
        None => return 0,
    };
    let buf_range = match xous::rsyscall(xous::SysCall::MapMemory(
        None, None, page_size, xous::MemoryFlags::W,
    )) {
        Ok(xous::Result::MemoryRange(r)) => r,
        _ => {
            let _ = write!(UartWriter,
                "[fs] cache buffer alloc FAILED ({} B requested)\n",
                beetos::PAGE_SIZE);
            return 0;
        }
    };

    let blocks_per_round =
        ((beetos::PAGE_SIZE - beetos_api_block::BUF_DATA_OFFSET) / block_size) as u32;
    let mut lba: u64 = 0;
    let mut bytes_done: usize = 0;
    while bytes_done < total_bytes {
        let remaining_blocks = info.capacity_blocks - lba;
        let n = (remaining_blocks as u32).min(blocks_per_round);
        if let Err(e) = client.read_blocks(lba, n, buf_range) {
            let _ = write!(UartWriter,
                "[fs] cache read FAILED at LBA {} ({} blocks, {}/{} B done): {:?}\n",
                lba, n, bytes_done, total_bytes, e);
            xous::rsyscall(xous::SysCall::UnmapMemory(buf_range)).ok();
            return 0;
        }
        let slice = unsafe {
            core::slice::from_raw_parts(buf_range.as_ptr(), buf_range.len())
        };
        let data = beetos_api_block::data(slice);
        let n_bytes = (n as usize) * block_size;
        unsafe {
            DISK_CACHE[bytes_done..bytes_done + n_bytes]
                .copy_from_slice(&data[..n_bytes]);
        }
        bytes_done += n_bytes;
        lba += n as u64;
    }

    xous::rsyscall(xous::SysCall::UnmapMemory(buf_range)).ok();
    unsafe { DISK_CACHE_LEN = total_bytes; }
    total_bytes
}

fn ipc_reply(sender: xous::MessageSender, val: usize) {
    if xous::return_scalar(sender, val).is_err() {
        puts("fs: IPC reply failed\n");
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
    unsafe {
        core::arch::asm!(
            "mov {0}, x0",
            out(reg) uart_base,
            options(nomem, nostack),
        );
        UART_BASE = uart_base;
    }

    ramfs::init();
    let _ = ramfs::mkdir("/tmp");
    let _ = ramfs::mkdir("/etc");
    let _ = ramfs::write("/etc/motd", b"Welcome to BeetOS!\n");

    let disk_size = populate_disk_cache_via_ipc();
    let _ = write!(UartWriter, "[fs] started, disk={} bytes via IPC\n", disk_size);

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
                        // Two-step page return — order matters and both
                        // pieces are non-obvious. See the comment on
                        // `return_memory_offset_valid` inside
                        // `handle_mutable_borrow` for the long version;
                        // the short version is:
                        //   1. the handler explicitly returns the page
                        //      (the kernel ships xous-rs with the
                        //      `forget-memory-messages` feature, which
                        //      strips Envelope::Drop on this build);
                        //   2. `forget(env)` is defensive — if that
                        //      feature ever flips back off, Drop would
                        //      call return_memory a second time and the
                        //      sender would observe a DoubleFree.
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
            ipc_reply(sender, result as usize);
        }
        id if id == FsOp::Ls as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_ls(path);
            ipc_reply(sender, result as usize);
        }
        id if id == FsOp::Mkdir as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_mkdir(path);
            ipc_reply(sender, result as usize);
        }
        id if id == FsOp::Remove as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_remove(path);
            ipc_reply(sender, result as usize);
        }
        id if id == FsOp::WriteShort as usize => {
            // arg1-arg2 = path (16 bytes), arg3-arg4 = content (16 bytes)
            let path_args = [scalar.arg1, scalar.arg2, 0, 0];
            let path = beetos_api_fs::unpack_path(&path_args);
            let content_args = [scalar.arg3, scalar.arg4];
            let content = unpack_short_content(&content_args);
            let result = do_write(path, content);
            ipc_reply(sender, result as usize);
        }
        id if id == FsOp::Stats as usize => {
            let (used, total, bytes) = ramfs::stats();
            let disk_size = unsafe { DISK_CACHE_LEN };
            let disk_files = get_disk_archive().map(|a| a.count()).unwrap_or(0);

            if xous::return_scalar5(sender, used, total, bytes, disk_size, disk_files).is_err() {
                puts("fs: IPC reply failed\n");
            }
        }
        id if id == FsOp::IsDir as usize => {
            let path = beetos_api_fs::unpack_path(&args);
            let result = do_is_dir(path);
            ipc_reply(sender, result as usize);
        }
        _ => {
            ipc_reply(sender, FsError::InvalidPath as usize);
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
    if is_disk_path(path) {
        let subpath = disk_subpath(path);
        return match get_disk_archive() {
            None => FsError::NotFound,
            Some(archive) => {
                if subpath.is_empty() || archive.has_dir(subpath) {
                    FsError::Ok
                } else {
                    FsError::NotFound
                }
            }
        };
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

    // Explicitly hand the page back to the sender.
    //
    // In a vanilla xous-rs build, `MessageEnvelope::Drop` would call
    // this for us as the envelope goes out of scope. BeetOS's kernel,
    // however, depends on xous-rs with the `forget-memory-messages`
    // feature enabled (see xous/kernel/Cargo.toml), and Cargo's feature
    // unification turns that feature on for every reverse-dep in the
    // workspace — including this binary. The result: `Drop for
    // Envelope` is *not compiled in*, so without this explicit call
    // the borrowed page is never returned and the sender blocks
    // forever waiting for it. (We learned that the hard way when the
    // shell self-test landed against `os/block`.)
    //
    // Any new IPC server in this tree that handles a MutableBorrow
    // *must* call this. See also the matching `forget(env)` in the
    // receive loop above for the second half of the pattern.
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
