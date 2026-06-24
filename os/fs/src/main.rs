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

use beetos::pl011::{putc, puts, Writer as UartWriter};
use beetos_api_fs::{BUF_STATUS_OFFSET, BUF_TEXT_OFFSET, FsError, FsOp, FS_SID};

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

    // Trust-but-verify the geometry the block service reports. Today
    // block is a sibling service we implicitly trust, but a bad value
    // here turns into one of three subtle failure modes downstream:
    //   * block_size == 0          → divide-by-zero computing blocks_per_round
    //   * block_size > IPC payload → blocks_per_round = 0 → infinite loop
    //   * cap × bs overflows usize → loop invariants break, OOB write
    // All three are catastrophic; the explicit checks below turn each
    // into a clean "return 0 with a diagnostic" instead.
    let block_size = info.block_size as usize;
    const MAX_BLOCK_SIZE: usize =
        beetos::PAGE_SIZE - beetos_api_block::BUF_DATA_OFFSET;
    if block_size == 0 || block_size > MAX_BLOCK_SIZE {
        let _ = write!(UartWriter,
            "[fs] bad block_size {} from block service (expected 1..={})\n",
            block_size, MAX_BLOCK_SIZE);
        return 0;
    }
    let total_bytes = match (info.capacity_blocks as usize).checked_mul(block_size) {
        Some(b) => b,
        None => {
            let _ = write!(UartWriter,
                "[fs] geometry overflow: {} blocks × {} B\n",
                info.capacity_blocks, block_size);
            return 0;
        }
    };
    if total_bytes == 0 { return 0; }
    // The fs only parses the tar at the head of the disk; the tail
    // (api/cryptblock's encrypted area, and any future partitions) is
    // none of its business. Cache just the head when the device is
    // bigger than the cache — the tar's end-of-archive zero blocks
    // land inside the cached window as long as the tar itself fits.
    let total_bytes = if total_bytes > MAX_DISK_SIZE {
        let _ = write!(UartWriter, "[fs] disk {} B, caching first {} B (tar head)\n",
            total_bytes, MAX_DISK_SIZE);
        MAX_DISK_SIZE
    } else {
        total_bytes
    };

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
        // Also cap by what's left in the (possibly clamped) cache
        // window, or the final round overruns DISK_CACHE.
        let remaining_cache = ((total_bytes - bytes_done) / block_size) as u64;
        let n = (remaining_blocks.min(remaining_cache) as u32).min(blocks_per_round);
        if n == 0 { break; }
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
// Encrypted /data area (api/cryptblock over the block service)
// ============================================================================
//
// One file = one sealed slot (flat namespace, ≤ DATA_NAME_MAX-byte
// names, ≤ ~448-byte contents). No central directory: lookup scans the
// slots, so every write/remove is a single atomic sealed-sector update
// and there's no metadata to corrupt. The session key lives here, in
// fs process memory, between CryptOpen and CryptLock.

use beetos_api_cryptblock::{CryptDisk, CryptError, CRYPT_SLOTS, SLOT_PAYLOAD};

static mut CRYPT: Option<CryptDisk> = None;
/// One persistent IPC page for CryptDisk ↔ block traffic, allocated on
/// first unlock (avoids a map/unmap pair per operation).
static mut CRYPT_BUF: Option<xous::MemoryRange> = None;

fn crypt_session() -> Option<CryptDisk> {
    unsafe { *core::ptr::addr_of!(CRYPT) }
}

fn crypt_buf() -> Option<xous::MemoryRange> {
    unsafe {
        if let Some(b) = *core::ptr::addr_of!(CRYPT_BUF) {
            return Some(b);
        }
        let page = xous::MemorySize::new(beetos::PAGE_SIZE)?;
        match xous::rsyscall(xous::SysCall::MapMemory(
            None, None, page, xous::MemoryFlags::W,
        )) {
            Ok(xous::Result::MemoryRange(r)) => {
                CRYPT_BUF = Some(r);
                Some(r)
            }
            _ => None,
        }
    }
}

fn is_data_path(path: &str) -> bool {
    let p = path.strip_prefix('/').unwrap_or(path);
    p == "data" || p.starts_with("data/")
}

fn data_subpath(path: &str) -> &str {
    let p = path.strip_prefix('/').unwrap_or(path);
    p.strip_prefix("data/").unwrap_or(p.strip_prefix("data").unwrap_or(p))
}

fn map_crypt_err(e: CryptError) -> FsError {
    match e {
        CryptError::NotFormatted => FsError::NotFound,
        CryptError::BadPassphrase => FsError::Locked,
        CryptError::Corrupt => FsError::Corrupt,
        CryptError::Empty => FsError::NotFound,
        CryptError::BadArgument => FsError::InvalidPath,
        CryptError::Io => FsError::NoSpace,
    }
}

/// Find the slot holding `name`. Returns `(slot, data_len)`.
fn crypt_find(disk: &CryptDisk, buf: xous::MemoryRange, name: &str) -> Option<(u64, usize)> {
    for slot in 0..CRYPT_SLOTS {
        if let Ok(payload) = disk.read_slot(buf, slot) {
            if let Some((entry_name, data)) = beetos_api_fs::unpack_data_entry(&payload) {
                if entry_name == name {
                    return Some((slot, data.len()));
                }
            }
        }
    }
    None
}

/// First never-written slot, for new files.
fn crypt_find_free(disk: &CryptDisk, buf: xous::MemoryRange) -> Option<u64> {
    for slot in 0..CRYPT_SLOTS {
        match disk.read_slot(buf, slot) {
            Err(CryptError::Empty) => return Some(slot),
            // Unparseable-but-sealed slots stay reserved; Corrupt ones
            // are not silently reused either (the operator should see
            // them via cat → Corrupt, then rm explicitly).
            _ => {}
        }
    }
    None
}

/// Is there a valid header on disk right now?
///
/// Used to gate `cryptfmt` against a destructive re-format by an
/// untrusted IPC sender: once the area is formatted, the operator
/// must unlock it (prove they know the passphrase) before any new
/// `cryptfmt` is accepted.
fn crypt_is_formatted() -> bool {
    use beetos_api_block::BlockClient;
    let Some(buf) = crypt_buf() else { return false };
    let Ok(client) = BlockClient::connect_with_retries(8) else { return false };
    let Ok(info) = client.info() else { return false };
    if info.capacity_blocks < beetos_api_cryptblock::CRYPT_AREA_SECTORS {
        return false;
    }
    let base = info.capacity_blocks - beetos_api_cryptblock::CRYPT_AREA_SECTORS;
    if client.read_blocks(base, 1, buf).is_err() { return false }
    let slice = unsafe { core::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
    let sector = beetos_api_block::data(slice);
    sector.len() >= 8 && &sector[..8] == beetos_api_cryptblock::MAGIC
}

fn crypt_format(passphrase: &str) -> FsError {
    use beetos_api_block::BlockClient;
    // Once the area is formatted, only an authenticated operator can
    // wipe it. Anyone with IPC access to the fs server can send
    // CryptFormat, so without this gate any sibling service could
    // erase /data without knowing the passphrase.
    if crypt_is_formatted() && crypt_session().is_none() {
        return FsError::Locked;
    }
    let Some(buf) = crypt_buf() else { return FsError::NoSpace };
    let client = match BlockClient::connect_with_retries(8) {
        Ok(c) => c,
        Err(_) => return FsError::NoSpace,
    };
    match CryptDisk::format(client, buf, passphrase.as_bytes()) {
        Ok(disk) => {
            unsafe { CRYPT = Some(disk) };
            FsError::Ok
        }
        Err(e) => map_crypt_err(e),
    }
}

fn crypt_open(passphrase: &str) -> FsError {
    use beetos_api_block::BlockClient;
    let Some(buf) = crypt_buf() else { return FsError::NoSpace };
    let client = match BlockClient::connect_with_retries(8) {
        Ok(c) => c,
        Err(_) => return FsError::NoSpace,
    };
    match CryptDisk::open(client, buf, passphrase.as_bytes()) {
        Ok(disk) => {
            unsafe { CRYPT = Some(disk) };
            FsError::Ok
        }
        // **Preserve the existing session on failure.** A bad-pass
        // attempt by an attacker (or a typo by another tab) must not
        // log out an already-unlocked operator — that would be a free
        // local DoS for anyone with IPC access to the fs server.
        Err(e) => map_crypt_err(e),
    }
}

fn crypt_lock() -> FsError {
    // Best-effort key hygiene: drop the session. (The CryptDisk copy
    // semantics mean the key bytes may linger on old stack frames —
    // real zeroization is an M1-port task alongside the Argon2 KDF.)
    unsafe { CRYPT = None };
    FsError::Ok
}

fn crypt_write(name: &str, content: &[u8]) -> FsError {
    let Some(disk) = crypt_session() else { return FsError::Locked };
    let Some(buf) = crypt_buf() else { return FsError::NoSpace };
    if name.is_empty() || name.contains('/') {
        return FsError::InvalidPath;
    }
    let mut payload = [0u8; SLOT_PAYLOAD];
    if beetos_api_fs::pack_data_entry(name, content, &mut payload).is_none() {
        return FsError::NoSpace;
    }
    let slot = match crypt_find(&disk, buf, name) {
        Some((slot, _)) => slot,
        None => match crypt_find_free(&disk, buf) {
            Some(s) => s,
            None => return FsError::NoSpace,
        },
    };
    match disk.write_slot(buf, slot, &payload) {
        Ok(()) => FsError::Ok,
        Err(e) => map_crypt_err(e),
    }
}

fn crypt_remove(name: &str) -> FsError {
    let Some(disk) = crypt_session() else { return FsError::Locked };
    let Some(buf) = crypt_buf() else { return FsError::NoSpace };
    match crypt_find(&disk, buf, name) {
        Some((slot, _)) => match disk.erase_slot(buf, slot) {
            Ok(()) => FsError::Ok,
            Err(e) => map_crypt_err(e),
        },
        None => FsError::NotFound,
    }
}

/// Read `name` and hand the decrypted bytes to `sink`.
///
/// **Tamper surfacing**: when the named file isn't found but at least
/// one slot failed authentication during the scan, the result is
/// `Corrupt` rather than `NotFound` — we can't recover the tampered
/// slot's name (auth failed before we could parse the entry), so we
/// can't be sure the missing file wasn't the corrupted one. Surfacing
/// `Corrupt` is the honest answer: "looked everywhere, some bytes
/// were tampered with, we can't say it isn't yours".
fn crypt_read(name: &str, mut sink: impl FnMut(&[u8])) -> FsError {
    let Some(disk) = crypt_session() else { return FsError::Locked };
    let Some(buf) = crypt_buf() else { return FsError::NoSpace };
    let mut saw_corrupt = false;
    for slot in 0..CRYPT_SLOTS {
        match disk.read_slot(buf, slot) {
            Ok(payload) => {
                if let Some((entry_name, data)) = beetos_api_fs::unpack_data_entry(&payload) {
                    if entry_name == name {
                        sink(data);
                        return FsError::Ok;
                    }
                }
            }
            Err(CryptError::Corrupt) => {
                saw_corrupt = true;
            }
            _ => {}
        }
    }
    if saw_corrupt { FsError::Corrupt } else { FsError::NotFound }
}

/// List entries: `cb(name, size)` per file.
fn crypt_list(mut cb: impl FnMut(&str, usize)) -> FsError {
    let Some(disk) = crypt_session() else { return FsError::Locked };
    let Some(buf) = crypt_buf() else { return FsError::NoSpace };
    for slot in 0..CRYPT_SLOTS {
        if let Ok(payload) = disk.read_slot(buf, slot) {
            if let Some((name, data)) = beetos_api_fs::unpack_data_entry(&payload) {
                cb(name, data.len());
            }
        }
    }
    FsError::Ok
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
    }
    beetos::pl011::init(uart_base);

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
        id if id == FsOp::CryptLock as usize => {
            ipc_reply(sender, crypt_lock() as usize);
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
    if is_data_path(path) {
        // /data itself is the directory; reading it should report
        // IsDirectory, not the misleading NotFound the empty-subpath
        // scan would return.
        if data_subpath(path).is_empty() { return FsError::IsDirectory; }
        let st = crypt_read(data_subpath(path), |data| {
            match core::str::from_utf8(data) {
                Ok(text) => {
                    puts(text);
                    if !text.ends_with('\n') { putc(b'\n'); }
                }
                Err(_) => {
                    let _ = write!(UartWriter, "<binary: {} bytes>\n", data.len());
                }
            }
        });
        return st;
    }

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
    if is_data_path(path) {
        // The /data root is cd-able; entries inside are files.
        return if data_subpath(path).is_empty() { FsError::Ok } else { FsError::NotDirectory };
    }
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
    if is_root {
        let state = if crypt_session().is_some() { "unlocked" } else { "locked" };
        let _ = write!(UartWriter, "  data/  (encrypted, {})\n", state);
    }

    if is_data_path(path) {
        if !data_subpath(path).is_empty() { return FsError::NotDirectory; }
        return crypt_list(|name, size| {
            let _ = write!(UartWriter, "  {} ({} bytes)\n", name, size);
        });
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
    // /data is a flat encrypted-slot namespace; no subdirectories,
    // and the mount root itself always exists. Round-10 adversarial
    // caught `mkdir /data` silently creating a *ramfs* /data entry
    // that then shadowed the virtual mount in ls listings.
    if is_data_path(path) { return FsError::AlreadyExists; }
    match ramfs::mkdir(path) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::AlreadyExists) => FsError::AlreadyExists,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(_) => FsError::NoSpace,
    }
}

fn do_remove(path: &str) -> FsError {
    if is_data_path(path) {
        let name = data_subpath(path);
        if name.is_empty() { return FsError::IsDirectory; }
        return crypt_remove(name);
    }
    if is_disk_path(path) { return FsError::ReadOnly; }
    match ramfs::remove(path) {
        Ok(()) => FsError::Ok,
        Err(ramfs::FsError::NotFound) => FsError::NotFound,
        Err(ramfs::FsError::NotEmpty) => FsError::NotEmpty,
        Err(_) => FsError::InvalidPath,
    }
}

fn do_write(path: &str, content: &[u8]) -> FsError {
    if is_data_path(path) {
        let name = data_subpath(path);
        if name.is_empty() { return FsError::IsDirectory; }
        return crypt_write(name, content);
    }
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
        } else if op == FsOp::CryptFormat as usize {
            // "path" carries the passphrase for the two crypt ops.
            crypt_format(path)
        } else if op == FsOp::CryptOpen as usize {
            crypt_open(path)
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
    if is_root {
        let state = if crypt_session().is_some() { "unlocked" } else { "locked" };
        let _ = write!(w, "  data/  (encrypted, {})\n", state);
    }

    if is_data_path(path) {
        if !data_subpath(path).is_empty() { return FsError::NotDirectory; }
        return crypt_list(|name, size| {
            let _ = write!(w, "  {} ({} bytes)\n", name, size);
        });
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

    if is_data_path(path) {
        if data_subpath(path).is_empty() { return FsError::IsDirectory; }
        return crypt_read(data_subpath(path), |data| {
            match core::str::from_utf8(data) {
                Ok(text) => {
                    let _ = write!(w, "{}", text);
                    if !text.ends_with('\n') { let _ = write!(w, "\n"); }
                }
                Err(_) => {
                    let _ = write!(w, "<binary: {} bytes>\n", data.len());
                }
            }
        });
    }

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
