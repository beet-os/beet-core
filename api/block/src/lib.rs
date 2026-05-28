// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS block-service IPC protocol.
//!
//! The block service (`os/block`) owns the per-platform storage
//! drivers (SDHCI on RPi5, ANS/NVMe on M1, virtio-blk on QEMU) and
//! exposes them through a single Xous IPC interface that other
//! services — most notably the FS service — call into.
//!
//! ## Protocol overview
//!
//! Two flavours of operation:
//!
//! - **Scalar / BlockingScalar** for metadata-only queries (capacity,
//!   block-size).  Returns small integers, no buffer.
//! - **MutableBorrow** for data transfers.  Caller lends a page-aligned
//!   buffer; the server reads/writes blocks into it.  Buffer layout
//!   is shared between the two parties via this crate's constants.
//!
//! Both reads and writes use the same shared-buffer convention so
//! the FS service can do `borrow_mut(buf, op=ReadBlocks)` and
//! `borrow_mut(buf, op=WriteBlocks)` without two different memory-
//! management paths.
//!
//! Layout of the MutableBorrow buffer (read + write share):
//!
//!   ```text
//!   offset  size  field
//!     0      8    LBA (u64, little-endian)
//!     8      4    n_blocks (u32, little-endian)
//!    12      1    BlockResult (status — caller checks this on return)
//!    13      3    reserved (zero on send)
//!    16     N×bs  data (read: server writes here; write: caller fills)
//!   ```
//!
//! The buffer's total length must be at least `16 + n_blocks ×
//! block_size`. Servers reject mismatched buffers with
//! [`BlockResult::BadBuffer`].
//!
//! No alloc required on either side: every field is fixed-offset.

#![no_std]

/// Well-known Server ID for the block service.
/// "BEETOSBL" (BeetOS block).
pub const BLOCK_SID: [u32; 4] = [0x4245_4554, 0x4F53_424C, 0, 0];

/// Byte offset of the LBA field inside a MutableBorrow buffer.
pub const BUF_LBA_OFFSET: usize = 0;
/// Byte offset of the n_blocks field.
pub const BUF_NBLOCKS_OFFSET: usize = 8;
/// Byte offset of the status byte the server writes on completion.
pub const BUF_STATUS_OFFSET: usize = 12;
/// Byte offset where the data payload starts.
pub const BUF_DATA_OFFSET: usize = 16;

/// Opcodes the block service understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum BlockOp {
    /// Query device geometry — block size + capacity.
    /// BlockingScalar with no args.
    /// Returns `Scalar2(block_size_u32, capacity_blocks_u64_low |
    ///                    capacity_blocks_u64_high << 32)` — packed
    /// into two scalars so we don't blow the message limit.
    GetInfo = 0,

    /// Read N blocks into the lent buffer (MutableBorrow).
    /// Buffer layout per the module docs:
    ///   [0..8]   LBA
    ///   [8..12]  n_blocks
    ///   [12]     status (filled in by the server on return)
    ///   [16..]   data (server-filled)
    ReadBlocks = 1,

    /// Write N blocks from the lent buffer (MutableBorrow).
    /// Same buffer layout — caller fills `data` BEFORE sending,
    /// server reads it, then writes the status byte for the caller
    /// to inspect when the borrow returns.
    WriteBlocks = 2,
}

/// Result of a block operation. Maps onto
/// `beetos_api_storage::BlockError` plus a Success variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockResult {
    Ok          = 0,
    OutOfRange  = 1,
    BadBuffer   = 2,
    Timeout     = 3,
    Io          = 4,
    NotReady    = 5,
    Other       = 6,
}

impl BlockResult {
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => Self::Ok,
            1 => Self::OutOfRange,
            2 => Self::BadBuffer,
            3 => Self::Timeout,
            4 => Self::Io,
            5 => Self::NotReady,
            _ => Self::Other,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire-format helpers — same on both sides of the IPC.
// ─────────────────────────────────────────────────────────────────────────────

/// Stamp the LBA + n_blocks header into the front of a buffer.
/// Returns the number of bytes consumed (always `BUF_DATA_OFFSET`).
#[inline]
pub fn write_header(buf: &mut [u8], lba: u64, n_blocks: u32) -> usize {
    assert!(buf.len() >= BUF_DATA_OFFSET);
    buf[BUF_LBA_OFFSET..BUF_LBA_OFFSET + 8]
        .copy_from_slice(&lba.to_le_bytes());
    buf[BUF_NBLOCKS_OFFSET..BUF_NBLOCKS_OFFSET + 4]
        .copy_from_slice(&n_blocks.to_le_bytes());
    buf[BUF_STATUS_OFFSET] = 0;
    BUF_DATA_OFFSET
}

/// Extract `(lba, n_blocks)` from the front of a buffer.
#[inline]
pub fn read_header(buf: &[u8]) -> (u64, u32) {
    assert!(buf.len() >= BUF_DATA_OFFSET);
    let mut l = [0u8; 8];
    l.copy_from_slice(&buf[BUF_LBA_OFFSET..BUF_LBA_OFFSET + 8]);
    let mut n = [0u8; 4];
    n.copy_from_slice(&buf[BUF_NBLOCKS_OFFSET..BUF_NBLOCKS_OFFSET + 4]);
    (u64::from_le_bytes(l), u32::from_le_bytes(n))
}

/// Server-side: write the status byte at the right offset.
#[inline]
pub fn write_status(buf: &mut [u8], status: BlockResult) {
    if buf.len() > BUF_STATUS_OFFSET {
        buf[BUF_STATUS_OFFSET] = status as u8;
    }
}

/// Client-side: read the server's status byte on borrow return.
#[inline]
pub fn read_status(buf: &[u8]) -> BlockResult {
    if buf.len() > BUF_STATUS_OFFSET {
        BlockResult::from_u8(buf[BUF_STATUS_OFFSET])
    } else {
        BlockResult::BadBuffer
    }
}

/// Borrow the data portion of a buffer (mutable view).
#[inline]
pub fn data_mut(buf: &mut [u8]) -> &mut [u8] {
    &mut buf[BUF_DATA_OFFSET..]
}

/// Borrow the data portion of a buffer (read-only view).
#[inline]
pub fn data(buf: &[u8]) -> &[u8] {
    &buf[BUF_DATA_OFFSET..]
}

// ─────────────────────────────────────────────────────────────────────────────
// Client helper — `BlockClient` wraps the connect + MutableBorrow dance so
// every caller doesn't open-code the IPC envelope. Server stays untouched;
// this is purely a sugar layer over `xous::rsyscall`.
//
// Buffer ownership: the caller allocates a `MemoryRange` (via
// `xous::map_memory`) and lends it via the borrow. We never alloc on the
// client side — keeps this crate suitable for callers that want a single,
// reused IPC buffer for the lifetime of the process (the typical pattern
// in tight no_alloc services).
// ─────────────────────────────────────────────────────────────────────────────

/// Total buffer length (in bytes) required to carry `n_blocks` of
/// 512-byte data plus the IPC header. Use this when sizing the page
/// you'll lend to the block service.
pub const fn buf_len_for_blocks(n_blocks: u32) -> usize {
    BUF_DATA_OFFSET + (n_blocks as usize) * 512
}

/// Geometry returned by [`BlockClient::info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    pub block_size: u32,
    pub capacity_blocks: u64,
}

/// Anything that can go wrong on the client side of a block call.
/// `Block(BlockResult)` carries the server-stamped status when the IPC
/// itself succeeded but the operation didn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientError {
    /// `Connect` to `BLOCK_SID` returned `ServerNotFound` — the service
    /// either isn't spawned yet, or hasn't called `CreateServerWithAddress`.
    NotConnected,
    /// The lent buffer is smaller than `buf_len_for_blocks(n_blocks)`.
    BufferTooSmall,
    /// `SendMessage` returned an unexpected `Result` variant or a
    /// kernel error.
    KernelError,
    /// IPC round-trip succeeded but the server returned a non-`Ok` status.
    Block(BlockResult),
}

/// Strongly-typed client for the BeetOS block service.
///
/// Holds a connection ID; cheap to copy. Constructed via
/// [`BlockClient::connect`] which performs the `SID` → `CID` handshake.
#[derive(Debug, Clone, Copy)]
pub struct BlockClient {
    cid: xous::CID,
}

impl BlockClient {
    /// Open a connection to the block service. The block service must
    /// have already registered its server (PID 6 on QEMU virt does this
    /// during `_start`); if not, returns [`ClientError::NotConnected`].
    pub fn connect() -> Result<Self, ClientError> {
        let sid = xous::SID::from_array(BLOCK_SID);
        match xous::rsyscall(xous::SysCall::Connect(sid)) {
            Ok(xous::Result::ConnectionID(cid)) => Ok(Self { cid }),
            _ => Err(ClientError::NotConnected),
        }
    }

    /// Query the device geometry (block size + capacity in blocks).
    /// Uses `BlockingScalar`/`Scalar2` — no buffer needed.
    pub fn info(&self) -> Result<DeviceInfo, ClientError> {
        let msg = xous::Message::BlockingScalar(xous::ScalarMessage {
            id: BlockOp::GetInfo as usize,
            arg1: 0, arg2: 0, arg3: 0, arg4: 0,
        });
        match xous::rsyscall(xous::SysCall::SendMessage(self.cid, msg)) {
            Ok(xous::Result::Scalar2(bs, cap)) => Ok(DeviceInfo {
                block_size: bs as u32,
                capacity_blocks: cap as u64,
            }),
            _ => Err(ClientError::KernelError),
        }
    }

    /// Read `n_blocks` blocks starting at `lba` into `buf`. The data
    /// lands at `BUF_DATA_OFFSET` (use [`data`] to slice it out after).
    /// `buf` must be at least [`buf_len_for_blocks`]`(n_blocks)`.
    pub fn read_blocks(
        &self,
        lba: u64,
        n_blocks: u32,
        buf: xous::MemoryRange,
    ) -> Result<(), ClientError> {
        self.borrow_op(BlockOp::ReadBlocks, lba, n_blocks, buf)
    }

    /// Write `n_blocks` blocks starting at `lba` from `buf`. The caller
    /// must have filled the data region at `BUF_DATA_OFFSET..` before
    /// calling. The server's status byte is checked on return.
    pub fn write_blocks(
        &self,
        lba: u64,
        n_blocks: u32,
        buf: xous::MemoryRange,
    ) -> Result<(), ClientError> {
        self.borrow_op(BlockOp::WriteBlocks, lba, n_blocks, buf)
    }

    /// Shared `MutableBorrow` path: stamp the header, send the message,
    /// decode the server-written status byte. Both Read and Write use
    /// the exact same envelope shape, only the opcode differs.
    fn borrow_op(
        &self,
        op: BlockOp,
        lba: u64,
        n_blocks: u32,
        buf: xous::MemoryRange,
    ) -> Result<(), ClientError> {
        let needed = buf_len_for_blocks(n_blocks);
        if buf.len() < needed { return Err(ClientError::BufferTooSmall); }

        // SAFETY: `buf` is a MemoryRange we own for the duration of the
        // call (the borrow returns it before SendMessage completes).
        let slice = unsafe {
            core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len())
        };
        write_header(slice, lba, n_blocks);

        let msg = xous::Message::MutableBorrow(xous::MemoryMessage {
            id: op as usize,
            buf,
            offset: None,
            valid: None,
        });

        match xous::rsyscall(xous::SysCall::SendMessage(self.cid, msg)) {
            Ok(xous::Result::MemoryReturned(_, _)) | Ok(xous::Result::Ok) => {
                let slice = unsafe {
                    core::slice::from_raw_parts(buf.as_ptr(), buf.len())
                };
                match read_status(slice) {
                    BlockResult::Ok => Ok(()),
                    other => Err(ClientError::Block(other)),
                }
            }
            _ => Err(ClientError::KernelError),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — pure header round-trip (BlockClient itself needs a live xous
// runtime, so its coverage comes from in-tree consumers like the shell's
// boot-time self-test).
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let mut buf = [0xFFu8; 1024];
        write_header(&mut buf, 0xABCD_EF01_2345_6789, 8);
        let (lba, n) = read_header(&buf);
        assert_eq!(lba, 0xABCD_EF01_2345_6789);
        assert_eq!(n, 8);
        // Status byte starts cleared.
        assert_eq!(read_status(&buf), BlockResult::Ok);
    }

    #[test]
    fn status_round_trip() {
        let mut buf = [0u8; 1024];
        write_status(&mut buf, BlockResult::OutOfRange);
        assert_eq!(read_status(&buf), BlockResult::OutOfRange);
        write_status(&mut buf, BlockResult::Timeout);
        assert_eq!(read_status(&buf), BlockResult::Timeout);
    }

    #[test]
    fn data_view_starts_at_offset_16() {
        let mut buf = [0u8; 64];
        for i in 0..buf.len() { buf[i] = i as u8; }
        let d = data(&buf);
        assert_eq!(d.len(), 64 - BUF_DATA_OFFSET);
        assert_eq!(d[0], 16);
        assert_eq!(d[1], 17);

        let dm = data_mut(&mut buf);
        dm[0] = 0xFF;
        assert_eq!(buf[BUF_DATA_OFFSET], 0xFF);
    }

    #[test]
    fn block_result_round_trips_through_u8() {
        for r in [BlockResult::Ok, BlockResult::OutOfRange, BlockResult::BadBuffer,
                  BlockResult::Timeout, BlockResult::Io, BlockResult::NotReady,
                  BlockResult::Other] {
            assert_eq!(BlockResult::from_u8(r as u8), r);
        }
        // Unknown byte falls back to Other (forward-compat for new server variants).
        assert_eq!(BlockResult::from_u8(99), BlockResult::Other);
    }

    #[test]
    fn buf_len_helper_matches_layout() {
        assert_eq!(buf_len_for_blocks(0), BUF_DATA_OFFSET);
        assert_eq!(buf_len_for_blocks(1), BUF_DATA_OFFSET + 512);
        assert_eq!(buf_len_for_blocks(8), BUF_DATA_OFFSET + 8 * 512);
    }
}
