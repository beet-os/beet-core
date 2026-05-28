// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Generic NVMe 1.4 protocol layer.
//!
//! NVMe is the storage protocol Apple M1's `ANS` controller speaks
//! at the queue level (same Submission Queue Entry / Completion
//! Queue Entry layout, same opcodes), even though the transport
//! to the silicon is Apple's `ASC` mailbox instead of PCIe.
//! Standard PCIe NVMe SSDs use the exact same queue format too, so
//! this module also benefits future RPi5-NVMe-slot work.
//!
//! Like [`crate::pcie`] and [`crate::sdhci`], the design is:
//!
//!   - **Pure / mockable** here: Submission Queue Entry (SQE) and
//!     Completion Queue Entry (CQE) encode/decode, opcode tables,
//!     identify-controller / identify-namespace parsing, error-code
//!     mapping.
//!   - **Hardware-bound** in the per-platform driver: the actual
//!     doorbell registers, IRQ wiring, DMA buffer management, and
//!     in Apple's case the ASC mailbox + DART IOMMU.
//!
//! References:
//!   - NVM-Express specification 1.4
//!   - Linux `drivers/nvme/host/core.c` (queue logic)
//!   - Linux `drivers/nvme/host/apple.c` (Apple ANS specifics)
//!   - Asahi `apple-nvme` Rust driver

#![allow(dead_code)]

// ─────────────────────────────────────────────────────────────────────────────
// NVMe Admin + I/O opcodes (NVM-Express 1.4, sections 5 and 6)
// ─────────────────────────────────────────────────────────────────────────────

#[allow(non_snake_case)]
pub mod admin_opc {
    pub const DELETE_IO_SQ:      u8 = 0x00;
    pub const CREATE_IO_SQ:      u8 = 0x01;
    pub const GET_LOG_PAGE:      u8 = 0x02;
    pub const DELETE_IO_CQ:      u8 = 0x04;
    pub const CREATE_IO_CQ:      u8 = 0x05;
    pub const IDENTIFY:          u8 = 0x06;
    pub const ABORT:             u8 = 0x08;
    pub const SET_FEATURES:      u8 = 0x09;
    pub const GET_FEATURES:      u8 = 0x0A;
    pub const ASYNC_EVENT_REQ:   u8 = 0x0C;
    pub const NAMESPACE_MGMT:    u8 = 0x0D;
    pub const FIRMWARE_COMMIT:   u8 = 0x10;
    pub const FIRMWARE_DOWNLOAD: u8 = 0x11;
}

#[allow(non_snake_case)]
pub mod nvm_opc {
    pub const FLUSH:        u8 = 0x00;
    pub const WRITE:        u8 = 0x01;
    pub const READ:         u8 = 0x02;
    pub const WRITE_UNCOR:  u8 = 0x04;
    pub const COMPARE:      u8 = 0x05;
    pub const WRITE_ZEROES: u8 = 0x08;
    pub const DSM:          u8 = 0x09; // dataset management
}

/// CNS (Controller / Namespace Structure) values for the IDENTIFY admin
/// opcode. Used in CDW10.
pub mod cns {
    pub const IDENTIFY_NAMESPACE:           u8 = 0x00;
    pub const IDENTIFY_CONTROLLER:          u8 = 0x01;
    pub const ACTIVE_NAMESPACES:            u8 = 0x02;
    pub const ALLOCATED_NAMESPACES:         u8 = 0x10;
    pub const IDENTIFY_NAMESPACE_ALLOCATED: u8 = 0x11;
}

// ─────────────────────────────────────────────────────────────────────────────
// Submission Queue Entry (SQE) — 64 bytes, NVMe 1.4 §4.2
// ─────────────────────────────────────────────────────────────────────────────

/// One NVMe submission queue entry. 64 bytes wide as defined by the
/// spec — laid out exactly so the kernel can DMA the slice to the
/// controller without further marshalling.
///
/// `Default::default()` clears every field; populate the variants
/// you need (opcode + namespace + PRPs + command dwords) per
/// operation.
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Sqe {
    /// Command dword 0 — packs opcode + flags + command identifier.
    pub cdw0: u32,
    /// Namespace ID (0 for admin commands without an NSID, 1+ for I/O).
    pub nsid: u32,
    /// Reserved (spec §4.2 — must be zero on write).
    pub rsvd0: u64,
    /// Metadata pointer.
    pub mptr: u64,
    /// Physical Region Page entry 1 (typically the buffer's first page PA).
    pub prp1: u64,
    /// Physical Region Page entry 2 (next page or PRP list pointer).
    pub prp2: u64,
    /// Command-specific dwords.
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

impl Sqe {
    /// Build an IDENTIFY admin command targeting `cns` for `nsid`,
    /// landing the result at the buffer addressed by `prp1` (PA).
    pub fn identify(cid: u16, cns: u8, nsid: u32, prp1: u64) -> Self {
        let mut s = Self::default();
        s.cdw0 = pack_cdw0(admin_opc::IDENTIFY, 0, cid);
        s.nsid = nsid;
        s.prp1 = prp1;
        s.cdw10 = cns as u32;
        s
    }

    /// I/O Read: read `n_blocks` 512 B (or namespace LBA size) blocks
    /// starting at `slba` into the buffer addressed by `prp1` (PA).
    pub fn read(cid: u16, nsid: u32, slba: u64, n_blocks: u16, prp1: u64) -> Self {
        let mut s = Self::default();
        s.cdw0 = pack_cdw0(nvm_opc::READ, 0, cid);
        s.nsid = nsid;
        s.prp1 = prp1;
        // CDW10/11 hold the 64-bit Starting LBA, low then high.
        s.cdw10 = (slba & 0xFFFF_FFFF) as u32;
        s.cdw11 = (slba >> 32) as u32;
        // CDW12 low 16 bits = (Number of Logical Blocks - 1).
        s.cdw12 = (n_blocks.saturating_sub(1)) as u32;
        s
    }

    /// I/O Write: mirror of [`Sqe::read`] with the WRITE opcode.
    pub fn write(cid: u16, nsid: u32, slba: u64, n_blocks: u16, prp1: u64) -> Self {
        let mut s = Self::read(cid, nsid, slba, n_blocks, prp1);
        s.cdw0 = pack_cdw0(nvm_opc::WRITE, 0, cid);
        s
    }

    /// Flush (NVM opcode 0x00).
    pub fn flush(cid: u16, nsid: u32) -> Self {
        let mut s = Self::default();
        s.cdw0 = pack_cdw0(nvm_opc::FLUSH, 0, cid);
        s.nsid = nsid;
        s
    }
}

/// Pack the CDW0 register of an SQE: opcode | fuse | flags | CID.
#[inline]
pub const fn pack_cdw0(opcode: u8, flags: u8, cid: u16) -> u32 {
    (opcode as u32)
        | ((flags as u32) << 8)
        | ((cid as u32) << 16)
}

#[inline]
pub const fn opcode_of(cdw0: u32) -> u8 { (cdw0 & 0xFF) as u8 }
#[inline]
pub const fn cid_of(cdw0: u32) -> u16 { ((cdw0 >> 16) & 0xFFFF) as u16 }

// ─────────────────────────────────────────────────────────────────────────────
// Completion Queue Entry (CQE) — 16 bytes, NVMe 1.4 §4.6
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Cqe {
    /// Command-specific data (DW0/DW1) — most operations leave these 0.
    pub dw0: u32,
    pub dw1: u32,
    /// SQ head pointer + SQ identifier (DW2: SQHD | SQID).
    pub sq_head_sqid: u32,
    /// CID | P (phase tag, bit 16) | status field (bits 17..31).
    pub cid_phase_status: u32,
}

impl Cqe {
    pub fn cid(&self) -> u16 { (self.cid_phase_status & 0xFFFF) as u16 }
    /// Phase tag — alternates between 0 and 1 each pass through the
    /// queue so the consumer can tell new entries from stale ones.
    pub fn phase(&self) -> bool { (self.cid_phase_status & (1 << 16)) != 0 }
    /// Raw status field — see [`StatusField`] for typed decoding.
    pub fn status_raw(&self) -> u16 { ((self.cid_phase_status >> 17) & 0x7FFF) as u16 }
    pub fn status(&self) -> StatusField { StatusField::from_raw(self.status_raw()) }
    pub fn sq_head(&self) -> u16 { (self.sq_head_sqid & 0xFFFF) as u16 }
    pub fn sqid(&self) -> u16 { ((self.sq_head_sqid >> 16) & 0xFFFF) as u16 }
}

// ─────────────────────────────────────────────────────────────────────────────
// Status field — NVMe 1.4 §4.6.1
// ─────────────────────────────────────────────────────────────────────────────

/// Decoded NVMe status field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusField {
    /// Status Code Type (3 bits) — Generic / Specific / Path / Vendor.
    pub sct: u8,
    /// Status Code (8 bits) — interpretation depends on SCT.
    pub sc:  u8,
    /// Do Not Retry — if set, the host shouldn't reissue the command.
    pub dnr: bool,
    /// More — additional information available in error log.
    pub more: bool,
}

impl StatusField {
    pub fn from_raw(raw: u16) -> Self {
        Self {
            sct:  ((raw >> 8) & 0x07) as u8,
            sc:   (raw & 0xFF) as u8,
            dnr:  (raw & 0x4000) != 0,
            more: (raw & 0x2000) != 0,
        }
    }
    /// True when the SCT == 0 (Generic) and SC == 0 (Successful
    /// Completion).
    pub fn is_success(&self) -> bool { self.sct == 0 && self.sc == 0 }
}

/// SCT values from NVMe spec.
pub mod sct {
    pub const GENERIC:           u8 = 0x0;
    pub const COMMAND_SPECIFIC:  u8 = 0x1;
    pub const MEDIA_DATA_ERROR:  u8 = 0x2;
    pub const PATH:              u8 = 0x3;
    pub const VENDOR_SPECIFIC:   u8 = 0x7;
}

/// Common Generic SC values (NVMe 1.4 Figure 124).
pub mod sc_generic {
    pub const SUCCESS:                u8 = 0x00;
    pub const INVALID_COMMAND_OPCODE: u8 = 0x01;
    pub const INVALID_FIELD:          u8 = 0x02;
    pub const CID_CONFLICT:           u8 = 0x03;
    pub const DATA_TRANSFER_ERROR:    u8 = 0x04;
    pub const ABORTED_POWER_LOSS:     u8 = 0x05;
    pub const INTERNAL_ERROR:         u8 = 0x06;
    pub const ABORTED_BY_REQUEST:     u8 = 0x07;
    pub const ABORTED_SQ_DELETION:    u8 = 0x08;
    pub const NS_NOT_READY:           u8 = 0x82;
    pub const NS_NOT_ATTACHED:        u8 = 0x0B;
    pub const LBA_OUT_OF_RANGE:       u8 = 0x80;
    pub const CAPACITY_EXCEEDED:      u8 = 0x81;
}

// ─────────────────────────────────────────────────────────────────────────────
// IDENTIFY data structures (NVMe 1.4 §5.15)
// ─────────────────────────────────────────────────────────────────────────────

/// Subset of the 4 KB Identify Controller data structure that BeetOS
/// actually cares about today: vendor/model/firmware strings, page
/// size geometry, and the number of I/O queues the controller
/// supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerInfo {
    pub vid:           u16,
    pub ssvid:         u16,
    pub serial:        [u8; 20],
    pub model:         [u8; 40],
    pub firmware_rev:  [u8; 8],
    /// Maximum data transfer size (MDTS) — 2^(MDTS) in units of CAP.MPSMIN
    /// page size. 0 = unlimited.
    pub mdts:          u8,
}

impl ControllerInfo {
    /// Parse from a 4096-byte IDENTIFY_CONTROLLER response buffer.
    pub fn parse(buf: &[u8]) -> Self {
        assert!(buf.len() >= 4096, "identify-controller buf must be 4 KB");
        let read_u16 = |off| u16::from_le_bytes([buf[off], buf[off + 1]]);
        let mut serial = [0u8; 20]; serial.copy_from_slice(&buf[4..24]);
        let mut model = [0u8; 40]; model.copy_from_slice(&buf[24..64]);
        let mut firmware_rev = [0u8; 8]; firmware_rev.copy_from_slice(&buf[64..72]);
        ControllerInfo {
            vid:          read_u16(0),
            ssvid:        read_u16(2),
            serial,
            model,
            firmware_rev,
            mdts:         buf[77],
        }
    }
}

/// Subset of the 4 KB Identify Namespace data structure (NVMe 1.4 §5.15.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceInfo {
    /// Total namespace size in LBAs.
    pub size_lba:     u64,
    /// Capacity (used by thin provisioning).
    pub capacity_lba: u64,
    /// Number of LBA formats - 1; we only inspect the active one.
    pub n_lba_formats: u8,
    /// LBA Format index in use (FLBAS, low 4 bits).
    pub lba_format_idx: u8,
    /// LBA Data Size in bytes (2^lbads from active LBAF entry).
    pub block_size:   u32,
}

impl NamespaceInfo {
    pub fn parse(buf: &[u8]) -> Self {
        assert!(buf.len() >= 4096, "identify-namespace buf must be 4 KB");
        let size_lba     = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let capacity_lba = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let flbas         = buf[26] & 0x0F;
        let n_lba_formats = buf[25]; // NLBAF
        // LBAFn starts at offset 128; each entry is 4 bytes:
        //   LBA Data Size (5 bits, exponent) at byte 2.
        let lbaf_off = 128 + (flbas as usize) * 4;
        let lbads_exp = (buf[lbaf_off + 2] & 0x1F) as u32;
        let block_size = 1u32 << lbads_exp;
        NamespaceInfo {
            size_lba, capacity_lba,
            n_lba_formats, lba_format_idx: flbas,
            block_size,
        }
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.size_lba * self.block_size as u64
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Submission / Completion queue rings (in-memory; transport-agnostic)
// ─────────────────────────────────────────────────────────────────────────────

/// Generic SQ ring — pure index arithmetic, no MMIO. The platform-
/// specific driver owns the actual buffer + writes the doorbell.
pub struct SubmissionQueue {
    pub depth: u16,
    pub tail:  u16,
}

impl SubmissionQueue {
    pub fn new(depth: u16) -> Self { Self { depth, tail: 0 } }

    /// Compute the next tail index. The platform driver writes the
    /// SQE at this slot, then bumps the doorbell.
    pub fn advance_tail(&mut self) -> u16 {
        let cur = self.tail;
        self.tail = (cur + 1) % self.depth;
        cur
    }
}

/// Generic CQ ring — owns a phase bit and the head index. Consumers
/// poll for "CQE.phase == expected_phase", then advance head + flip
/// phase when wrapping.
pub struct CompletionQueue {
    pub depth: u16,
    pub head:  u16,
    pub phase: bool,
}

impl CompletionQueue {
    pub fn new(depth: u16) -> Self { Self { depth, head: 0, phase: true } }

    /// Indicate that the slot at `head` was a valid completion; bump
    /// head, flip phase if we wrapped.
    pub fn pop(&mut self) {
        let next = self.head + 1;
        if next >= self.depth {
            self.head = 0;
            self.phase = !self.phase;
        } else {
            self.head = next;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SQE encoding ────────────────────────────────────────────────────

    #[test]
    fn sqe_struct_is_64_bytes() {
        // NVMe spec: each SQE is exactly 64 bytes so the controller
        // can DMA without further reshuffling.
        assert_eq!(core::mem::size_of::<Sqe>(), 64);
    }

    #[test]
    fn cqe_struct_is_16_bytes() {
        assert_eq!(core::mem::size_of::<Cqe>(), 16);
    }

    #[test]
    fn pack_cdw0_packs_fields_correctly() {
        let w = pack_cdw0(admin_opc::IDENTIFY, 0, 0x1234);
        assert_eq!(opcode_of(w), admin_opc::IDENTIFY);
        assert_eq!(cid_of(w), 0x1234);
        // No flags set.
        assert_eq!((w >> 8) & 0xFF, 0);
    }

    #[test]
    fn sqe_identify_packs_cns_and_nsid() {
        let s = Sqe::identify(42, cns::IDENTIFY_CONTROLLER, 0, 0xDEAD_BEEF_CAFE);
        assert_eq!(opcode_of(s.cdw0), admin_opc::IDENTIFY);
        assert_eq!(cid_of(s.cdw0), 42);
        assert_eq!(s.cdw10 as u8, cns::IDENTIFY_CONTROLLER);
        assert_eq!(s.prp1, 0xDEAD_BEEF_CAFE);
    }

    #[test]
    fn sqe_read_packs_64bit_lba_split_across_cdw10_11() {
        let s = Sqe::read(7, 1, 0x0000_0001_BEEF_CAFE, 8, 0x1000);
        assert_eq!(opcode_of(s.cdw0), nvm_opc::READ);
        assert_eq!(s.cdw10, 0xBEEF_CAFE);
        assert_eq!(s.cdw11, 0x0000_0001);
        // n_blocks - 1 = 7.
        assert_eq!(s.cdw12 & 0xFFFF, 7);
        assert_eq!(s.prp1, 0x1000);
        assert_eq!(s.nsid, 1);
    }

    #[test]
    fn sqe_write_differs_from_read_only_in_opcode() {
        let r = Sqe::read(1, 2, 100, 1, 0x4000);
        let w = Sqe::write(1, 2, 100, 1, 0x4000);
        assert_ne!(r, w);
        assert_eq!(opcode_of(r.cdw0), nvm_opc::READ);
        assert_eq!(opcode_of(w.cdw0), nvm_opc::WRITE);
        // Other fields match.
        assert_eq!(r.cdw10, w.cdw10);
        assert_eq!(r.cdw11, w.cdw11);
        assert_eq!(r.cdw12, w.cdw12);
        assert_eq!(r.prp1,  w.prp1);
        assert_eq!(r.nsid,  w.nsid);
    }

    #[test]
    fn sqe_flush_only_carries_opcode_and_nsid() {
        let f = Sqe::flush(99, 3);
        assert_eq!(opcode_of(f.cdw0), nvm_opc::FLUSH);
        assert_eq!(cid_of(f.cdw0), 99);
        assert_eq!(f.nsid, 3);
        assert_eq!(f.prp1, 0);
        assert_eq!(f.cdw10, 0);
    }

    // ── CQE decoding ────────────────────────────────────────────────────

    #[test]
    fn cqe_decodes_cid_phase_status() {
        // Successful completion of CID 0xABCD with phase=1, status=0.
        let cqe = Cqe {
            dw0: 0, dw1: 0,
            sq_head_sqid: (5 << 16) | 0x10, // SQID=5, SQHD=16
            cid_phase_status: 0xABCD | (1 << 16) /* phase */,
        };
        assert_eq!(cqe.cid(), 0xABCD);
        assert!(cqe.phase());
        assert!(cqe.status().is_success());
        assert_eq!(cqe.sqid(), 5);
        assert_eq!(cqe.sq_head(), 16);
    }

    #[test]
    fn cqe_status_decodes_invalid_field() {
        // SCT=0 (generic), SC=0x02 (invalid field), DNR=1, More=0.
        let status_raw: u16 = sc_generic::INVALID_FIELD as u16 | (sct::GENERIC as u16) << 8 | 0x4000 /* DNR */;
        let cqe = Cqe {
            dw0: 0, dw1: 0,
            sq_head_sqid: 0,
            cid_phase_status: (status_raw as u32) << 17,
        };
        let s = cqe.status();
        assert_eq!(s.sct, sct::GENERIC);
        assert_eq!(s.sc, sc_generic::INVALID_FIELD);
        assert!(s.dnr);
        assert!(!s.more);
        assert!(!s.is_success());
    }

    #[test]
    fn status_field_extracts_lba_out_of_range() {
        let raw: u16 = sc_generic::LBA_OUT_OF_RANGE as u16;
        let s = StatusField::from_raw(raw);
        assert_eq!(s.sc, sc_generic::LBA_OUT_OF_RANGE);
        assert!(!s.is_success());
    }

    // ── Identify-controller parse ───────────────────────────────────────

    #[test]
    fn controller_info_parses_strings_and_ids() {
        let mut buf = [0u8; 4096];
        buf[0] = 0x6B; buf[1] = 0x10;  // VID = 0x106B (Apple)
        buf[2] = 0xCD; buf[3] = 0xAB;  // SSVID
        buf[4..24].copy_from_slice(b"SN1234567890        ");
        buf[24..64].copy_from_slice(b"APPLE SSD AP1024Q                       ");
        buf[64..72].copy_from_slice(b"FW17    ");
        buf[77] = 5; // MDTS = 5 → 2^5 × min page size

        let ci = ControllerInfo::parse(&buf);
        assert_eq!(ci.vid, 0x106B);
        assert_eq!(ci.ssvid, 0xABCD);
        assert_eq!(&ci.serial[..14], b"SN1234567890  ");
        assert_eq!(&ci.model[..15], b"APPLE SSD AP102");
        assert_eq!(&ci.firmware_rev, b"FW17    ");
        assert_eq!(ci.mdts, 5);
    }

    // ── Identify-namespace parse ────────────────────────────────────────

    #[test]
    fn namespace_info_decodes_capacity_and_block_size() {
        let mut buf = [0u8; 4096];
        // NSZE = 0x40000 LBAs (256 K LBAs)
        buf[0..8].copy_from_slice(&0x0000_0000_0004_0000u64.to_le_bytes());
        // NCAP = same
        buf[8..16].copy_from_slice(&0x0000_0000_0004_0000u64.to_le_bytes());
        // NLBAF = 1, FLBAS = 0 (using LBAF0).
        buf[25] = 1;
        buf[26] = 0;
        // LBAF0 at offset 128: LBA Data Size exponent = 12 → 4096 B blocks.
        buf[128 + 2] = 12;

        let ns = NamespaceInfo::parse(&buf);
        assert_eq!(ns.size_lba, 0x40000);
        assert_eq!(ns.block_size, 4096);
        // Apple NVMe drives commonly use 4 KB blocks.
        assert_eq!(ns.capacity_bytes(), 0x40000 * 4096);
    }

    // ── SQ / CQ ring tests ──────────────────────────────────────────────

    #[test]
    fn sq_advances_and_wraps() {
        let mut sq = SubmissionQueue::new(4);
        assert_eq!(sq.advance_tail(), 0);
        assert_eq!(sq.advance_tail(), 1);
        assert_eq!(sq.advance_tail(), 2);
        assert_eq!(sq.advance_tail(), 3);
        // Wraps modulo depth.
        assert_eq!(sq.advance_tail(), 0);
    }

    #[test]
    fn cq_pop_flips_phase_on_wrap() {
        let mut cq = CompletionQueue::new(4);
        assert!(cq.phase);
        cq.pop(); cq.pop(); cq.pop();
        assert!(cq.phase); // still on initial pass
        cq.pop();
        // Wrapped — phase must have flipped.
        assert!(!cq.phase);
        assert_eq!(cq.head, 0);
    }
}
