// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Encrypted block layer for BeetOS — AES-256-GCM over `api/block`.
//!
//! Turns the tail of the raw disk into an authenticated encrypted
//! area. Layout (sectors are the block service's 512-byte unit, LBAs
//! relative to `base_lba = capacity - CRYPT_AREA_SECTORS`):
//!
//! ```text
//! base+0              header: magic, salt, passphrase check value
//! base+1              version map: sealed sector of per-slot u16
//!                     write counters (rollback detection)
//! base+2 .. base+N    slots:  one sealed sector each
//! ```
//!
//! Every sealed sector is self-contained:
//!
//! ```text
//! [ nonce (12) | tag (16) | ciphertext (484) ]
//! ```
//!
//! - **Nonce**: fresh kernel entropy (`SysCall::GetRandom`) on every
//!   write — no counters to persist. Reuse resistance is only as good
//!   as the kernel RNG: `GetRandom` routes through the platform seam,
//!   which is virtio-rng (real host entropy) on QEMU — the xtask
//!   invocations all pass `-device virtio-rng-device` and the smoke
//!   suite asserts the device came up — and FEAT_RNG (RNDR) on real
//!   silicon. Only when both are absent does it degrade to the
//!   counter-mixed xorshift PRNG in `arch/aarch64/rand.rs`, which is
//!   **not** a CSPRNG — don't trust the crypt area with real secrets
//!   on such a configuration.
//! - **AAD**: the slot's absolute LBA. A sealed sector copied to a
//!   different LBA fails authentication, so ciphertext can't be
//!   shuffled around the disk undetected.
//! - **Tag**: GCM authentication over ciphertext + AAD. Any bit flip
//!   in nonce, payload or position is detected at open time.
//!
//! ## Rollback detection (and its honest limits)
//!
//! Every slot's AAD also includes its current **write counter** from
//! the sealed version map at `base+1`. Restoring an older sealed image
//! of a slot (or of a file removed via `erase_slot`, which bumps the
//! counter before zeroing) fails authentication → `Corrupt`. To roll a
//! slot back silently, an attacker must also restore the version map —
//! which then mismatches (detectably) every *other* slot written since
//! that snapshot. What this deliberately does NOT detect: a rollback of
//! the **entire area** (header + vmap + all slots) to a fully
//! consistent older snapshot. That is unfixable with on-disk state
//! alone — it needs a monotonic counter outside the attacker's reach
//! (secure element / TPM), which is real-hardware (M1 port) territory.
//! A zeroed slot with a zero counter still reads as `Empty`
//! (indistinguishable from never-written) — deletion of a
//! never-rewritten file is the one silent erase that remains.
//!
//! ## Key derivation (dev-grade — read this before shipping)
//!
//! `key = SHA-256(salt || passphrase)`. A single hash is **not** a
//! password KDF: it has no work factor, so offline brute force runs at
//! hash speed. Fine for the QEMU dev loop this slice targets; the M1
//! port must replace it with Argon2id (or at minimum PBKDF2) and fold
//! in a device-unique hardware secret so the disk can't be attacked
//! off-device at all.
//!
//! The pure helpers ([`seal_sector`], [`open_sector`], [`derive_key`],
//! header build/parse) have no syscall dependencies and are unit
//! tested on the host; the [`CryptDisk`] convenience wrapper (real
//! IPC + entropy) only exists on beetos builds.

#![cfg_attr(not(test), no_std)]

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use sha2::{Digest, Sha256};

/// Size of the encrypted area at the end of the disk, in sectors.
/// Header + version map + 126 slots.
pub const CRYPT_AREA_SECTORS: u64 = 128;

/// Usable slots (sector 0 of the area is the header, sector 1 the
/// version map).
pub const CRYPT_SLOTS: u64 = CRYPT_AREA_SECTORS - 2;

pub const SECTOR_SIZE: usize = 512;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// Plaintext bytes that fit in one sealed sector.
pub const SLOT_PAYLOAD: usize = SECTOR_SIZE - NONCE_LEN - TAG_LEN;

/// Header magic — bumped if the layout ever changes.
/// v2: version-map sector at base+1, write counter in every slot AAD.
pub const MAGIC: &[u8; 8] = b"BEETCRY2";

const SALT_LEN: usize = 16;
/// Known plaintext sealed into the header so `open` can verify the
/// passphrase (authenticated — a wrong key fails the GCM tag).
const CHECK_PLAINTEXT: &[u8; 16] = b"beetos-crypt-ok!";
const CHECK_AAD: &[u8] = b"beetos-crypt-hdr";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CryptError {
    /// Block service IO failed.
    Io,
    /// Header magic missing — area not formatted.
    NotFormatted,
    /// Header present but the passphrase check failed.
    BadPassphrase,
    /// Slot index out of range or payload too large.
    BadArgument,
    /// Authentication failed on a data slot (tamper or corruption).
    Corrupt,
    /// Slot exists but was never written (all-zero sector).
    Empty,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers (host-testable, no syscalls)
// ─────────────────────────────────────────────────────────────────────────────

/// Derive the AES-256 key from salt + passphrase. See the module docs
/// for why this is dev-grade.
pub fn derive_key(salt: &[u8; SALT_LEN], passphrase: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(passphrase);
    h.finalize().into()
}

/// AAD binding a sealed sector to its absolute disk position AND its
/// write generation. `version` is the slot's current write counter
/// from the version map (0 for sectors that don't participate in
/// rollback tracking: the version map itself).
fn slot_aad(abs_lba: u64, version: u16) -> [u8; 10] {
    let mut aad = [0u8; 10];
    aad[..8].copy_from_slice(&abs_lba.to_le_bytes());
    aad[8..].copy_from_slice(&version.to_le_bytes());
    aad
}

/// Seal `payload` (≤ [`SLOT_PAYLOAD`] bytes, zero-padded) into a
/// 512-byte sector image bound to `abs_lba` and write-generation
/// `version`.
pub fn seal_sector(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    abs_lba: u64,
    version: u16,
    payload: &[u8],
) -> Result<[u8; SECTOR_SIZE], CryptError> {
    if payload.len() > SLOT_PAYLOAD {
        return Err(CryptError::BadArgument);
    }
    let mut buf = [0u8; SLOT_PAYLOAD];
    buf[..payload.len()].copy_from_slice(payload);

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let tag = cipher
        .encrypt_in_place_detached(
            Nonce::from_slice(nonce),
            &slot_aad(abs_lba, version),
            &mut buf,
        )
        .map_err(|_| CryptError::BadArgument)?;

    let mut sector = [0u8; SECTOR_SIZE];
    sector[..NONCE_LEN].copy_from_slice(nonce);
    sector[NONCE_LEN..NONCE_LEN + TAG_LEN].copy_from_slice(&tag);
    sector[NONCE_LEN + TAG_LEN..].copy_from_slice(&buf);
    Ok(sector)
}

/// Open a sealed 512-byte sector expected at write-generation
/// `version`. Returns the decrypted payload, or
/// [`CryptError::Empty`] for an all-zero (never written) sector, or
/// [`CryptError::Corrupt`] when authentication fails — including when
/// the sector is a stale-but-genuine older generation (rollback).
pub fn open_sector(
    key: &[u8; 32],
    abs_lba: u64,
    version: u16,
    sector: &[u8; SECTOR_SIZE],
) -> Result<[u8; SLOT_PAYLOAD], CryptError> {
    if sector.iter().all(|&b| b == 0) {
        return Err(CryptError::Empty);
    }
    let nonce: [u8; NONCE_LEN] = sector[..NONCE_LEN].try_into().unwrap();
    let tag: [u8; TAG_LEN] = sector[NONCE_LEN..NONCE_LEN + TAG_LEN].try_into().unwrap();
    let mut buf: [u8; SLOT_PAYLOAD] = sector[NONCE_LEN + TAG_LEN..].try_into().unwrap();

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(&nonce),
            &slot_aad(abs_lba, version),
            &mut buf,
            (&tag).into(),
        )
        .map_err(|_| CryptError::Corrupt)?;
    Ok(buf)
}

// ─────────────────────────────────────────────────────────────────────────────
// Version map (rollback detection)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-slot write counters, packed little-endian u16 into a sealed
/// sector payload (126 × 2 = 252 bytes ≤ SLOT_PAYLOAD). u16 caps a
/// slot at 65535 rewrites — `write_slot` refuses further writes rather
/// than wrapping (a wrapped counter would let an attacker replay the
/// generation-0 image).
pub fn pack_vmap(counters: &[u16; CRYPT_SLOTS as usize]) -> [u8; SLOT_PAYLOAD] {
    let mut out = [0u8; SLOT_PAYLOAD];
    for (i, c) in counters.iter().enumerate() {
        out[i * 2..i * 2 + 2].copy_from_slice(&c.to_le_bytes());
    }
    out
}

/// Inverse of [`pack_vmap`].
pub fn unpack_vmap(payload: &[u8; SLOT_PAYLOAD]) -> [u16; CRYPT_SLOTS as usize] {
    let mut out = [0u16; CRYPT_SLOTS as usize];
    for (i, c) in out.iter_mut().enumerate() {
        *c = u16::from_le_bytes([payload[i * 2], payload[i * 2 + 1]]);
    }
    out
}

/// Build the header sector: magic, salt, sealed passphrase check.
pub fn build_header(
    salt: &[u8; SALT_LEN],
    check_nonce: &[u8; NONCE_LEN],
    key: &[u8; 32],
) -> Result<[u8; SECTOR_SIZE], CryptError> {
    let mut check = *CHECK_PLAINTEXT;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(check_nonce), CHECK_AAD, &mut check)
        .map_err(|_| CryptError::BadArgument)?;

    let mut sector = [0u8; SECTOR_SIZE];
    sector[..8].copy_from_slice(MAGIC);
    sector[8..8 + SALT_LEN].copy_from_slice(salt);
    sector[24..24 + NONCE_LEN].copy_from_slice(check_nonce);
    sector[36..36 + TAG_LEN].copy_from_slice(&tag);
    sector[52..52 + CHECK_PLAINTEXT.len()].copy_from_slice(&check);
    Ok(sector)
}

/// Parse + verify a header sector against `passphrase`. On success
/// returns the derived key.
pub fn open_header(
    sector: &[u8; SECTOR_SIZE],
    passphrase: &[u8],
) -> Result<[u8; 32], CryptError> {
    if &sector[..8] != MAGIC {
        return Err(CryptError::NotFormatted);
    }
    let salt: [u8; SALT_LEN] = sector[8..8 + SALT_LEN].try_into().unwrap();
    let nonce: [u8; NONCE_LEN] = sector[24..24 + NONCE_LEN].try_into().unwrap();
    let tag: [u8; TAG_LEN] = sector[36..36 + TAG_LEN].try_into().unwrap();
    let mut check: [u8; 16] = sector[52..52 + CHECK_PLAINTEXT.len()].try_into().unwrap();

    let key = derive_key(&salt, passphrase);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher
        .decrypt_in_place_detached(Nonce::from_slice(&nonce), CHECK_AAD, &mut check, (&tag).into())
        .map_err(|_| CryptError::BadPassphrase)?;
    if &check != CHECK_PLAINTEXT {
        return Err(CryptError::BadPassphrase);
    }
    Ok(key)
}

// ─────────────────────────────────────────────────────────────────────────────
// CryptDisk — IPC + entropy wrapper (beetos builds only)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(beetos)]
mod disk {
    use super::*;
    use beetos_api_block::BlockClient;

    /// Fill `out` with kernel entropy (16 bytes per syscall).
    ///
    /// **Fails loudly on entropy failure.** Earlier this fell back to
    /// fixed constants — that would have made every nonce identical
    /// and broken GCM completely (key recovery). In practice
    /// `GetRandom` cannot refuse, but if it ever did we'd much rather
    /// crash than silently destroy the security of every subsequent
    /// sealed sector.
    pub fn rand_bytes(out: &mut [u8]) {
        let mut i = 0;
        while i < out.len() {
            let (r0, r1) = match xous::rsyscall(xous::SysCall::GetRandom) {
                Ok(xous::Result::Scalar2(a, b)) => (a as u64, b as u64),
                _ => panic!("cryptblock: GetRandom failed — refusing to seal with predictable nonce"),
            };
            for half in [r0, r1] {
                for b in half.to_le_bytes() {
                    if i >= out.len() {
                        break;
                    }
                    out[i] = b;
                    i += 1;
                }
            }
        }
    }

    /// An open (passphrase-verified) encrypted area.
    ///
    /// All IO goes through one caller-provided IPC page (`buf`), so
    /// the type itself stays allocation-free and copy-cheap.
    #[derive(Clone, Copy)]
    pub struct CryptDisk {
        client: BlockClient,
        base_lba: u64,
        key: [u8; 32],
    }

    impl CryptDisk {
        fn area_base(client: &BlockClient) -> Result<u64, CryptError> {
            let info = client.info().map_err(|_| CryptError::Io)?;
            // The whole area layout assumes 512-byte sectors (SECTOR_SIZE):
            // read_sector/write_sector slice exactly SECTOR_SIZE, and
            // area_base counts capacity in those units. A device that
            // reported a different block size would silently read/write
            // misaligned sectors, so refuse rather than corrupt.
            if info.block_size as usize != SECTOR_SIZE {
                return Err(CryptError::BadArgument);
            }
            if info.capacity_blocks < CRYPT_AREA_SECTORS {
                return Err(CryptError::NotFormatted);
            }
            Ok(info.capacity_blocks - CRYPT_AREA_SECTORS)
        }

        fn read_sector(
            client: &BlockClient,
            buf: xous::MemoryRange,
            lba: u64,
        ) -> Result<[u8; SECTOR_SIZE], CryptError> {
            client.read_blocks(lba, 1, buf).map_err(|_| CryptError::Io)?;
            let slice = unsafe { core::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
            Ok(beetos_api_block::data(slice)[..SECTOR_SIZE]
                .try_into()
                .unwrap())
        }

        fn write_sector(
            client: &BlockClient,
            buf: xous::MemoryRange,
            lba: u64,
            sector: &[u8; SECTOR_SIZE],
        ) -> Result<(), CryptError> {
            let slice =
                unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };
            beetos_api_block::data_mut(slice)[..SECTOR_SIZE].copy_from_slice(sector);
            client.write_blocks(lba, 1, buf).map_err(|_| CryptError::Io)
        }

        /// Format the area: zero all slots, then commit the header.
        ///
        /// **Order matters.** A crash between header and slot-zero
        /// writes would leave an area that *opens* (valid header) but
        /// whose unwritten slots still hold the previous key's
        /// ciphertext, surfacing as `Corrupt` to every read — looking
        /// like silent tampering. Zeroing first means a crash mid-
        /// format leaves the *header* missing, so `open` returns
        /// `NotFormatted` and the next `format` cleanly retries from
        /// scratch.
        pub fn format(
            client: BlockClient,
            buf: xous::MemoryRange,
            passphrase: &[u8],
        ) -> Result<Self, CryptError> {
            let base = Self::area_base(&client)?;

            let zero = [0u8; SECTOR_SIZE];
            for slot in 0..CRYPT_SLOTS {
                Self::write_sector(&client, buf, base + 2 + slot, &zero)?;
            }

            let mut salt = [0u8; SALT_LEN];
            rand_bytes(&mut salt);
            let mut check_nonce = [0u8; NONCE_LEN];
            rand_bytes(&mut check_nonce);

            let key = derive_key(&salt, passphrase);

            // Version map before the header (same crash-ordering logic
            // as the slots): a crash before the header lands leaves the
            // area NotFormatted, never half-tracked.
            let disk = Self { client, base_lba: base, key };
            disk.write_vmap(buf, &[0u16; CRYPT_SLOTS as usize])?;

            let header = build_header(&salt, &check_nonce, &key)?;
            Self::write_sector(&client, buf, base, &header)?;

            Ok(disk)
        }

        /// Open an existing area, verifying `passphrase` against the
        /// header's authenticated check value.
        pub fn open(
            client: BlockClient,
            buf: xous::MemoryRange,
            passphrase: &[u8],
        ) -> Result<Self, CryptError> {
            let base = Self::area_base(&client)?;
            let header = Self::read_sector(&client, buf, base)?;
            let key = open_header(&header, passphrase)?;
            Ok(Self { client, base_lba: base, key })
        }

        fn vmap_lba(&self) -> u64 {
            self.base_lba + 1
        }

        fn slot_lba(&self, slot: u64) -> u64 {
            self.base_lba + 2 + slot
        }

        /// Read + open the version map. The vmap participates in the
        /// AAD scheme with version 0 (it cannot version itself — see
        /// the module docs for what that does and does not protect).
        /// An all-zero vmap sector is `Corrupt`, not `Empty`: format
        /// always writes one, so its absence means tampering.
        fn read_vmap(
            &self,
            buf: xous::MemoryRange,
        ) -> Result<[u16; CRYPT_SLOTS as usize], CryptError> {
            let sector = Self::read_sector(&self.client, buf, self.vmap_lba())?;
            match open_sector(&self.key, self.vmap_lba(), 0, &sector) {
                Ok(payload) => Ok(unpack_vmap(&payload)),
                Err(CryptError::Empty) => Err(CryptError::Corrupt),
                Err(e) => Err(e),
            }
        }

        /// Seal + persist the version map with a fresh nonce.
        fn write_vmap(
            &self,
            buf: xous::MemoryRange,
            counters: &[u16; CRYPT_SLOTS as usize],
        ) -> Result<(), CryptError> {
            let mut nonce = [0u8; NONCE_LEN];
            rand_bytes(&mut nonce);
            let payload = pack_vmap(counters);
            let sector = seal_sector(&self.key, &nonce, self.vmap_lba(), 0, &payload)?;
            Self::write_sector(&self.client, buf, self.vmap_lba(), &sector)
        }

        /// Seal `payload` (≤ [`SLOT_PAYLOAD`] bytes) into `slot` with a
        /// fresh random nonce and the next write-generation, then
        /// persist slot and version map.
        ///
        /// Ordering: slot first, vmap second. A crash in between leaves
        /// the slot sealed at generation N+1 while the vmap still says
        /// N → the next read reports `Corrupt` (loud), never a silent
        /// stale read.
        pub fn write_slot(
            &self,
            buf: xous::MemoryRange,
            slot: u64,
            payload: &[u8],
        ) -> Result<(), CryptError> {
            if slot >= CRYPT_SLOTS {
                return Err(CryptError::BadArgument);
            }
            let mut counters = self.read_vmap(buf)?;
            let next = counters[slot as usize]
                .checked_add(1)
                .ok_or(CryptError::BadArgument)?; // wear cap — never wrap to 0
            let lba = self.slot_lba(slot);
            let mut nonce = [0u8; NONCE_LEN];
            rand_bytes(&mut nonce);
            let sector = seal_sector(&self.key, &nonce, lba, next, payload)?;
            Self::write_sector(&self.client, buf, lba, &sector)?;
            counters[slot as usize] = next;
            self.write_vmap(buf, &counters)
        }

        /// Read + authenticate `slot` at its current write-generation.
        /// [`CryptError::Empty`] for a never-written slot;
        /// [`CryptError::Corrupt`] for tampering INCLUDING a rolled-back
        /// (stale-but-genuine) sector.
        pub fn read_slot(
            &self,
            buf: xous::MemoryRange,
            slot: u64,
        ) -> Result<[u8; SLOT_PAYLOAD], CryptError> {
            if slot >= CRYPT_SLOTS {
                return Err(CryptError::BadArgument);
            }
            let counters = self.read_vmap(buf)?;
            let lba = self.slot_lba(slot);
            let sector = Self::read_sector(&self.client, buf, lba)?;
            open_sector(&self.key, lba, counters[slot as usize], &sector)
        }

        /// Erase `slot` back to the never-written state (raw zero
        /// sector — subsequent reads return [`CryptError::Empty`]).
        ///
        /// Bumps the write counter BEFORE zeroing: with the counter
        /// bumped, restoring the pre-erase sealed image fails auth
        /// (`Corrupt`) instead of silently resurrecting the file. The
        /// crash window (counter bumped, slot not yet zeroed) also
        /// reads `Corrupt` — loud, never stale.
        pub fn erase_slot(
            &self,
            buf: xous::MemoryRange,
            slot: u64,
        ) -> Result<(), CryptError> {
            if slot >= CRYPT_SLOTS {
                return Err(CryptError::BadArgument);
            }
            let mut counters = self.read_vmap(buf)?;
            // Same wear cap as write_slot: a saturated counter would
            // leave the last sealed image forever-valid, so a worn-out
            // slot can neither be rewritten nor silently erased.
            counters[slot as usize] = counters[slot as usize]
                .checked_add(1)
                .ok_or(CryptError::BadArgument)?;
            self.write_vmap(buf, &counters)?;
            Self::write_sector(&self.client, buf, self.slot_lba(slot), &[0u8; SECTOR_SIZE])
        }
    }
}

#[cfg(beetos)]
pub use disk::{rand_bytes, CryptDisk};

// ─────────────────────────────────────────────────────────────────────────────
// Host-side unit tests for the pure layer
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];
    const NONCE: [u8; NONCE_LEN] = [9u8; NONCE_LEN];

    #[test]
    fn seal_open_roundtrip() {
        let sector = seal_sector(&KEY, &NONCE, 42, 1, b"hello crypt").unwrap();
        let plain = open_sector(&KEY, 42, 1, &sector).unwrap();
        assert_eq!(&plain[..11], b"hello crypt");
        assert!(plain[11..].iter().all(|&b| b == 0), "padding must be zero");
    }

    #[test]
    fn tamper_detected() {
        let mut sector = seal_sector(&KEY, &NONCE, 42, 1, b"payload").unwrap();
        sector[100] ^= 0x01;
        assert_eq!(open_sector(&KEY, 42, 1, &sector), Err(CryptError::Corrupt));
    }

    #[test]
    fn wrong_lba_detected() {
        // A sealed sector moved to another LBA must fail (AAD binding).
        let sector = seal_sector(&KEY, &NONCE, 42, 1, b"payload").unwrap();
        assert_eq!(open_sector(&KEY, 43, 1, &sector), Err(CryptError::Corrupt));
    }

    #[test]
    fn rollback_detected() {
        // The core anti-rollback property: a stale-but-genuine sealed
        // sector (older write-generation) must fail authentication
        // when the version map says the slot has moved on.
        let v1 = seal_sector(&KEY, &NONCE, 42, 1, b"old secret").unwrap();
        let n2: [u8; NONCE_LEN] = [11u8; NONCE_LEN];
        let v2 = seal_sector(&KEY, &n2, 42, 2, b"new secret").unwrap();
        // Current generation opens fine…
        assert!(open_sector(&KEY, 42, 2, &v2).is_ok());
        // …but restoring the generation-1 image reads Corrupt, not
        // "old secret".
        assert_eq!(open_sector(&KEY, 42, 2, &v1), Err(CryptError::Corrupt));
    }

    #[test]
    fn erased_slot_restore_detected() {
        // erase_slot bumps the counter before zeroing, so the
        // pre-erase image (sealed at generation N) fails against the
        // post-erase expectation (N+1).
        let pre_erase = seal_sector(&KEY, &NONCE, 42, 3, b"deleted!").unwrap();
        assert_eq!(open_sector(&KEY, 42, 4, &pre_erase), Err(CryptError::Corrupt));
    }

    #[test]
    fn vmap_pack_roundtrip() {
        let mut counters = [0u16; CRYPT_SLOTS as usize];
        counters[0] = 1;
        counters[7] = 0xBEEF;
        counters[CRYPT_SLOTS as usize - 1] = u16::MAX;
        let packed = pack_vmap(&counters);
        assert_eq!(unpack_vmap(&packed), counters);
        // The packed map must fit a sealed sector payload with room to
        // spare (compile-time sanity of the 126×2 ≤ 484 assumption).
        assert!(CRYPT_SLOTS as usize * 2 <= SLOT_PAYLOAD);
    }

    #[test]
    fn wrong_key_detected() {
        let sector = seal_sector(&KEY, &NONCE, 42, 1, b"payload").unwrap();
        let other = [8u8; 32];
        assert_eq!(open_sector(&other, 42, 1, &sector), Err(CryptError::Corrupt));
    }

    #[test]
    fn empty_sector_is_empty() {
        let zero = [0u8; SECTOR_SIZE];
        assert_eq!(open_sector(&KEY, 42, 1, &zero), Err(CryptError::Empty));
    }

    #[test]
    fn payload_too_big_rejected() {
        let big = [0u8; SLOT_PAYLOAD + 1];
        assert_eq!(
            seal_sector(&KEY, &NONCE, 0, 1, &big),
            Err(CryptError::BadArgument)
        );
    }

    #[test]
    fn ciphertext_hides_plaintext() {
        let needle = b"SUPER_SECRET_MARKER";
        let sector = seal_sector(&KEY, &NONCE, 7, 1, needle).unwrap();
        // The plaintext must not appear anywhere in the sealed sector.
        assert!(
            !sector.windows(needle.len()).any(|w| w == needle),
            "plaintext leaked into sealed sector"
        );
    }

    #[test]
    fn header_roundtrip_and_bad_pass() {
        let salt = [3u8; 16];
        let key = derive_key(&salt, b"hunter2");
        let header = build_header(&salt, &NONCE, &key).unwrap();

        let reopened = open_header(&header, b"hunter2").unwrap();
        assert_eq!(reopened, key);

        assert_eq!(
            open_header(&header, b"wrongpass"),
            Err(CryptError::BadPassphrase)
        );
    }

    #[test]
    fn unformatted_header_detected() {
        let zero = [0u8; SECTOR_SIZE];
        assert_eq!(
            open_header(&zero, b"any"),
            Err(CryptError::NotFormatted)
        );
    }

    #[test]
    fn derive_key_is_salted() {
        let k1 = derive_key(&[1u8; 16], b"pass");
        let k2 = derive_key(&[2u8; 16], b"pass");
        assert_ne!(k1, k2);
    }
}
