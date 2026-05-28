// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS block-device service.
//!
//! Owns the per-platform storage drivers and exposes them through a
//! single Xous IPC interface defined in `api/block`. Other services
//! (FS most importantly) talk to us instead of touching MMIO
//! directly — that's the microkernel-correct path to storage
//! (option β in the BlockDevice integration plan).
//!
//! Today the only backend wired up is the kernel-mapped read-only
//! disk region (qemu-virt virtio-blk image, copied into the
//! service's address space at boot by the kernel — same delivery
//! the FS service uses today, transitional until we move the disk
//! mapping entirely here).  Future commits will swap that for a
//! real driver (SDHCI on RPi5, ANS on M1) without changing the IPC
//! protocol clients see.
//!
//! Boot params (set by the kernel in x0-x2):
//!   x0 = UART MMIO VA
//!   x1 = disk data VA (0 if no disk present)
//!   x2 = disk data size in bytes (0 if no disk present)

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use beetos_api_block::{
    self as block, BlockOp, BlockResult, BLOCK_SID,
    BUF_DATA_OFFSET,
};

// ─────────────────────────────────────────────────────────────────────────────
// UART (mirror of the FS service's tiny PL011 driver — kept inline so the
// service is one self-contained binary without a shared "platform-uart"
// dependency).
// ─────────────────────────────────────────────────────────────────────────────

const UART_DR:      usize = 0x00;
const UART_FR:      usize = 0x18;
const UART_FR_TXFF: u32   = 1 << 5;

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

struct UartWriter;
impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() { putc(b); }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend — the disk pages the kernel mapped into our address space.
//
// Read-only today; writes return Io.  The eventual real-driver backend
// (SDHCI Host, NVMe Transport) will sit behind a tiny `impl Storage`
// dispatch.
// ─────────────────────────────────────────────────────────────────────────────

const BLOCK_SIZE: u32 = 512;

static mut DISK_BASE: usize = 0;
static mut DISK_SIZE: usize = 0;

fn capacity_blocks() -> u64 {
    unsafe { DISK_SIZE as u64 / BLOCK_SIZE as u64 }
}

/// Copy `n_blocks * BLOCK_SIZE` bytes starting at LBA `lba` from the
/// disk region into the caller's buffer (the `data` slice).
/// Returns `BlockResult` so the IPC layer can stamp the right
/// status byte.
fn handle_read(lba: u64, n_blocks: u32, data: &mut [u8]) -> BlockResult {
    let needed = (n_blocks as usize) * BLOCK_SIZE as usize;
    if data.len() < needed { return BlockResult::BadBuffer; }

    let cap_blocks = capacity_blocks();
    let end = lba.saturating_add(n_blocks as u64);
    if end > cap_blocks { return BlockResult::OutOfRange; }

    unsafe {
        if DISK_BASE == 0 || DISK_SIZE == 0 { return BlockResult::NotReady; }
        let src_off = (lba as usize) * BLOCK_SIZE as usize;
        let src = (DISK_BASE + src_off) as *const u8;
        // memcpy via volatile reads — the disk region is normal RAM
        // but we go through pointer arithmetic explicitly so the
        // intent is unmistakable.
        core::ptr::copy_nonoverlapping(src, data.as_mut_ptr(), needed);
    }
    BlockResult::Ok
}

/// Writes are not supported by today's transitional in-RAM disk
/// backing (the kernel mapped it read-only).  Future driver
/// backends will route this through SDHCI Host::write_block or
/// NvmeBlockDevice::write_blocks.
fn handle_write(_lba: u64, _n_blocks: u32, _data: &[u8]) -> BlockResult {
    BlockResult::Io
}

// ─────────────────────────────────────────────────────────────────────────────
// IPC dispatch
// ─────────────────────────────────────────────────────────────────────────────

fn ipc_reply2(sender: xous::MessageSender, a: usize, b: usize) {
    let _ = xous::rsyscall(xous::SysCall::ReturnScalar2(sender, a, b));
}

fn handle_blocking_scalar(sender: xous::MessageSender, scalar: xous::ScalarMessage) {
    match scalar.id {
        id if id == BlockOp::GetInfo as usize => {
            // BlockOp::GetInfo returns (block_size, capacity_blocks).
            // capacity_blocks is u64 — we just pack it as the second
            // Scalar2 word; on a 64-bit target this is a clean fit.
            ipc_reply2(sender, BLOCK_SIZE as usize, capacity_blocks() as usize);
        }
        _ => {
            // Unknown scalar op — return zeros so the caller knows
            // we didn't crash but also didn't satisfy them.
            ipc_reply2(sender, 0, 0);
        }
    }
}

fn handle_mutable_borrow(_sender: xous::MessageSender, mem: &xous::MemoryMessage) {
    // SAFETY: kernel handed us this memory range via the IPC borrow;
    // it's valid for the duration of this dispatch and aliased back
    // to the caller when we return.
    let buf = unsafe {
        core::slice::from_raw_parts_mut(mem.buf.as_mut_ptr(), mem.buf.len())
    };
    if buf.len() < BUF_DATA_OFFSET {
        // Buffer too small even for the header — best-effort status
        // stamp at offset 0 so the caller spots the problem.
        if !buf.is_empty() { buf[0] = BlockResult::BadBuffer as u8; }
        return;
    }

    let (lba, n_blocks) = block::read_header(buf);
    let status = match mem.id {
        id if id == BlockOp::ReadBlocks as usize => {
            handle_read(lba, n_blocks, block::data_mut(buf))
        }
        id if id == BlockOp::WriteBlocks as usize => {
            handle_write(lba, n_blocks, block::data(buf))
        }
        _ => BlockResult::Other,
    };
    block::write_status(buf, status);
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

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

    let _ = write!(
        UartWriter,
        "[block] started, disk={} bytes ({} blocks)\n",
        disk_size, capacity_blocks(),
    );

    let sid = xous::SID::from_array(BLOCK_SID);
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
                        // MutableBorrow returns the page to the
                        // caller automatically on Drop; forget the
                        // envelope so we don't double-free in the
                        // xous-rs Drop impl.
                        core::mem::forget(env);
                    }
                    _ => {}
                }
            }
            _ => { xous::yield_slice(); }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)); } }
}
