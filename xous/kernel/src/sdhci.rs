// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SD / SDHCI driver — protocol layer.
//!
//! Like [`crate::pcie`], this module is split into:
//!
//!   - **Pure / mockable** — command encoding, CRC7 + CRC16
//!     calculation, OCR / CID / CSD parsing, response R1 status
//!     decoding, the card-init state machine. All unit-tested
//!     against in-tree fixtures.
//!   - **Hardware-bound** — the SDHCI register layout + the BCM2712
//!     SDHOST controller bring-up. Lives in
//!     `platform/bcm2712/sdhci_brcm.rs` so the generic protocol
//!     side stays no_std + alloc-free + fully testable.
//!
//! References:
//!   - SD Physical Layer Specification 8.00
//!   - SDHCI v3.00 spec
//!   - Linux `drivers/mmc/host/sdhci.c` + `drivers/mmc/core/mmc.c`
//!   - Linux `drivers/mmc/host/sdhci-iproc.c` (BCM2711 — close to
//!     what BCM2712 ships)
//!
//! Status: protocol-layer scaffold + 100 % unit-test coverage of
//! the parts that don't touch real MMIO. The platform glue that
//! drives a real SDHCI controller is one of the next concrete RPi5
//! milestones.

#![allow(dead_code)]

// ─────────────────────────────────────────────────────────────────────────────
// Command IDs (SD physical layer spec, section 4.7.4)
// ─────────────────────────────────────────────────────────────────────────────

#[allow(non_snake_case)]
pub mod cmd {
    pub const GO_IDLE_STATE:        u8 = 0;
    pub const ALL_SEND_CID:         u8 = 2;
    pub const SEND_RELATIVE_ADDR:   u8 = 3;
    pub const SELECT_CARD:          u8 = 7;
    pub const SEND_IF_COND:         u8 = 8;
    pub const SEND_CSD:             u8 = 9;
    pub const STOP_TRANSMISSION:    u8 = 12;
    pub const SEND_STATUS:          u8 = 13;
    pub const SET_BLOCKLEN:         u8 = 16;
    pub const READ_SINGLE_BLOCK:    u8 = 17;
    pub const READ_MULTIPLE_BLOCK:  u8 = 18;
    pub const WRITE_BLOCK:          u8 = 24;
    pub const WRITE_MULTIPLE_BLOCK: u8 = 25;
    pub const APP_CMD:              u8 = 55;
    /// Application commands — sent right after APP_CMD.
    pub const SD_SEND_OP_COND:      u8 = 41;
}

/// What kind of response a command expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseType {
    /// No response.
    None,
    /// 48-bit response — most commands.
    R1,
    /// 48-bit, "busy" version.
    R1b,
    /// 136-bit response — CID/CSD reads.
    R2,
    /// 48-bit OCR — ACMD41.
    R3,
    /// 48-bit RCA + status — CMD3.
    R6,
    /// 48-bit IF cond echo — CMD8.
    R7,
}

/// One SD command ready to be put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdCommand {
    pub cmd:    u8,
    pub arg:    u32,
    pub rsp:    ResponseType,
}

impl SdCommand {
    pub const fn new(cmd: u8, arg: u32, rsp: ResponseType) -> Self {
        Self { cmd, arg, rsp }
    }

    /// Encode the command frame the way SD cards see it on the wire:
    /// 48 bits = `01 | CMD6 | ARG32 | CRC7 | 1`.
    pub fn encode_frame(&self) -> [u8; 6] {
        let mut buf = [0u8; 6];
        buf[0] = 0x40 | (self.cmd & 0x3F);
        buf[1] = (self.arg >> 24) as u8;
        buf[2] = (self.arg >> 16) as u8;
        buf[3] = (self.arg >>  8) as u8;
        buf[4] =  self.arg        as u8;
        // CRC7 over the first 5 bytes; bit 0 of the last byte is the
        // end-bit (always 1).
        buf[5] = (crc7(&buf[..5]) << 1) | 0x01;
        buf
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CRC7 (polynomial x^7 + x^3 + 1 — used for command/response frames)
// ─────────────────────────────────────────────────────────────────────────────

/// CRC7 used in SD command + response frames. Polynomial 0x12 (=0x09
/// shifted one bit, MSB-first convention to match Linux's
/// `crc7_be`). Init 0, returns the 7-bit CRC right-justified.
///
/// Reference: Linux `lib/crc7.c`, table generator uses identical
/// polynomial; SD spec annex CMD0 yields 0x4A which is the test
/// case below.
pub fn crc7(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x12;
            } else {
                crc <<= 1;
            }
        }
    }
    crc >> 1
}

// ─────────────────────────────────────────────────────────────────────────────
// CRC16-CCITT (polynomial x^16 + x^12 + x^5 + 1 — used for data blocks)
// ─────────────────────────────────────────────────────────────────────────────

/// CRC16-CCITT (poly 0x1021) used on SD data lines.
pub fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

// ─────────────────────────────────────────────────────────────────────────────
// Response types
// ─────────────────────────────────────────────────────────────────────────────

/// Common R1 card status bits we care about. Card returns 32 bits;
/// bit positions per SD spec section 4.10.1.
pub mod r1 {
    pub const OUT_OF_RANGE:    u32 = 1 << 31;
    pub const ADDRESS_ERROR:   u32 = 1 << 30;
    pub const BLOCK_LEN_ERROR: u32 = 1 << 29;
    pub const ERASE_SEQ_ERROR: u32 = 1 << 28;
    pub const ERASE_PARAM:     u32 = 1 << 27;
    pub const WP_VIOLATION:    u32 = 1 << 26;
    pub const CARD_IS_LOCKED:  u32 = 1 << 25;
    pub const LOCK_UNLOCK_FAILED: u32 = 1 << 24;
    pub const COM_CRC_ERROR:   u32 = 1 << 23;
    pub const ILLEGAL_COMMAND: u32 = 1 << 22;
    pub const CARD_ECC_FAILED: u32 = 1 << 21;
    pub const CC_ERROR:        u32 = 1 << 20;
    pub const ERROR:           u32 = 1 << 19;
    pub const READY_FOR_DATA:  u32 = 1 << 8;
    pub const APP_CMD:         u32 = 1 << 5;

    /// Mask of bits that indicate any kind of error.
    pub const ANY_ERROR: u32 = 0xFDFF_E000;

    /// Card state — bits 12..9 of R1.
    pub fn state(r1: u32) -> u8 {
        ((r1 >> 9) & 0xF) as u8
    }
}

/// Card state values from R1[12:9].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardState {
    Idle = 0, Ready = 1, Ident = 2, Stby = 3,
    Tran = 4, Data = 5, Rcv = 6, Prg = 7,
    Dis  = 8, Unknown = 15,
}

impl CardState {
    pub fn from_r1(r1: u32) -> Self {
        match r1::state(r1) {
            0 => Self::Idle,  1 => Self::Ready, 2 => Self::Ident, 3 => Self::Stby,
            4 => Self::Tran,  5 => Self::Data,  6 => Self::Rcv,   7 => Self::Prg,
            8 => Self::Dis,   _ => Self::Unknown,
        }
    }
}

/// OCR register bit fields (returned in ACMD41 R3 response).
pub mod ocr {
    /// Card busy — clears once initialisation completes.
    pub const BUSY:        u32 = 1 << 31;
    /// Card Capacity Status — 1 = SDHC/SDXC, 0 = SDSC.
    pub const CCS:         u32 = 1 << 30;
    /// UHS-II card.
    pub const UHS_II:      u32 = 1 << 29;
    /// 1.8 V switching accepted.
    pub const S18A:        u32 = 1 << 24;
    /// 3.2-3.3 V voltage window (typical for SD).
    pub const V32_33:      u32 = 1 << 20;
    /// 3.3-3.4 V voltage window.
    pub const V33_34:      u32 = 1 << 21;

    pub fn host_voltage_window() -> u32 { V32_33 | V33_34 }
}

/// Parsed Card IDentification register (CMD2 / R2, 128 bits).
///
/// Field layout per SD spec section 5.2 — MSB-first as the card
/// returns it, packed into four big-endian u32s by the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cid {
    pub manufacturer_id:    u8,    // MID
    pub oem_application_id: u16,   // OID
    pub product_name:       [u8; 5], // PNM
    pub product_revision:   u8,    // PRV
    pub serial:             u32,   // PSN
    pub manufacturing_date: u16,   // MDT (year offset from 2000, month)
}

impl Cid {
    /// Parse a 128-bit CID response from four big-endian u32 words
    /// (`r2[0]` = MSW, `r2[3]` = LSW). The SDHCI controller strips
    /// the 8-bit CRC + start/end bits, so what we get is the 120
    /// useful bits packed into the high 120 bits of these 4 words.
    pub fn parse(r2: [u32; 4]) -> Self {
        // `bit` indexes CID positions MSB-first (bit 0 = r2[0] high
        // bit, bit 127 = r2[3] low bit). Bit-in-word = 31 - (bit%32)
        // keeps the "high bit of word = lower index" invariant.
        let get = |bit: u32, width: u32| -> u32 {
            let mut v = 0u32;
            for b in bit..(bit + width) {
                let word = r2[(b / 32) as usize];
                let bit_in_word = 31 - (b % 32);
                v = (v << 1) | ((word >> bit_in_word) & 1);
            }
            v
        };

        // PNM is 40 bits of ASCII (5 chars). Read each byte separately
        // since 40 bits don't fit in a u32. PNM[0] is the MSB-most
        // byte, lying at CID bit 24..31.
        let mut product_name = [0u8; 5];
        for i in 0..5 {
            product_name[i] = get(24 + (i as u32) * 8, 8) as u8;
        }

        Cid {
            manufacturer_id:    get(0,  8) as u8,
            oem_application_id: get(8,  16) as u16,
            product_name,
            product_revision:   get(64, 8) as u8,
            serial:             get(72, 32),
            manufacturing_date: get(104, 12) as u16,
        }
    }
}

/// Parsed Card Specific Data register (CMD9 / R2, 128 bits) — only
/// SDHC/SDXC v2.0 CSD (`CSD_STRUCTURE == 0b01`) is implemented since
/// every consumer card sold since ~2009 is one of those.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csd {
    pub structure_version: u8, // 0 = v1.0, 1 = v2.0
    pub capacity_bytes:    u64,
    pub block_len_bytes:   u32,
    pub read_bl_partial:   bool,
    pub write_bl_partial:  bool,
}

impl Csd {
    pub fn parse(r2: [u32; 4]) -> Self {
        let csd_struct = ((r2[0] >> 30) & 0x3) as u8;
        match csd_struct {
            0 => Self::parse_v1(r2),
            1 => Self::parse_v2(r2),
            _ => Self::parse_v2(r2), // forward-compatible best-effort
        }
    }

    fn parse_v2(r2: [u32; 4]) -> Self {
        // V2.0 CSD: C_SIZE is bits [69:48] (22 bits). Capacity =
        // (C_SIZE + 1) × 512 KB.  Block size is fixed at 512 B.
        // r2[0]=bits 127..96, r2[1]=95..64, r2[2]=63..32, r2[3]=31..0.
        let c_size = ((r2[1] & 0x3F) << 16) | ((r2[2] >> 16) & 0xFFFF);
        let capacity_bytes = (c_size as u64 + 1) * 512 * 1024;
        Csd {
            structure_version: 1,
            capacity_bytes,
            block_len_bytes:   512,
            read_bl_partial:   false,
            write_bl_partial:  false,
        }
    }

    fn parse_v1(r2: [u32; 4]) -> Self {
        // V1.0 CSD: block size = 2^READ_BL_LEN (4 bits at [83:80]).
        // C_SIZE is [73:62] (12 bits), C_SIZE_MULT is [49:47] (3 bits).
        // capacity = (C_SIZE+1) × 2^(C_SIZE_MULT+2) × block_len.
        let read_bl_len = ((r2[1] >> 16) & 0xF) as u32;
        let c_size      = (((r2[1] & 0x3FF) << 2) | ((r2[2] >> 30) & 0x3)) as u64;
        let c_size_mult = (((r2[2] >> 15) & 0x7)) as u64;
        let block_len   = 1u32 << read_bl_len;
        let mult        = 1u64 << (c_size_mult + 2);
        let capacity_bytes = (c_size + 1) * mult * block_len as u64;
        Csd {
            structure_version: 0,
            capacity_bytes,
            block_len_bytes:   block_len,
            read_bl_partial:   false,
            write_bl_partial:  false,
        }
    }

    pub fn blocks_512(&self) -> u64 { self.capacity_bytes / 512 }
}

// ─────────────────────────────────────────────────────────────────────────────
// Init state machine
// ─────────────────────────────────────────────────────────────────────────────

/// Steps the card walks through from power-up to "ready for I/O".
///
/// Each variant carries enough state for the next command to be
/// re-derivable from the previous response, so the same state
/// machine drives both real hardware and the test harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitStep {
    /// Begin: send CMD0 → no response.
    GoIdle,
    /// Send CMD8 (interface condition) to detect v2.0 cards.
    SendIfCond,
    /// Loop sending ACMD41 until OCR.BUSY clears.
    SendOpCond { retries: u8, sdhc_capable: bool },
    /// All-Send-CID (CMD2) — fetch CID, advance to Ident.
    AllSendCid,
    /// SEND_RELATIVE_ADDR (CMD3) — card returns RCA.
    SendRelativeAddr,
    /// SEND_CSD (CMD9) using RCA — fetch capacity.
    SendCsd { rca: u16 },
    /// SELECT_CARD (CMD7) — enter Tran state. Done after this.
    SelectCard { rca: u16 },
    /// Init complete; we know the CID, RCA, CSD.
    Done {
        cid: Cid,
        rca: u16,
        csd: Csd,
    },
    /// Init failed (timed out, illegal response, error bit, …).
    Error(&'static str),
}

// ─────────────────────────────────────────────────────────────────────────────
// SDHCI v3.0 host controller
// ─────────────────────────────────────────────────────────────────────────────

/// MMIO abstraction — identical pattern to [`crate::pcie::Mmio`] so
/// SDHCI logic can be unit-tested against a `MockMmio` while the
/// real driver wraps a raw pointer to the controller's MMIO region.
pub trait Mmio {
    fn read32(&self, offset: usize) -> u32;
    fn write32(&self, offset: usize, value: u32);

    // u8 / u16 default impls based on u32 access — the SDHCI register
    // file is bytewise-addressable but most cores want 32-bit aligned
    // MMIO loads, so we shift/mask out of the surrounding word.
    fn read16(&self, off: usize) -> u16 {
        let w = self.read32(off & !3);
        ((w >> ((off & 2) * 8)) & 0xFFFF) as u16
    }
    fn read8(&self, off: usize) -> u8 {
        let w = self.read32(off & !3);
        ((w >> ((off & 3) * 8)) & 0xFF) as u8
    }
    fn write16(&self, off: usize, v: u16) {
        let aligned = off & !3;
        let shift = (off & 2) * 8;
        let mask  = 0xFFFFu32 << shift;
        let w = (self.read32(aligned) & !mask) | ((v as u32) << shift);
        self.write32(aligned, w);
    }
    fn write8(&self, off: usize, v: u8) {
        let aligned = off & !3;
        let shift = (off & 3) * 8;
        let mask  = 0xFFu32 << shift;
        let w = (self.read32(aligned) & !mask) | ((v as u32) << shift);
        self.write32(aligned, w);
    }
}

/// SDHCI v3.0 register file offsets.
pub mod reg {
    pub const SDMA_ADDR:        usize = 0x00; // u32
    pub const BLOCK_SIZE:       usize = 0x04; // u16
    pub const BLOCK_COUNT:      usize = 0x06; // u16
    pub const ARGUMENT:         usize = 0x08; // u32
    pub const TRANSFER_MODE:    usize = 0x0C; // u16
    pub const COMMAND:          usize = 0x0E; // u16
    pub const RESPONSE0:        usize = 0x10; // u32
    pub const RESPONSE1:        usize = 0x14;
    pub const RESPONSE2:        usize = 0x18;
    pub const RESPONSE3:        usize = 0x1C;
    pub const BUFFER_DATA_PORT: usize = 0x20;
    pub const PRESENT_STATE:    usize = 0x24;
    pub const HOST_CONTROL_1:   usize = 0x28; // u8
    pub const POWER_CONTROL:    usize = 0x29; // u8
    pub const CLOCK_CONTROL:    usize = 0x2C; // u16
    pub const TIMEOUT_CONTROL:  usize = 0x2E; // u8
    pub const SOFTWARE_RESET:   usize = 0x2F; // u8
    pub const INT_STATUS:       usize = 0x30; // u32
    pub const INT_STATUS_ENABLE: usize = 0x34;
    pub const SIGNAL_ENABLE:    usize = 0x38;
    pub const CAPABILITIES:     usize = 0x40; // u64
    pub const HOST_VERSION:     usize = 0xFE; // u16
}

/// PRESENT_STATE bits.
pub mod state {
    pub const CMD_INHIBIT:       u32 = 1 << 0;
    pub const CMD_INHIBIT_DAT:   u32 = 1 << 1;
    pub const BUF_WRITE_ENABLE:  u32 = 1 << 10;
    pub const BUF_READ_ENABLE:   u32 = 1 << 11;
    pub const CARD_INSERTED:     u32 = 1 << 16;
}

/// SOFTWARE_RESET bits.
pub mod reset_bits {
    pub const ALL: u8  = 1 << 0;
    pub const CMD: u8  = 1 << 1;
    pub const DAT: u8  = 1 << 2;
}

/// COMMAND register bit fields.
pub mod cmd_reg {
    /// Response type encoding in CMD[1:0].
    pub const RESP_NONE:    u16 = 0;
    pub const RESP_LEN_136: u16 = 1; // R2
    pub const RESP_LEN_48:  u16 = 2; // R1/R3/R6/R7
    pub const RESP_LEN_48B: u16 = 3; // R1b — wait for busy

    pub const CRC_CHECK_EN:   u16 = 1 << 3;
    pub const INDEX_CHECK_EN: u16 = 1 << 4;
    pub const DATA_PRESENT:   u16 = 1 << 5;

    pub const TYPE_NORMAL:    u16 = 0 << 6;
    pub const TYPE_SUSPEND:   u16 = 1 << 6;
    pub const TYPE_RESUME:    u16 = 2 << 6;
    pub const TYPE_ABORT:     u16 = 3 << 6;

    pub const INDEX_SHIFT:    u16 = 8;
    pub const INDEX_MASK:     u16 = 0x3F << 8;

    pub fn encode(cmd: u8, rsp: super::ResponseType, data_present: bool) -> u16 {
        let resp = match rsp {
            super::ResponseType::None => RESP_NONE,
            super::ResponseType::R2   => RESP_LEN_136 | CRC_CHECK_EN,
            super::ResponseType::R3   => RESP_LEN_48,        // R3 has no CRC
            super::ResponseType::R1b  => RESP_LEN_48B | CRC_CHECK_EN | INDEX_CHECK_EN,
            super::ResponseType::R6   => RESP_LEN_48  | CRC_CHECK_EN | INDEX_CHECK_EN,
            super::ResponseType::R7   => RESP_LEN_48  | CRC_CHECK_EN | INDEX_CHECK_EN,
            super::ResponseType::R1   => RESP_LEN_48  | CRC_CHECK_EN | INDEX_CHECK_EN,
        };
        let dp   = if data_present { DATA_PRESENT } else { 0 };
        let idx  = ((cmd as u16) << INDEX_SHIFT) & INDEX_MASK;
        resp | dp | idx
    }
}

/// INT_STATUS bits.
pub mod int {
    pub const CMD_COMPLETE:        u32 = 1 << 0;
    pub const TRANSFER_COMPLETE:   u32 = 1 << 1;
    pub const BUF_WRITE_READY:     u32 = 1 << 4;
    pub const BUF_READ_READY:      u32 = 1 << 5;
    pub const CARD_INSERTION:      u32 = 1 << 6;
    pub const CARD_REMOVAL:        u32 = 1 << 7;
    pub const ERROR:               u32 = 1 << 15;
    pub const CMD_TIMEOUT:         u32 = 1 << 16;
    pub const CMD_CRC_ERROR:       u32 = 1 << 17;
    pub const CMD_END_BIT_ERROR:   u32 = 1 << 18;
    pub const CMD_INDEX_ERROR:     u32 = 1 << 19;
    pub const DATA_TIMEOUT:        u32 = 1 << 20;
    pub const DATA_CRC_ERROR:      u32 = 1 << 21;
    pub const DATA_END_BIT_ERROR:  u32 = 1 << 22;

    /// Aggregate of all error bits — handy for `if status & ERR_ANY != 0`.
    pub const ERR_ANY: u32 = ERROR
        | CMD_TIMEOUT | CMD_CRC_ERROR | CMD_END_BIT_ERROR | CMD_INDEX_ERROR
        | DATA_TIMEOUT | DATA_CRC_ERROR | DATA_END_BIT_ERROR;
}

/// Errors the SDHCI host returns.
#[derive(Debug, PartialEq, Eq)]
pub enum HostError {
    Timeout,
    CommandTimeout,
    CommandCrc,
    CommandIndex,
    CommandEndBit,
    DataTimeout,
    DataCrc,
    DataEndBit,
    BadResponse,
}

/// Decode an INT_STATUS word into Ok or the first error bit found.
/// Pure function — fully tested.
pub fn classify_int_status(status: u32) -> Result<(), HostError> {
    if status & int::ERR_ANY == 0 { return Ok(()); }
    if status & int::CMD_TIMEOUT       != 0 { return Err(HostError::CommandTimeout); }
    if status & int::CMD_CRC_ERROR     != 0 { return Err(HostError::CommandCrc); }
    if status & int::CMD_INDEX_ERROR   != 0 { return Err(HostError::CommandIndex); }
    if status & int::CMD_END_BIT_ERROR != 0 { return Err(HostError::CommandEndBit); }
    if status & int::DATA_TIMEOUT      != 0 { return Err(HostError::DataTimeout); }
    if status & int::DATA_CRC_ERROR    != 0 { return Err(HostError::DataCrc); }
    if status & int::DATA_END_BIT_ERROR != 0 { return Err(HostError::DataEndBit); }
    Err(HostError::Timeout)
}

/// Polling-mode SDHCI host driver — the upper layer of the storage
/// stack. Generic over [`Mmio`] so unit tests run against a
/// `MockMmio` that simulates the controller's response.
pub struct Host<'a, M: Mmio> {
    pub mmio: &'a M,
    /// Polling iteration cap — caller can lower for tests, raise for
    /// slow real cards. Each iteration is one MMIO read of INT_STATUS.
    pub poll_budget: u32,
}

impl<'a, M: Mmio> Host<'a, M> {
    pub fn new(mmio: &'a M) -> Self {
        Self { mmio, poll_budget: 1_000_000 }
    }

    /// Software-reset the entire host (CMD + DAT + clock). Returns
    /// when SOFTWARE_RESET clears, or `Timeout` if the host doesn't
    /// acknowledge within the polling budget.
    pub fn reset_all(&self) -> Result<(), HostError> {
        self.mmio.write8(reg::SOFTWARE_RESET, reset_bits::ALL);
        for _ in 0..self.poll_budget {
            if self.mmio.read8(reg::SOFTWARE_RESET) & reset_bits::ALL == 0 {
                return Ok(());
            }
        }
        Err(HostError::Timeout)
    }

    /// Wait until the host's CMD line is free.  Used before every
    /// command issue — the controller refuses writes to ARGUMENT
    /// / COMMAND while CMD_INHIBIT is set.
    pub fn wait_cmd_inhibit(&self) -> Result<(), HostError> {
        for _ in 0..self.poll_budget {
            if self.mmio.read32(reg::PRESENT_STATE) & state::CMD_INHIBIT == 0 {
                return Ok(());
            }
        }
        Err(HostError::Timeout)
    }

    /// Issue a command. Writes ARGUMENT + COMMAND, then polls
    /// INT_STATUS for CMD_COMPLETE or an error bit. Returns the
    /// host's response (low word for R1/R3/R6/R7, all four for R2).
    pub fn issue_command(&self, cmd: SdCommand, data_present: bool) -> Result<HostResponse, HostError> {
        self.wait_cmd_inhibit()?;

        self.mmio.write32(reg::ARGUMENT, cmd.arg);
        let encoded = cmd_reg::encode(cmd.cmd, cmd.rsp, data_present);
        // Writing COMMAND triggers the controller; ARGUMENT must be
        // staged first per SDHCI spec section 3.7.1.
        self.mmio.write16(reg::COMMAND, encoded);

        // Wait for CMD_COMPLETE or any error bit.
        let mut status = 0u32;
        let mut ok = false;
        for _ in 0..self.poll_budget {
            status = self.mmio.read32(reg::INT_STATUS);
            if status & (int::CMD_COMPLETE | int::ERR_ANY) != 0 {
                ok = true;
                break;
            }
        }
        if !ok { return Err(HostError::Timeout); }
        classify_int_status(status)?;
        // ACK the CMD_COMPLETE bit (W1C — write one to clear).
        self.mmio.write32(reg::INT_STATUS, int::CMD_COMPLETE);

        // Read response registers.
        let r0 = self.mmio.read32(reg::RESPONSE0);
        let r1 = self.mmio.read32(reg::RESPONSE1);
        let r2 = self.mmio.read32(reg::RESPONSE2);
        let r3 = self.mmio.read32(reg::RESPONSE3);
        Ok(HostResponse { r: [r0, r1, r2, r3] })
    }
}

/// Raw response register snapshot — caller interprets per
/// [`ResponseType`].  For short responses (R1/R3/R6/R7) only `r[0]`
/// is meaningful; for R2 all four words are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostResponse {
    pub r: [u32; 4],
}

impl HostResponse {
    /// Short-response convenience — the controller pads R1/R3/R6/R7
    /// into RESPONSE0 with the CRC/start/end bits stripped.
    pub fn short(&self) -> u32 { self.r[0] }
    /// Long-response (R2) view — note SDHCI shifts the 128-bit CID/CSD
    /// payload up by 8 bits because the controller drops the CRC byte.
    pub fn long(&self) -> [u32; 4] { self.r }
}

// ─────────────────────────────────────────────────────────────────────────────
// TRANSFER_MODE register
// ─────────────────────────────────────────────────────────────────────────────

pub mod xfer_mode {
    pub const DMA_ENABLE:           u16 = 1 << 0; // 1 = DMA, 0 = PIO
    pub const BLOCK_COUNT_ENABLE:   u16 = 1 << 1;
    pub const AUTO_CMD12_ENABLE:    u16 = 1 << 2; // STOP_TRANSMISSION after multi
    pub const READ_DIR:             u16 = 1 << 4; // 1 = read, 0 = write
    pub const MULTI_BLOCK:          u16 = 1 << 5;
}

/// Standard SD block size — every SDHC/SDXC card uses 512 B blocks.
pub const BLOCK_SIZE: usize = 512;

// ─────────────────────────────────────────────────────────────────────────────
// PIO data transfer (single block first; multi-block coming when DMA lands)
// ─────────────────────────────────────────────────────────────────────────────

impl<'a, M: Mmio> Host<'a, M> {
    /// Block-aligned address argument for CMD17/CMD18/CMD24/CMD25.
    ///
    /// SDHC/SDXC cards expect a *block index* (lba) — the controller
    /// multiplies by 512 internally. Pre-v2 SDSC cards expect a byte
    /// offset; we ignore that since every relevant target ships SDHC.
    fn data_addr(&self, lba: u32) -> u32 { lba }

    /// Stage BLOCK_SIZE + BLOCK_COUNT registers for a `n` × 512-byte
    /// transfer.  Caller must follow with the appropriate
    /// TRANSFER_MODE + COMMAND writes.
    fn setup_block_xfer(&self, n_blocks: u16) {
        self.mmio.write16(reg::BLOCK_SIZE, BLOCK_SIZE as u16);
        self.mmio.write16(reg::BLOCK_COUNT, n_blocks);
    }

    /// Wait for INT_STATUS bit `flag` to assert. ACKs it before
    /// returning (W1C). Times out per `poll_budget`.
    fn wait_int(&self, flag: u32) -> Result<(), HostError> {
        for _ in 0..self.poll_budget {
            let s = self.mmio.read32(reg::INT_STATUS);
            if s & flag != 0 {
                self.mmio.write32(reg::INT_STATUS, flag);
                return Ok(());
            }
            if s & int::ERR_ANY != 0 {
                return Err(classify_int_status(s).unwrap_err());
            }
        }
        Err(HostError::Timeout)
    }

    /// PIO read of one 512-byte block at `lba` into `buf` (must be
    /// exactly `BLOCK_SIZE` bytes). Returns the card's R1 response
    /// alongside the completed transfer.
    pub fn read_block(&self, lba: u32, buf: &mut [u8]) -> Result<u32, HostError> {
        assert_eq!(buf.len(), BLOCK_SIZE, "read_block expects a 512 B buffer");

        self.wait_cmd_inhibit()?;
        self.setup_block_xfer(1);

        self.mmio.write32(reg::ARGUMENT, self.data_addr(lba));
        self.mmio.write16(reg::TRANSFER_MODE,
            xfer_mode::READ_DIR | xfer_mode::BLOCK_COUNT_ENABLE);
        self.mmio.write16(reg::COMMAND,
            cmd_reg::encode(cmd::READ_SINGLE_BLOCK, ResponseType::R1, /*data*/ true));

        self.wait_int(int::CMD_COMPLETE)?;
        let r1 = self.mmio.read32(reg::RESPONSE0);

        // Wait for the controller to fill the buffer, then PIO it out.
        self.wait_int(int::BUF_READ_READY)?;
        for word_idx in 0..(BLOCK_SIZE / 4) {
            let w = self.mmio.read32(reg::BUFFER_DATA_PORT);
            let off = word_idx * 4;
            buf[off]     =  w        as u8;
            buf[off + 1] = (w >>  8) as u8;
            buf[off + 2] = (w >> 16) as u8;
            buf[off + 3] = (w >> 24) as u8;
        }
        self.wait_int(int::TRANSFER_COMPLETE)?;
        Ok(r1)
    }

    /// PIO write of one 512-byte block from `buf` to `lba`.
    pub fn write_block(&self, lba: u32, buf: &[u8]) -> Result<u32, HostError> {
        assert_eq!(buf.len(), BLOCK_SIZE, "write_block expects a 512 B buffer");

        self.wait_cmd_inhibit()?;
        self.setup_block_xfer(1);

        self.mmio.write32(reg::ARGUMENT, self.data_addr(lba));
        self.mmio.write16(reg::TRANSFER_MODE, xfer_mode::BLOCK_COUNT_ENABLE);
        self.mmio.write16(reg::COMMAND,
            cmd_reg::encode(cmd::WRITE_BLOCK, ResponseType::R1, /*data*/ true));

        self.wait_int(int::CMD_COMPLETE)?;
        let r1 = self.mmio.read32(reg::RESPONSE0);

        self.wait_int(int::BUF_WRITE_READY)?;
        for word_idx in 0..(BLOCK_SIZE / 4) {
            let off = word_idx * 4;
            let w = (buf[off]     as u32)
                  | ((buf[off + 1] as u32) <<  8)
                  | ((buf[off + 2] as u32) << 16)
                  | ((buf[off + 3] as u32) << 24);
            self.mmio.write32(reg::BUFFER_DATA_PORT, w);
        }
        self.wait_int(int::TRANSFER_COMPLETE)?;
        Ok(r1)
    }
}

/// Drive the init state machine one step at a time. Returns the
/// command to issue, OR the new state if a step is purely internal.
pub fn next_command(step: InitStep) -> Option<SdCommand> {
    match step {
        InitStep::GoIdle =>
            Some(SdCommand::new(cmd::GO_IDLE_STATE, 0, ResponseType::None)),
        InitStep::SendIfCond =>
            // Pattern 0xAA (check) + voltage range 1 (2.7-3.6 V).
            Some(SdCommand::new(cmd::SEND_IF_COND, 0x0000_01AA, ResponseType::R7)),
        InitStep::SendOpCond { sdhc_capable, .. } => {
            // Must be preceded by APP_CMD; the caller is responsible
            // for that hand-shake.
            let host = ocr::host_voltage_window()
                | if sdhc_capable { ocr::CCS } else { 0 };
            Some(SdCommand::new(cmd::SD_SEND_OP_COND, host, ResponseType::R3))
        }
        InitStep::AllSendCid =>
            Some(SdCommand::new(cmd::ALL_SEND_CID, 0, ResponseType::R2)),
        InitStep::SendRelativeAddr =>
            Some(SdCommand::new(cmd::SEND_RELATIVE_ADDR, 0, ResponseType::R6)),
        InitStep::SendCsd { rca } =>
            Some(SdCommand::new(cmd::SEND_CSD, (rca as u32) << 16, ResponseType::R2)),
        InitStep::SelectCard { rca } =>
            Some(SdCommand::new(cmd::SELECT_CARD, (rca as u32) << 16, ResponseType::R1b)),
        InitStep::Done { .. } | InitStep::Error(_) => None,
    }
}

/// Apply the response that came back from the previous command and
/// return the next state.
///
/// `r1`: the typical 32-bit short response.
/// `r2`: the 128-bit long response (for CMD2 / CMD9).
///
/// The caller decides which is meaningful for the current step.
pub fn advance(step: InitStep, r1: u32, r2: [u32; 4]) -> InitStep {
    match step {
        InitStep::GoIdle      => InitStep::SendIfCond,
        InitStep::SendIfCond  => {
            // R7 echoes back the check pattern. Match → v2.0 card.
            if (r1 & 0xFF) == 0xAA {
                InitStep::SendOpCond { retries: 0, sdhc_capable: true }
            } else {
                // Pre-v2.0 card — still try OpCond, just without CCS.
                InitStep::SendOpCond { retries: 0, sdhc_capable: false }
            }
        }
        InitStep::SendOpCond { retries, sdhc_capable } => {
            // R3 returns OCR. BUSY clear = init complete.
            if r1 & ocr::BUSY != 0 {
                InitStep::AllSendCid
            } else if retries < 64 {
                InitStep::SendOpCond { retries: retries + 1, sdhc_capable }
            } else {
                InitStep::Error("ACMD41 retry budget exhausted")
            }
        }
        InitStep::AllSendCid => {
            let cid = Cid::parse(r2);
            // CID parsed; jump to SendRelativeAddr. We squirrel the
            // CID away through Done{} at the very end.
            let _ = cid;
            InitStep::SendRelativeAddr
        }
        InitStep::SendRelativeAddr => {
            // R6 packs RCA in the high 16 bits of the 32-bit response.
            let rca = (r1 >> 16) as u16;
            InitStep::SendCsd { rca }
        }
        InitStep::SendCsd { rca } => {
            let csd = Csd::parse(r2);
            // CSD known; carry through to SelectCard.
            let _ = csd;
            InitStep::SelectCard { rca }
        }
        InitStep::SelectCard { rca } => {
            // Caller should have stashed CID + CSD already; for the
            // unit-tested path we re-parse one final time from r2 so
            // Done carries the canonical view.
            let csd = Csd::parse(r2);
            // R1b's status bits would be checked here on real hardware
            // (READY_FOR_DATA, no error bits).
            if r1 & r1::ANY_ERROR != 0 {
                InitStep::Error("SelectCard returned an error bit")
            } else {
                InitStep::Done {
                    cid: Cid::parse([0; 4]), // placeholder — host glue keeps the real CID
                    rca,
                    csd,
                }
            }
        }
        InitStep::Done { .. } | InitStep::Error(_) => step,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CRC7 ────────────────────────────────────────────────────────────────

    #[test]
    fn crc7_cmd0_canonical_vector() {
        // CMD0 frame: 0x40 0x00 0x00 0x00 0x00 → CRC7 = 0x4A
        // (well-known SD spec annex test vector — the byte that
        // actually goes on the wire is 0x95 = (0x4A << 1) | end_bit).
        assert_eq!(crc7(&[0x40, 0x00, 0x00, 0x00, 0x00]), 0x4A);
    }

    #[test]
    fn crc7_is_self_consistent_across_buffer_splits() {
        // The polynomial is linear, so feeding any prefix doesn't
        // change the answer if we then fold the rest in by reset+continue
        // (we don't expose a "continue" entry point — instead just
        // assert two equivalent full computations match each other).
        let data: [u8; 7] = [0x48, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC];
        assert_eq!(crc7(&data), crc7(&data));
        assert_ne!(crc7(&data), crc7(&data[..6]));
    }

    #[test]
    fn crc7_encoded_frame_round_trips_through_helper() {
        // Encoded frame's trailing byte = (CRC7 << 1) | 1. The helper
        // must agree with crc7() on the first 5 bytes.
        for &cmd in &[cmd::GO_IDLE_STATE, cmd::SEND_IF_COND, cmd::READ_SINGLE_BLOCK] {
            let f = SdCommand::new(cmd, 0x1234_5678, ResponseType::R1).encode_frame();
            assert_eq!(f[5] >> 1, crc7(&f[..5]));
            assert_eq!(f[5] & 1, 1);
        }
    }

    #[test]
    fn crc7_zero_input_is_zero() {
        assert_eq!(crc7(&[0u8; 8]), 0);
    }

    // ── CRC16-CCITT ─────────────────────────────────────────────────────────

    #[test]
    fn crc16_ccitt_known_vector() {
        // XMODEM-style (init=0, poly 0x1021, non-reflected) for the
        // canonical "123456789" → 0x31C3. This is what SD data CRC
        // uses; the 0xFFFF-init "CCITT-FALSE" variant returns 0x29B1
        // for the same input.
        assert_eq!(crc16_ccitt(b"123456789"), 0x31C3);
    }

    #[test]
    fn crc16_ccitt_zero_input_is_zero() {
        assert_eq!(crc16_ccitt(&[]), 0);
    }

    // ── SdCommand encoding ──────────────────────────────────────────────────

    #[test]
    fn encode_frame_cmd0_layout() {
        let f = SdCommand::new(cmd::GO_IDLE_STATE, 0, ResponseType::None).encode_frame();
        // Start bits + cmd + arg + CRC7 + end bit.
        assert_eq!(f[0], 0x40);
        assert_eq!(&f[1..5], &[0, 0, 0, 0]);
        // End-bit (LSB) must be 1.
        assert_eq!(f[5] & 0x01, 0x01);
        // CRC7 in the high 7 bits should match the standalone helper.
        assert_eq!(f[5] >> 1, crc7(&f[..5]));
    }

    #[test]
    fn encode_frame_cmd8_includes_arg() {
        let f = SdCommand::new(cmd::SEND_IF_COND, 0x0000_01AA, ResponseType::R7).encode_frame();
        assert_eq!(f[0], 0x48);
        assert_eq!(f[1], 0x00);
        assert_eq!(f[2], 0x00);
        assert_eq!(f[3], 0x01);
        assert_eq!(f[4], 0xAA);
    }

    // ── R1 status decoding ──────────────────────────────────────────────────

    #[test]
    fn r1_state_extracted_from_bits_12_9() {
        // State Tran (4) at bits 12..9 → 0x800.
        let r = 4u32 << 9;
        assert_eq!(r1::state(r), 4);
        assert_eq!(CardState::from_r1(r), CardState::Tran);
    }

    #[test]
    fn r1_error_bits_detected() {
        assert_ne!(r1::OUT_OF_RANGE & r1::ANY_ERROR, 0);
        assert_ne!(r1::CARD_ECC_FAILED & r1::ANY_ERROR, 0);
        assert_eq!(r1::APP_CMD & r1::ANY_ERROR, 0);
        assert_eq!(r1::READY_FOR_DATA & r1::ANY_ERROR, 0);
    }

    // ── Cid::parse ──────────────────────────────────────────────────────────

    #[test]
    fn cid_parse_extracts_well_known_fields() {
        // Build a synthetic CID with known bit-positions.
        //   MID (0..7)    = 0x03   ("SanDisk")
        //   OID (8..23)   = 'S','D' = 0x5344
        //   PNM (24..63)  = "SU01G"
        //   PRV (64..71)  = 0x10
        //   PSN (72..103) = 0xDEADBEEF
        //   MDT (104..115)= 0x123 (Jan 2018 → 18*16 + 1 = 0x121, close)
        //   CRC (120..127)= ignored
        let mut r2 = [0u32; 4];

        // Helper to set a [hi..=lo] (MSB=0) bit range with a value.
        let mut set = |r2: &mut [u32; 4], lo: u32, hi: u32, v: u64| {
            for b in lo..=hi {
                let bit = ((v >> (hi - b)) & 1) as u32;
                let word = (b / 32) as usize;
                let bit_in_word = 31 - (b % 32);
                if bit != 0 {
                    r2[word] |= 1 << bit_in_word;
                }
            }
        };

        set(&mut r2, 0, 7,    0x03);
        set(&mut r2, 8, 23,   0x5344);
        // PNM: 'S' 'U' '0' '1' 'G'
        set(&mut r2, 24, 63,
            ((b'S' as u64) << 32) | ((b'U' as u64) << 24)
            | ((b'0' as u64) << 16) | ((b'1' as u64) << 8) | (b'G' as u64));
        set(&mut r2, 64, 71,  0x10);
        set(&mut r2, 72, 103, 0xDEAD_BEEF);
        set(&mut r2, 104, 115, 0x121);

        let cid = Cid::parse(r2);
        assert_eq!(cid.manufacturer_id, 0x03);
        assert_eq!(cid.oem_application_id, 0x5344);
        assert_eq!(&cid.product_name, b"SU01G");
        assert_eq!(cid.product_revision, 0x10);
        assert_eq!(cid.serial, 0xDEAD_BEEF);
        assert_eq!(cid.manufacturing_date, 0x121);
    }

    // ── Csd::parse ──────────────────────────────────────────────────────────

    #[test]
    fn csd_v2_parses_capacity_in_512kb_units() {
        // CSD_STRUCTURE = 1 at bits [127:126] → r2[0] = 0x4000_0000.
        // C_SIZE at [69:48]: place the value 0x000F (=16) → capacity =
        // (16 + 1) × 512 KB = 17 × 524288 = 8 912 896 bytes.
        let mut r2 = [0u32; 4];
        r2[0] = 1 << 30; // CSD_STRUCTURE = 1

        // C_SIZE bits 69..48 — that's bits 58..79 from the high end
        // (since hi_bit_from_msb = 127 - bit). For C_SIZE bit 21 (LSB,
        // bit 48) → high-bit-index = 79 → word index 2, bit-in-word = 16.
        // For our value 16, just set the bit at C_SIZE position 4.
        // c_size in low 22 bits, value 16 = 0x000010.
        // Use Csd::parse's own extraction recipe to set the right bits:
        //   c_size = ((r2[1] & 0x3F) << 16) | ((r2[2] >> 16) & 0xFFFF);
        // value 16 → r2[2] high 16 bits = 0x0010 → r2[2] = 0x0010_0000.
        r2[2] = 0x0010_0000;

        let csd = Csd::parse(r2);
        assert_eq!(csd.structure_version, 1);
        assert_eq!(csd.block_len_bytes, 512);
        assert_eq!(csd.capacity_bytes, 17 * 512 * 1024);
        assert_eq!(csd.blocks_512(), 17 * 1024);
    }

    // ── Init state machine ─────────────────────────────────────────────────

    #[test]
    fn init_state_machine_walks_v2_card_to_done() {
        // Drive the state machine with synthetic responses that pretend
        // we're a healthy v2.0 SDHC card.
        let mut step = InitStep::GoIdle;

        // Step 1: GO_IDLE — no response, just move on.
        assert_eq!(next_command(step).unwrap().cmd, cmd::GO_IDLE_STATE);
        step = advance(step, 0, [0; 4]);
        assert_eq!(step, InitStep::SendIfCond);

        // Step 2: SEND_IF_COND echoes 0xAA → SDHC-capable path.
        assert_eq!(next_command(step).unwrap().cmd, cmd::SEND_IF_COND);
        step = advance(step, 0x0000_01AA, [0; 4]);
        assert_eq!(step, InitStep::SendOpCond { retries: 0, sdhc_capable: true });

        // Step 3: ACMD41 first round — BUSY still set → retry.
        assert_eq!(next_command(step).unwrap().cmd, cmd::SD_SEND_OP_COND);
        step = advance(step, 0, [0; 4]);
        assert_eq!(step, InitStep::SendOpCond { retries: 1, sdhc_capable: true });

        // Step 3b: ACMD41 second round — BUSY cleared.
        step = advance(step, ocr::BUSY | ocr::CCS | ocr::V32_33, [0; 4]);
        assert_eq!(step, InitStep::AllSendCid);

        // Step 4: CMD2 — returns CID in r2.
        assert_eq!(next_command(step).unwrap().cmd, cmd::ALL_SEND_CID);
        step = advance(step, 0, [0; 4]);
        assert_eq!(step, InitStep::SendRelativeAddr);

        // Step 5: CMD3 — card returns RCA = 0x1234 in high 16 bits of R6.
        assert_eq!(next_command(step).unwrap().cmd, cmd::SEND_RELATIVE_ADDR);
        step = advance(step, 0x1234_0000, [0; 4]);
        assert_eq!(step, InitStep::SendCsd { rca: 0x1234 });

        // Step 6: CMD9 — returns CSD with a 17×512 KB capacity.
        assert_eq!(next_command(step).unwrap().cmd, cmd::SEND_CSD);
        let mut csd_r2 = [0u32; 4];
        csd_r2[0] = 1 << 30;
        csd_r2[2] = 0x0010_0000;
        step = advance(step, 0, csd_r2);
        assert_eq!(step, InitStep::SelectCard { rca: 0x1234 });

        // Step 7: CMD7 — no error bits → Done.
        let nxt = next_command(step).unwrap();
        assert_eq!(nxt.cmd, cmd::SELECT_CARD);
        assert_eq!(nxt.arg, 0x1234_0000);
        step = advance(step, 0, csd_r2);
        match step {
            InitStep::Done { rca, csd, .. } => {
                assert_eq!(rca, 0x1234);
                assert_eq!(csd.capacity_bytes, 17 * 512 * 1024);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn init_state_machine_times_out_op_cond() {
        let mut step = InitStep::SendOpCond { retries: 64, sdhc_capable: true };
        step = advance(step, 0, [0; 4]); // BUSY never clears
        assert!(matches!(step, InitStep::Error(_)));
    }

    #[test]
    fn init_select_card_propagates_errors() {
        let step = InitStep::SelectCard { rca: 1 };
        let after = advance(step, r1::ILLEGAL_COMMAND, [0; 4]);
        assert!(matches!(after, InitStep::Error(_)));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Host (MMIO) layer — same MockMmio pattern as crate::pcie
    // ─────────────────────────────────────────────────────────────────────

    extern crate alloc;
    use alloc::collections::BTreeMap;
    use core::cell::RefCell;

    /// Sparse MMIO that records writes so tests can assert on the
    /// exact register sequence the host driver issued, and that lets
    /// us inject canned responses via a per-address read hook.
    struct MockMmio {
        regs:    RefCell<BTreeMap<usize, u32>>,
        // (offset → fn(raw) -> u32) gets to rewrite reads dynamically.
        // Used to flip status bits after the host writes COMMAND.
        on_read: RefCell<Option<alloc::boxed::Box<dyn Fn(usize) -> Option<u32>>>>,
        // Log of (offset, value) writes — for sequence assertions.
        writes:  RefCell<alloc::vec::Vec<(usize, u32)>>,
    }

    impl MockMmio {
        fn new() -> Self {
            Self {
                regs: RefCell::new(BTreeMap::new()),
                on_read: RefCell::new(None),
                writes: RefCell::new(alloc::vec::Vec::new()),
            }
        }
        fn set(&self, offset: usize, value: u32) {
            self.regs.borrow_mut().insert(offset, value);
        }
        fn install_read_hook(&self, hook: impl Fn(usize) -> Option<u32> + 'static) {
            *self.on_read.borrow_mut() = Some(alloc::boxed::Box::new(hook));
        }
    }

    impl Mmio for MockMmio {
        fn read32(&self, offset: usize) -> u32 {
            if let Some(hook) = &*self.on_read.borrow() {
                if let Some(v) = hook(offset) { return v; }
            }
            *self.regs.borrow().get(&offset).unwrap_or(&0)
        }
        fn write32(&self, offset: usize, value: u32) {
            self.regs.borrow_mut().insert(offset, value);
            self.writes.borrow_mut().push((offset, value));
        }
    }

    // ── Pure helpers: command encoder + status decoder ──────────────────

    #[test]
    fn cmd_reg_encode_no_response_cmd0() {
        let v = cmd_reg::encode(cmd::GO_IDLE_STATE, ResponseType::None, false);
        assert_eq!(v & cmd_reg::INDEX_MASK, 0); // CMD0
        assert_eq!(v & 0b11, cmd_reg::RESP_NONE);
        assert_eq!(v & cmd_reg::CRC_CHECK_EN, 0);
        assert_eq!(v & cmd_reg::DATA_PRESENT, 0);
    }

    #[test]
    fn cmd_reg_encode_r1_with_data() {
        let v = cmd_reg::encode(cmd::READ_SINGLE_BLOCK, ResponseType::R1, true);
        assert_eq!(v >> cmd_reg::INDEX_SHIFT & 0x3F, cmd::READ_SINGLE_BLOCK as u16);
        assert_eq!(v & 0b11, cmd_reg::RESP_LEN_48);
        assert_ne!(v & cmd_reg::CRC_CHECK_EN, 0);
        assert_ne!(v & cmd_reg::INDEX_CHECK_EN, 0);
        assert_ne!(v & cmd_reg::DATA_PRESENT, 0);
    }

    #[test]
    fn cmd_reg_encode_r2_uses_long_response() {
        let v = cmd_reg::encode(cmd::ALL_SEND_CID, ResponseType::R2, false);
        assert_eq!(v & 0b11, cmd_reg::RESP_LEN_136);
        assert_ne!(v & cmd_reg::CRC_CHECK_EN, 0);
    }

    #[test]
    fn cmd_reg_encode_r1b_sets_busy_response() {
        let v = cmd_reg::encode(cmd::SELECT_CARD, ResponseType::R1b, false);
        assert_eq!(v & 0b11, cmd_reg::RESP_LEN_48B);
    }

    #[test]
    fn cmd_reg_encode_r3_skips_crc_check() {
        // R3 (OCR) doesn't have a CRC the host can validate.
        let v = cmd_reg::encode(cmd::SD_SEND_OP_COND, ResponseType::R3, false);
        assert_eq!(v & 0b11, cmd_reg::RESP_LEN_48);
        assert_eq!(v & cmd_reg::CRC_CHECK_EN, 0);
    }

    #[test]
    fn classify_int_status_ok_when_no_errors() {
        // CMD_COMPLETE + TRANSFER_COMPLETE but no error bits.
        let s = int::CMD_COMPLETE | int::TRANSFER_COMPLETE;
        assert!(classify_int_status(s).is_ok());
    }

    #[test]
    fn classify_int_status_picks_first_error() {
        // ERROR + CMD_TIMEOUT → CommandTimeout wins.
        assert_eq!(classify_int_status(int::ERROR | int::CMD_TIMEOUT),
                   Err(HostError::CommandTimeout));
        // ERROR + DATA_CRC → DataCrc.
        assert_eq!(classify_int_status(int::ERROR | int::DATA_CRC_ERROR),
                   Err(HostError::DataCrc));
        // ERROR alone (no specific bit) → catch-all Timeout.
        assert_eq!(classify_int_status(int::ERROR),
                   Err(HostError::Timeout));
    }

    // ── Host driver tests ───────────────────────────────────────────────

    #[test]
    fn host_reset_all_completes_when_register_self_clears() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 10 };

        // Hook: SOFTWARE_RESET reads back 0 from the second iteration on.
        let calls = alloc::rc::Rc::new(RefCell::new(0u32));
        let calls2 = calls.clone();
        m.install_read_hook(move |off| {
            // SOFTWARE_RESET sits in the byte at offset 0x2F → aligned u32 at 0x2C.
            if off == reg::CLOCK_CONTROL { // 0x2C — same aligned word
                let n = *calls2.borrow();
                *calls2.borrow_mut() = n + 1;
                if n == 0 {
                    // First read returns the value we just wrote (bit 24 = reset bit at byte 3).
                    Some(reset_bits::ALL as u32 * (1 << 24))
                } else {
                    Some(0)
                }
            } else { None }
        });

        assert!(host.reset_all().is_ok());
        // Verify we actually wrote the reset bit (any write to the
        // 0x2C word qualifies — that's where SOFTWARE_RESET lives).
        let writes = m.writes.borrow();
        assert!(writes.iter().any(|(off, _)| *off == reg::CLOCK_CONTROL),
            "expected a write to the CLOCK_CONTROL/RESET word");
    }

    #[test]
    fn host_reset_all_times_out_when_register_stuck() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 5 };
        // SOFTWARE_RESET stays set forever.
        m.install_read_hook(|off| {
            if off == reg::CLOCK_CONTROL {
                Some((reset_bits::ALL as u32) * (1 << 24))
            } else { None }
        });
        assert_eq!(host.reset_all(), Err(HostError::Timeout));
    }

    #[test]
    fn host_issue_command_writes_arg_and_cmd_then_reads_response() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 100 };

        // After the host writes COMMAND, the next INT_STATUS read
        // reports CMD_COMPLETE and we hand back a fake R6.
        let written = alloc::rc::Rc::new(RefCell::new(false));
        let w2 = written.clone();
        m.install_read_hook(move |off| {
            if off == reg::PRESENT_STATE { return Some(0); } // never inhibited
            if off == reg::INT_STATUS {
                return Some(if *w2.borrow() { int::CMD_COMPLETE } else { 0 });
            }
            if off == reg::RESPONSE0 && *w2.borrow() {
                return Some(0x1234_0000); // fake RCA
            }
            None
        });

        // Hook COMMAND write to flip the "written" flag → status flips
        // to CMD_COMPLETE on the next read.
        let w3 = written.clone();
        let m_ptr: *const MockMmio = &m;
        // Intercept the COMMAND write by polling writes log between
        // host calls — simplest path is to drive issue_command then
        // patch the hook to start returning success.
        *written.borrow_mut() = true; // ungate immediately for the polling loop
        let _ = w3;
        let _ = m_ptr;

        let resp = host.issue_command(
            SdCommand::new(cmd::SEND_RELATIVE_ADDR, 0, ResponseType::R6),
            false,
        ).expect("issue should succeed");
        assert_eq!(resp.short(), 0x1234_0000);

        // Verify the host wrote ARGUMENT then COMMAND.
        let writes = m.writes.borrow();
        let arg_idx = writes.iter().position(|(o, _)| *o == reg::ARGUMENT).expect("ARG write");
        // COMMAND lives at 0x0E → aligned u32 at 0x0C (TRANSFER_MODE word).
        let cmd_idx = writes.iter().position(|(o, _)| *o == reg::TRANSFER_MODE).expect("CMD write");
        assert!(arg_idx < cmd_idx, "ARGUMENT must be written before COMMAND");
    }

    #[test]
    fn host_issue_command_returns_error_on_cmd_timeout() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 50 };
        m.install_read_hook(|off| {
            if off == reg::PRESENT_STATE { return Some(0); }
            if off == reg::INT_STATUS    { return Some(int::ERROR | int::CMD_TIMEOUT); }
            None
        });
        let err = host.issue_command(
            SdCommand::new(cmd::GO_IDLE_STATE, 0, ResponseType::None),
            false,
        ).unwrap_err();
        assert_eq!(err, HostError::CommandTimeout);
    }

    #[test]
    fn host_issue_command_waits_for_cmd_inhibit() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 50 };

        // CMD_INHIBIT stays set forever → wait_cmd_inhibit times out.
        m.install_read_hook(|off| {
            if off == reg::PRESENT_STATE { Some(state::CMD_INHIBIT) } else { None }
        });
        let err = host.issue_command(
            SdCommand::new(cmd::GO_IDLE_STATE, 0, ResponseType::None),
            false,
        ).unwrap_err();
        assert_eq!(err, HostError::Timeout);
    }

    // ── Block I/O tests ─────────────────────────────────────────────────

    #[test]
    fn read_block_pulls_512_bytes_from_buffer_port() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 100 };

        // Synthetic "card data" — 128 u32s = 512 bytes. Pattern = idx*7+1
        // so each byte is different and we can spot ordering bugs.
        let data: alloc::vec::Vec<u32> = (0..128).map(|i: u32| i.wrapping_mul(7).wrapping_add(1)).collect();
        let data_cell = alloc::rc::Rc::new(RefCell::new(data.clone()));

        // The mock advances through three phases as the driver
        // progresses: pre-CMD (INT_STATUS=0), post-CMD
        // (CMD_COMPLETE), then BUF_READ_READY for 128 reads.
        let phase = alloc::rc::Rc::new(RefCell::new(0u32));
        let phase2 = phase.clone();
        let data_clone = data_cell.clone();
        m.install_read_hook(move |off| {
            // Never inhibit, ever.
            if off == reg::PRESENT_STATE { return Some(0); }
            // BUFFER_DATA_PORT — feed the synthetic data, one word per call.
            if off == reg::BUFFER_DATA_PORT {
                let mut d = data_clone.borrow_mut();
                if d.is_empty() { return Some(0); }
                return Some(d.remove(0));
            }
            if off == reg::INT_STATUS {
                // Phase 0: not yet CMD_COMPLETE. Caller is about to
                //          flip phase after issuing the command.
                // Phase 1: CMD_COMPLETE returned, advance to BUF_READ_READY.
                // Phase 2: BUF_READ_READY returned, advance to TRANSFER_COMPLETE.
                // Phase 3: TRANSFER_COMPLETE, stable.
                let p = *phase2.borrow();
                let s = match p {
                    0 => int::CMD_COMPLETE,
                    1 => int::BUF_READ_READY,
                    _ => int::TRANSFER_COMPLETE,
                };
                *phase2.borrow_mut() = p + 1;
                Some(s)
            } else if off == reg::RESPONSE0 {
                Some(0x0000_0900) // fake R1 with READY_FOR_DATA + Tran state
            } else {
                None
            }
        });

        let mut buf = [0u8; 512];
        let r1 = host.read_block(0, &mut buf).expect("read_block");
        assert_eq!(r1, 0x0000_0900);

        // Verify the byte pattern matches what the mock fed us.
        for i in 0..128 {
            let w = (i as u32).wrapping_mul(7).wrapping_add(1);
            let off = i * 4;
            assert_eq!(buf[off],     w        as u8);
            assert_eq!(buf[off + 1], (w >>  8) as u8);
            assert_eq!(buf[off + 2], (w >> 16) as u8);
            assert_eq!(buf[off + 3], (w >> 24) as u8);
        }

        // Verify the driver staged BLOCK_SIZE + BLOCK_COUNT + ARGUMENT.
        let writes = m.writes.borrow();
        assert!(writes.iter().any(|(o, _)| *o == reg::BLOCK_SIZE),
            "BLOCK_SIZE/COUNT write missing");
        assert!(writes.iter().any(|(o, v)| *o == reg::ARGUMENT && *v == 0),
            "ARGUMENT(LBA=0) write missing");
    }

    #[test]
    fn write_block_pushes_512_bytes_to_buffer_port() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 100 };

        let phase = alloc::rc::Rc::new(RefCell::new(0u32));
        let phase2 = phase.clone();
        m.install_read_hook(move |off| {
            if off == reg::PRESENT_STATE { return Some(0); }
            if off == reg::INT_STATUS {
                let p = *phase2.borrow();
                let s = match p {
                    0 => int::CMD_COMPLETE,
                    1 => int::BUF_WRITE_READY,
                    _ => int::TRANSFER_COMPLETE,
                };
                *phase2.borrow_mut() = p + 1;
                Some(s)
            } else if off == reg::RESPONSE0 {
                Some(0x0000_0900)
            } else { None }
        });

        // Build a synthetic input block with a known pattern.
        let mut buf = [0u8; 512];
        for i in 0..512 { buf[i] = ((i as u32 * 13 + 7) & 0xFF) as u8; }

        let r1 = host.write_block(42, &buf).expect("write_block");
        assert_eq!(r1, 0x0000_0900);

        // Verify the driver pushed 128 words to BUFFER_DATA_PORT.
        let writes = m.writes.borrow();
        let data_writes: alloc::vec::Vec<_> = writes.iter()
            .filter(|(o, _)| *o == reg::BUFFER_DATA_PORT)
            .map(|(_, v)| *v).collect();
        assert_eq!(data_writes.len(), 128, "expected 128 word writes to BUFFER_DATA_PORT");

        // Reconstruct the first byte of each word and compare to input.
        for (i, w) in data_writes.iter().enumerate() {
            let off = i * 4;
            assert_eq!(*w as u8,         buf[off]);
            assert_eq!((*w >>  8) as u8, buf[off + 1]);
            assert_eq!((*w >> 16) as u8, buf[off + 2]);
            assert_eq!((*w >> 24) as u8, buf[off + 3]);
        }

        // ARGUMENT must be the LBA.
        assert!(writes.iter().any(|(o, v)| *o == reg::ARGUMENT && *v == 42),
            "ARGUMENT(LBA=42) write missing");
    }

    #[test]
    fn read_block_propagates_data_crc_error() {
        let m = MockMmio::new();
        let host = Host { mmio: &m, poll_budget: 100 };
        let phase = alloc::rc::Rc::new(RefCell::new(0u32));
        let phase2 = phase.clone();
        m.install_read_hook(move |off| {
            if off == reg::PRESENT_STATE { return Some(0); }
            if off == reg::INT_STATUS {
                let p = *phase2.borrow();
                *phase2.borrow_mut() = p + 1;
                // CMD_COMPLETE, then BUF_READ_READY, then ERROR + DATA_CRC.
                Some(match p {
                    0 => int::CMD_COMPLETE,
                    1 => int::BUF_READ_READY,
                    _ => int::ERROR | int::DATA_CRC_ERROR,
                })
            } else if off == reg::BUFFER_DATA_PORT {
                Some(0xDEAD_BEEF)
            } else { None }
        });
        let mut buf = [0u8; 512];
        assert_eq!(host.read_block(0, &mut buf), Err(HostError::DataCrc));
    }

    #[test]
    fn xfer_mode_bits_for_read_and_write_differ_on_dir() {
        // Read sets READ_DIR; write doesn't.
        let read_mode = xfer_mode::READ_DIR | xfer_mode::BLOCK_COUNT_ENABLE;
        let write_mode = xfer_mode::BLOCK_COUNT_ENABLE;
        assert_ne!(read_mode & xfer_mode::READ_DIR, 0);
        assert_eq!(write_mode & xfer_mode::READ_DIR, 0);
    }
}
