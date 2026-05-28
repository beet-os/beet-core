// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Generic block-device abstraction over [`crate::sdhci`],
//! [`crate::nvme`], and any future storage transport (virtio-blk,
//! PCIe NVMe, hosted-mode file-backed).
//!
//! Why this lives at the kernel top level:
//!
//! - The protocol layers (`sdhci`, `nvme`) describe **commands**
//!   on a wire. The platform glue (`platform/bcm2712/sdhci_brcm`,
//!   future `platform/apple_t8103/ans`) wires those commands to
//!   real MMIO. Neither of them is what a filesystem or an app
//!   wants to use — both want "give me block N" or "write block N".
//! - A single shared trait lets the FS service (and future raw-
//!   block tools) treat every storage backend identically. SDHCI on
//!   RPi5, ANS NVMe on M1, hosted-mode file under `cargo run`,
//!   virtio-blk on QEMU virt — same call, different impl.
//! - Tests live here so the contract is enforced by code, not
//!   docstrings: the suite below runs the same WRITE-then-READ
//!   round-trip against three different backends and asserts they
//!   all behave identically.

use core::result::Result;

/// Errors common to every block device.  Backend-specific failure
/// modes get mapped onto these on the way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// `lba + n_blocks` would extend past `capacity_blocks()`.
    OutOfRange,
    /// Buffer length is not an exact multiple of `block_size()`.
    BadBuffer,
    /// Underlying hardware / transport timed out.
    Timeout,
    /// Underlying hardware returned an error status (CRC, data, etc.).
    Io,
    /// Device reported a condition we don't yet handle.
    Other,
}

/// What a storage backend has to provide. All methods take `&mut self`
/// because real drivers serialise on per-device state (controller
/// command queues, mailbox channels, MMIO registers).
pub trait BlockDevice {
    /// Bytes per logical block. Almost always 512 on SDHC and 4096 on
    /// modern NVMe; the FS layer treats this as "device's native
    /// granularity".
    fn block_size(&self) -> u32;

    /// Number of logical blocks the device exposes. `capacity_bytes()`
    /// is `block_size() * capacity_blocks()`.
    fn capacity_blocks(&self) -> u64;

    fn capacity_bytes(&self) -> u64 {
        self.block_size() as u64 * self.capacity_blocks()
    }

    /// Read `buf.len() / block_size()` blocks starting at `lba` into
    /// `buf`. `buf` must be an exact multiple of `block_size()`.
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write `buf.len() / block_size()` blocks starting at `lba`
    /// from `buf`.
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Hosted backend: file-backed block device for `cargo run` / tests.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(not(beetos))]
mod hosted {
    use super::{BlockDevice, BlockError};
    extern crate alloc;
    use alloc::vec::Vec;

    /// In-memory block device — Vec<u8> as the backing store. Used
    /// by the hosted-mode integration tests so we don't need real
    /// filesystem access in `cargo test`. Pair with a `Vec<u8>` of
    /// `block_size * n_blocks` zeros to get a fresh device.
    pub struct MemBlockDevice {
        data: Vec<u8>,
        block_size: u32,
    }

    impl MemBlockDevice {
        pub fn new(block_size: u32, n_blocks: u64) -> Self {
            let len = (block_size as u64 * n_blocks) as usize;
            Self { data: alloc::vec![0u8; len], block_size }
        }

        /// Borrow the backing bytes — handy for tests that want to
        /// snapshot / patch the device behind the trait.
        pub fn as_bytes(&self) -> &[u8] { &self.data }
    }

    impl BlockDevice for MemBlockDevice {
        fn block_size(&self) -> u32 { self.block_size }
        fn capacity_blocks(&self) -> u64 {
            self.data.len() as u64 / self.block_size as u64
        }
        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
            let off = lba.checked_mul(self.block_size as u64)
                .ok_or(BlockError::OutOfRange)?;
            let end = off.checked_add(buf.len() as u64)
                .ok_or(BlockError::OutOfRange)?;
            if end > self.data.len() as u64 { return Err(BlockError::OutOfRange); }
            buf.copy_from_slice(&self.data[off as usize..end as usize]);
            Ok(())
        }
        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
            if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
            let off = lba.checked_mul(self.block_size as u64)
                .ok_or(BlockError::OutOfRange)?;
            let end = off.checked_add(buf.len() as u64)
                .ok_or(BlockError::OutOfRange)?;
            if end > self.data.len() as u64 { return Err(BlockError::OutOfRange); }
            self.data[off as usize..end as usize].copy_from_slice(buf);
            Ok(())
        }
    }

    /// File-backed block device — opens a sparse file on the host
    /// FS and seeks for each request. `cargo run` use case: point at
    /// `target/sd.img` and the kernel sees a real block-addressable
    /// surface. The file is created if absent (zero-filled to
    /// `block_size * n_blocks`).
    pub struct FileBlockDevice {
        file:       std::fs::File,
        block_size: u32,
        n_blocks:   u64,
    }

    impl FileBlockDevice {
        pub fn open_or_create(
            path: impl AsRef<std::path::Path>,
            block_size: u32,
            n_blocks: u64,
        ) -> std::io::Result<Self> {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new()
                .read(true).write(true).create(true).truncate(false)
                .open(path)?;
            let want = block_size as u64 * n_blocks;
            let cur = file.seek(SeekFrom::End(0))?;
            if cur < want {
                // Extend with zeros — `set_len` would do it too but
                // some FSes report ENOSPC only at write time, so
                // poking the last byte gets us a real error here.
                file.seek(SeekFrom::Start(want - 1))?;
                file.write_all(&[0])?;
            }
            Ok(Self { file, block_size, n_blocks })
        }
    }

    impl BlockDevice for FileBlockDevice {
        fn block_size(&self) -> u32 { self.block_size }
        fn capacity_blocks(&self) -> u64 { self.n_blocks }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            use std::io::{Read, Seek, SeekFrom};
            if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
            let off = lba.checked_mul(self.block_size as u64).ok_or(BlockError::OutOfRange)?;
            if off + buf.len() as u64 > self.capacity_bytes() { return Err(BlockError::OutOfRange); }
            self.file.seek(SeekFrom::Start(off)).map_err(|_| BlockError::Io)?;
            self.file.read_exact(buf).map_err(|_| BlockError::Io)?;
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
            use std::io::{Seek, SeekFrom, Write};
            if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
            let off = lba.checked_mul(self.block_size as u64).ok_or(BlockError::OutOfRange)?;
            if off + buf.len() as u64 > self.capacity_bytes() { return Err(BlockError::OutOfRange); }
            self.file.seek(SeekFrom::Start(off)).map_err(|_| BlockError::Io)?;
            self.file.write_all(buf).map_err(|_| BlockError::Io)?;
            Ok(())
        }
    }
}

#[cfg(not(beetos))]
pub use hosted::{FileBlockDevice, MemBlockDevice};

// ─────────────────────────────────────────────────────────────────────────────
// NVMe Transport → BlockDevice adapter
// ─────────────────────────────────────────────────────────────────────────────

/// Wraps an NVMe [`Transport`] + a known namespace into a
/// [`BlockDevice`]. Single-namespace device today — each
/// adapter targets one (namespace_id, block_size, capacity)
/// combination, discovered up-front via IDENTIFY.
pub struct NvmeBlockDevice<T: crate::nvme::Transport> {
    transport:    T,
    namespace_id: u32,
    block_size:   u32,
    n_blocks:     u64,
    /// Per-operation command identifier; incremented on each issue
    /// so the CQE can be matched against its SQE in the future.
    next_cid:     u16,
}

impl<T: crate::nvme::Transport> NvmeBlockDevice<T> {
    /// Discover capacity + block size by issuing IDENTIFY_NAMESPACE,
    /// then wrap the transport.  `nsid` is typically 1 on every
    /// consumer drive.
    pub fn new(mut transport: T, nsid: u32) -> Result<Self, BlockError> {
        let mut buf = [0u8; 4096];
        let sqe = crate::nvme::Sqe::identify(0, crate::nvme::cns::IDENTIFY_NAMESPACE, nsid, 0);
        let cqe = transport.submit(&sqe, &mut buf);
        if !cqe.status().is_success() { return Err(BlockError::Io); }
        let ns = crate::nvme::NamespaceInfo::parse(&buf);
        Ok(Self {
            transport,
            namespace_id: nsid,
            block_size:   ns.block_size,
            n_blocks:     ns.size_lba,
            next_cid:     1,
        })
    }
}

impl<T: crate::nvme::Transport> BlockDevice for NvmeBlockDevice<T> {
    fn block_size(&self) -> u32 { self.block_size }
    fn capacity_blocks(&self) -> u64 { self.n_blocks }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
        let n = (buf.len() / self.block_size as usize) as u16;
        if lba + n as u64 > self.n_blocks { return Err(BlockError::OutOfRange); }
        self.next_cid = self.next_cid.wrapping_add(1);
        let sqe = crate::nvme::Sqe::read(self.next_cid, self.namespace_id, lba, n, 0);
        let cqe = self.transport.submit(&sqe, buf);
        let s = cqe.status();
        if s.is_success() { Ok(()) }
        else if s.sc == crate::nvme::sc_generic::LBA_OUT_OF_RANGE { Err(BlockError::OutOfRange) }
        else { Err(BlockError::Io) }
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
        let n = (buf.len() / self.block_size as usize) as u16;
        if lba + n as u64 > self.n_blocks { return Err(BlockError::OutOfRange); }
        self.next_cid = self.next_cid.wrapping_add(1);
        // Transport::submit takes `&mut [u8]` because a real
        // controller fills the buffer on reads. For writes we need
        // to pass our caller's `&[u8]` payload through unchanged —
        // copy into a scratch we own so the trait signature holds.
        // In hardware land the controller DMAs out of the buffer
        // we point it at, so this copy disappears once the driver
        // stages the payload into the DMA region directly.
        let mut scratch = scratch_buffer(buf.len());
        scratch.as_mut().copy_from_slice(buf);
        let sqe = crate::nvme::Sqe::write(self.next_cid, self.namespace_id, lba, n, 0);
        let cqe = self.transport.submit(&sqe, scratch.as_mut());
        let s = cqe.status();
        if s.is_success() { Ok(()) }
        else if s.sc == crate::nvme::sc_generic::LBA_OUT_OF_RANGE { Err(BlockError::OutOfRange) }
        else { Err(BlockError::Io) }
    }
}

/// Owning buffer abstraction so the no_std + alloc-free kernel path
/// uses a static cap, while hosted tests use a heap Vec. The buffer
/// only needs to live for the duration of one `Transport::submit`
/// call, so the static cap is fine in practice (every Sqe::write we
/// issue is one block).
#[cfg(not(beetos))]
struct Scratch(Vec<u8>);
#[cfg(beetos)]
struct Scratch { buf: [u8; 4096], len: usize }

#[cfg(not(beetos))]
impl AsMut<[u8]> for Scratch {
    fn as_mut(&mut self) -> &mut [u8] { &mut self.0 }
}
#[cfg(beetos)]
impl AsMut<[u8]> for Scratch {
    fn as_mut(&mut self) -> &mut [u8] { &mut self.buf[..self.len] }
}

#[cfg(not(beetos))]
fn scratch_buffer(len: usize) -> Scratch { Scratch(vec![0u8; len]) }
#[cfg(beetos)]
fn scratch_buffer(len: usize) -> Scratch {
    // Kernel-side: bounded to one page. Callers passing larger
    // payloads need to chunk — enforced by the assert.
    assert!(len <= 4096, "kernel-side block write > 4 KB not yet supported");
    Scratch { buf: [0u8; 4096], len }
}

// ─────────────────────────────────────────────────────────────────────────────
// SDHCI Host → BlockDevice adapter
// ─────────────────────────────────────────────────────────────────────────────

/// Wraps an SDHCI [`Host`](crate::sdhci::Host) + the negotiated
/// [`CardInfo`](crate::sdhci::CardInfo) into a [`BlockDevice`].
/// Inherits the Host's lifetime over the underlying [`Mmio`](crate::sdhci::Mmio).
///
/// Single-block PIO reads / writes today — multi-block requests
/// are decomposed into a loop of single-block transfers so callers
/// don't have to know what the controller's current command set
/// supports. CMD18 / CMD25 batching lands once we have IRQ-driven
/// completion.
pub struct SdBlockDevice<'a, M: crate::sdhci::Mmio> {
    host:       crate::sdhci::Host<'a, M>,
    block_size: u32,
    capacity:   u64,
}

impl<'a, M: crate::sdhci::Mmio> SdBlockDevice<'a, M> {
    /// Build the adapter from an already-initialised Host + the
    /// `CardInfo` returned by [`platform::bcm2712::sdhci_brcm::init`].
    /// (The protocol layer requires the card to be in the Tran state
    /// before any block I/O is issued — caller's responsibility.)
    pub fn new(host: crate::sdhci::Host<'a, M>, card: crate::sdhci::CardInfo) -> Self {
        Self {
            host,
            block_size: card.csd.block_len_bytes,
            capacity:   card.blocks,
        }
    }
}

impl<'a, M: crate::sdhci::Mmio> BlockDevice for SdBlockDevice<'a, M> {
    fn block_size(&self) -> u32 { self.block_size }
    fn capacity_blocks(&self) -> u64 { self.capacity }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
        let n = (buf.len() / self.block_size as usize) as u64;
        if lba + n > self.capacity { return Err(BlockError::OutOfRange); }
        // SDHCI LBA arg is 32-bit; bail on cards we can't address
        // (would need CMD23 + extended addressing — not yet).
        if lba + n > u32::MAX as u64 { return Err(BlockError::OutOfRange); }

        let bs = self.block_size as usize;
        for i in 0..n {
            let chunk = &mut buf[(i as usize) * bs..(i as usize + 1) * bs];
            // Host::read_block takes a 32-bit LBA + a 512 B buffer
            // exactly. Translate the BlockError surface.
            self.host.read_block((lba + i) as u32, chunk)
                .map_err(map_sdhci_err)?;
        }
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() as u64 % self.block_size as u64 != 0 { return Err(BlockError::BadBuffer); }
        let n = (buf.len() / self.block_size as usize) as u64;
        if lba + n > self.capacity { return Err(BlockError::OutOfRange); }
        if lba + n > u32::MAX as u64 { return Err(BlockError::OutOfRange); }

        let bs = self.block_size as usize;
        for i in 0..n {
            let chunk = &buf[(i as usize) * bs..(i as usize + 1) * bs];
            self.host.write_block((lba + i) as u32, chunk)
                .map_err(map_sdhci_err)?;
        }
        Ok(())
    }
}

fn map_sdhci_err(e: crate::sdhci::HostError) -> BlockError {
    use crate::sdhci::HostError as H;
    match e {
        H::Timeout | H::CommandTimeout | H::DataTimeout => BlockError::Timeout,
        H::CommandCrc | H::CommandIndex | H::CommandEndBit
            | H::DataCrc | H::DataEndBit => BlockError::Io,
        H::BadResponse => BlockError::Other,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — exercise the BlockDevice contract on every backend
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;

    /// Contract: WRITE arbitrary payload to LBA N, READ back, byte-
    /// exact match. Runs against any `BlockDevice` impl — the same
    /// test body validates MemBlockDevice + NvmeBlockDevice +
    /// FileBlockDevice without duplicating logic.
    fn round_trip<D: BlockDevice>(dev: &mut D) {
        let bs = dev.block_size() as usize;
        let mut payload = alloc::vec![0u8; bs];
        for i in 0..bs { payload[i] = ((i * 13 + 7) & 0xFF) as u8; }
        dev.write_blocks(2, &payload).expect("write");
        let mut back = alloc::vec![0u8; bs];
        dev.read_blocks(2, &mut back).expect("read");
        assert_eq!(payload, back);
    }

    #[test]
    fn mem_block_device_round_trip() {
        let mut d = MemBlockDevice::new(512, 16);
        assert_eq!(d.block_size(), 512);
        assert_eq!(d.capacity_blocks(), 16);
        assert_eq!(d.capacity_bytes(), 16 * 512);
        round_trip(&mut d);
    }

    #[test]
    fn mem_block_device_rejects_misaligned_buf() {
        let mut d = MemBlockDevice::new(512, 4);
        let mut bad = [0u8; 100];
        assert_eq!(d.read_blocks(0, &mut bad), Err(BlockError::BadBuffer));
        let bad_w = [0u8; 100];
        assert_eq!(d.write_blocks(0, &bad_w), Err(BlockError::BadBuffer));
    }

    #[test]
    fn mem_block_device_rejects_out_of_range() {
        let mut d = MemBlockDevice::new(512, 2);
        let mut buf = [0u8; 512];
        assert_eq!(d.read_blocks(99, &mut buf), Err(BlockError::OutOfRange));
        assert_eq!(d.write_blocks(99, &buf), Err(BlockError::OutOfRange));
    }

    // ── NVMe adapter wired up to the in-tree HostedNvmeController ───────

    // Re-export the hosted simulator from nvme's test module by
    // copy-pasting a minimal version here (the original is gated
    // #[cfg(test)] inside nvme.rs so it can't be cross-mod-referenced).
    use crate::nvme::{self, Sqe, Cqe, sc_generic, cns, Transport, opcode_of,
                       admin_opc, nvm_opc, pack_cdw0, cid_of};
    use core::cell::RefCell;
    use alloc::vec::Vec;

    struct SimNvme {
        ns: Vec<u8>,
        block_size: u32,
        sq_head: RefCell<u16>,
    }
    impl SimNvme {
        fn new(bs: u32, n: u64) -> Self {
            Self { ns: alloc::vec![0u8; (bs as u64 * n) as usize],
                   block_size: bs, sq_head: RefCell::new(0) }
        }
        fn cqe(&self, sqe: &Sqe, sc: u8) -> Cqe {
            let mut h = self.sq_head.borrow_mut();
            *h = h.wrapping_add(1);
            Cqe {
                dw0: 0, dw1: 0,
                sq_head_sqid: *h as u32,
                cid_phase_status: (cid_of(sqe.cdw0) as u32)
                    | (1 << 16) | ((sc as u32) << 17),
            }
        }
    }
    impl Transport for SimNvme {
        fn submit(&mut self, sqe: &Sqe, data: &mut [u8]) -> Cqe {
            match opcode_of(sqe.cdw0) {
                op if op == admin_opc::IDENTIFY => {
                    if data.len() < 4096 { return self.cqe(sqe, sc_generic::INVALID_FIELD); }
                    for b in data.iter_mut() { *b = 0; }
                    if (sqe.cdw10 & 0xFF) as u8 == cns::IDENTIFY_NAMESPACE {
                        let n = (self.ns.len() / self.block_size as usize) as u64;
                        data[0..8].copy_from_slice(&n.to_le_bytes());
                        data[8..16].copy_from_slice(&n.to_le_bytes());
                        let exp = self.block_size.trailing_zeros() as u8;
                        data[128 + 2] = exp;
                    }
                    self.cqe(sqe, sc_generic::SUCCESS)
                }
                op if op == nvm_opc::READ => {
                    let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                    let n = (sqe.cdw12 & 0xFFFF) as u64 + 1;
                    let off = (slba * self.block_size as u64) as usize;
                    let len = (n * self.block_size as u64) as usize;
                    if off + len > self.ns.len() { return self.cqe(sqe, sc_generic::LBA_OUT_OF_RANGE); }
                    data[..len].copy_from_slice(&self.ns[off..off + len]);
                    self.cqe(sqe, sc_generic::SUCCESS)
                }
                op if op == nvm_opc::WRITE => {
                    let slba = sqe.cdw10 as u64 | ((sqe.cdw11 as u64) << 32);
                    let n = (sqe.cdw12 & 0xFFFF) as u64 + 1;
                    let off = (slba * self.block_size as u64) as usize;
                    let len = (n * self.block_size as u64) as usize;
                    if off + len > self.ns.len() { return self.cqe(sqe, sc_generic::LBA_OUT_OF_RANGE); }
                    self.ns[off..off + len].copy_from_slice(&data[..len]);
                    self.cqe(sqe, sc_generic::SUCCESS)
                }
                _ => self.cqe(sqe, sc_generic::INVALID_COMMAND_OPCODE),
            }
        }
    }

    #[test]
    fn nvme_block_device_through_simulator_round_trip() {
        let sim = SimNvme::new(512, 32);
        let mut dev = NvmeBlockDevice::new(sim, 1).expect("identify");
        assert_eq!(dev.block_size(), 512);
        assert_eq!(dev.capacity_blocks(), 32);
        round_trip(&mut dev);
    }

    #[test]
    fn nvme_block_device_propagates_lba_out_of_range() {
        let sim = SimNvme::new(512, 4);
        let mut dev = NvmeBlockDevice::new(sim, 1).unwrap();
        let mut buf = [0u8; 512];
        assert_eq!(dev.read_blocks(99, &mut buf), Err(BlockError::OutOfRange));
    }

    // ── File-backed device ─────────────────────────────────────────────

    #[test]
    fn file_block_device_round_trip() {
        // Use a unique tmp file so parallel `cargo test` runs don't collide.
        let path = std::env::temp_dir().join(format!(
            "beetos-block-test-{}.img",
            std::process::id(),
        ));
        let mut d = FileBlockDevice::open_or_create(&path, 512, 16).unwrap();
        assert_eq!(d.block_size(), 512);
        assert_eq!(d.capacity_blocks(), 16);
        round_trip(&mut d);
        // Reopen and confirm persistence.
        drop(d);
        let mut d2 = FileBlockDevice::open_or_create(&path, 512, 16).unwrap();
        let mut back = [0u8; 512];
        d2.read_blocks(2, &mut back).unwrap();
        for i in 0..512 {
            assert_eq!(back[i], ((i * 13 + 7) & 0xFF) as u8);
        }
        let _ = std::fs::remove_file(&path);
    }

    // ── SDHCI adapter — capacity + validation tests ────────────────────
    //
    // The SDHCI Host owns a 'a borrow of the Mmio so we can't trivially
    // reuse the existing in-crate sdhci::tests::MockMmio across module
    // boundaries. We test the adapter's *validation* surface here
    // (block size, capacity, misalignment, out-of-range) which doesn't
    // require simulating a full controller — that side is already
    // covered by sdhci's own 31 tests of Host::{read,write}_block.

    struct MinimalSdhciMmio;
    impl crate::sdhci::Mmio for MinimalSdhciMmio {
        fn read32(&self, _: usize) -> u32 { 0 }
        fn write32(&self, _: usize, _: u32) {}
    }

    fn fake_card(block_size: u32, n_blocks: u64) -> crate::sdhci::CardInfo {
        crate::sdhci::CardInfo {
            rca: 0,
            csd: crate::sdhci::Csd {
                structure_version: 1,
                capacity_bytes:    block_size as u64 * n_blocks,
                block_len_bytes:   block_size,
                read_bl_partial:   false,
                write_bl_partial:  false,
            },
            blocks: n_blocks,
        }
    }

    #[test]
    fn sd_block_device_reports_card_geometry() {
        let mmio = MinimalSdhciMmio;
        let host = crate::sdhci::Host::new(&mmio);
        let card = fake_card(512, 4096);
        let dev = SdBlockDevice::new(host, card);
        assert_eq!(dev.block_size(), 512);
        assert_eq!(dev.capacity_blocks(), 4096);
        assert_eq!(dev.capacity_bytes(), 512 * 4096);
    }

    #[test]
    fn sd_block_device_rejects_misaligned_and_out_of_range() {
        let mmio = MinimalSdhciMmio;
        // poll_budget = 0 keeps the test fast — it'll never actually
        // submit a command before the bounds checks reject the call.
        let host = crate::sdhci::Host { mmio: &mmio, poll_budget: 0 };
        let card = fake_card(512, 4);
        let mut dev = SdBlockDevice::new(host, card);

        // Misaligned buffer.
        let mut bad = [0u8; 100];
        assert_eq!(dev.read_blocks(0, &mut bad), Err(BlockError::BadBuffer));
        let bad_w = [0u8; 100];
        assert_eq!(dev.write_blocks(0, &bad_w), Err(BlockError::BadBuffer));

        // LBA out of range (capacity is 4 blocks).
        let mut buf = [0u8; 512];
        assert_eq!(dev.read_blocks(99, &mut buf), Err(BlockError::OutOfRange));
        assert_eq!(dev.write_blocks(99, &buf), Err(BlockError::OutOfRange));
    }

    #[test]
    fn file_block_device_creates_zero_filled_when_absent() {
        let path = std::env::temp_dir().join(format!(
            "beetos-block-create-{}.img",
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&path);
        let mut d = FileBlockDevice::open_or_create(&path, 512, 4).unwrap();
        let mut buf = [0xAAu8; 512];
        d.read_blocks(3, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0), "fresh device must read all zeros");
        let _ = std::fs::remove_file(&path);
    }
}
