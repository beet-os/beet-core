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
//! base+1 .. base+N    slots:  one sealed sector each
//! ```
//!
//! Every sealed sector is self-contained:
//!
//! ```text
//! [ nonce (12) | tag (16) | ciphertext (484) ]
//! ```
//!
//! - **Nonce**: fresh kernel entropy (`SysCall::GetRandom`) on every
//!   write — no counters to persist, no reuse across crash/reboot.
//! - **AAD**: the slot's absolute LBA. A sealed sector copied to a
//!   different LBA fails authentication, so ciphertext can't be
//!   shuffled around the disk undetected.
//! - **Tag**: GCM authentication over ciphertext + AAD. Any bit flip
//!   in nonce, payload or position is detected at open time.
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
/// Header + 127 slots.
pub const CRYPT_AREA_SECTORS: u64 = 128;

/// Usable slots (sector 0 of the area is the header).
pub const CRYPT_SLOTS: u64 = CRYPT_AREA_SECTORS - 1;

pub const SECTOR_SIZE: usize = 512;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// Plaintext bytes that fit in one sealed sector.
pub const SLOT_PAYLOAD: usize = SECTOR_SIZE - NONCE_LEN - TAG_LEN;

/// Header magic — bumped if the layout ever changes.
pub const MAGIC: &[u8; 8] = b"BEETCRY1";

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

/// AAD binding a sealed sector to its absolute disk position.
fn lba_aad(abs_lba: u64) -> [u8; 8] {
    abs_lba.to_le_bytes()
}

/// Seal `payload` (≤ [`SLOT_PAYLOAD`] bytes, zero-padded) into a
/// 512-byte sector image bound to `abs_lba`.
pub fn seal_sector(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    abs_lba: u64,
    payload: &[u8],
) -> Result<[u8; SECTOR_SIZE], CryptError> {
    if payload.len() > SLOT_PAYLOAD {
        return Err(CryptError::BadArgument);
    }
    let mut buf = [0u8; SLOT_PAYLOAD];
    buf[..payload.len()].copy_from_slice(payload);

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(nonce), &lba_aad(abs_lba), &mut buf)
        .map_err(|_| CryptError::BadArgument)?;

    let mut sector = [0u8; SECTOR_SIZE];
    sector[..NONCE_LEN].copy_from_slice(nonce);
    sector[NONCE_LEN..NONCE_LEN + TAG_LEN].copy_from_slice(&tag);
    sector[NONCE_LEN + TAG_LEN..].copy_from_slice(&buf);
    Ok(sector)
}

/// Open a sealed 512-byte sector. Returns the decrypted payload, or
/// [`CryptError::Empty`] for an all-zero (never written) sector, or
/// [`CryptError::Corrupt`] when authentication fails.
pub fn open_sector(
    key: &[u8; 32],
    abs_lba: u64,
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
            &lba_aad(abs_lba),
            &mut buf,
            (&tag).into(),
        )
        .map_err(|_| CryptError::Corrupt)?;
    Ok(buf)
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

        /// Format the area: fresh salt, header written, all slots
        /// zeroed (zero = "empty", see [`open_sector`]).
        pub fn format(
            client: BlockClient,
            buf: xous::MemoryRange,
            passphrase: &[u8],
        ) -> Result<Self, CryptError> {
            let base = Self::area_base(&client)?;

            let mut salt = [0u8; SALT_LEN];
            rand_bytes(&mut salt);
            let mut check_nonce = [0u8; NONCE_LEN];
            rand_bytes(&mut check_nonce);

            let key = derive_key(&salt, passphrase);
            let header = build_header(&salt, &check_nonce, &key)?;
            Self::write_sector(&client, buf, base, &header)?;

            let zero = [0u8; SECTOR_SIZE];
            for slot in 0..CRYPT_SLOTS {
                Self::write_sector(&client, buf, base + 1 + slot, &zero)?;
            }

            Ok(Self { client, base_lba: base, key })
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

        /// Seal `payload` (≤ [`SLOT_PAYLOAD`] bytes) into `slot` with a
        /// fresh random nonce and persist it.
        pub fn write_slot(
            &self,
            buf: xous::MemoryRange,
            slot: u64,
            payload: &[u8],
        ) -> Result<(), CryptError> {
            if slot >= CRYPT_SLOTS {
                return Err(CryptError::BadArgument);
            }
            let lba = self.base_lba + 1 + slot;
            let mut nonce = [0u8; NONCE_LEN];
            rand_bytes(&mut nonce);
            let sector = seal_sector(&self.key, &nonce, lba, payload)?;
            Self::write_sector(&self.client, buf, lba, &sector)
        }

        /// Read + authenticate `slot`. [`CryptError::Empty`] for a
        /// never-written slot.
        pub fn read_slot(
            &self,
            buf: xous::MemoryRange,
            slot: u64,
        ) -> Result<[u8; SLOT_PAYLOAD], CryptError> {
            if slot >= CRYPT_SLOTS {
                return Err(CryptError::BadArgument);
            }
            let lba = self.base_lba + 1 + slot;
            let sector = Self::read_sector(&self.client, buf, lba)?;
            open_sector(&self.key, lba, &sector)
        }

        /// Erase `slot` back to the never-written state (raw zero
        /// sector — subsequent reads return [`CryptError::Empty`]).
        pub fn erase_slot(
            &self,
            buf: xous::MemoryRange,
            slot: u64,
        ) -> Result<(), CryptError> {
            if slot >= CRYPT_SLOTS {
                return Err(CryptError::BadArgument);
            }
            let lba = self.base_lba + 1 + slot;
            Self::write_sector(&self.client, buf, lba, &[0u8; SECTOR_SIZE])
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
        let sector = seal_sector(&KEY, &NONCE, 42, b"hello crypt").unwrap();
        let plain = open_sector(&KEY, 42, &sector).unwrap();
        assert_eq!(&plain[..11], b"hello crypt");
        assert!(plain[11..].iter().all(|&b| b == 0), "padding must be zero");
    }

    #[test]
    fn tamper_detected() {
        let mut sector = seal_sector(&KEY, &NONCE, 42, b"payload").unwrap();
        sector[100] ^= 0x01;
        assert_eq!(open_sector(&KEY, 42, &sector), Err(CryptError::Corrupt));
    }

    #[test]
    fn wrong_lba_detected() {
        // A sealed sector moved to another LBA must fail (AAD binding).
        let sector = seal_sector(&KEY, &NONCE, 42, b"payload").unwrap();
        assert_eq!(open_sector(&KEY, 43, &sector), Err(CryptError::Corrupt));
    }

    #[test]
    fn wrong_key_detected() {
        let sector = seal_sector(&KEY, &NONCE, 42, b"payload").unwrap();
        let other = [8u8; 32];
        assert_eq!(open_sector(&other, 42, &sector), Err(CryptError::Corrupt));
    }

    #[test]
    fn empty_sector_is_empty() {
        let zero = [0u8; SECTOR_SIZE];
        assert_eq!(open_sector(&KEY, 42, &zero), Err(CryptError::Empty));
    }

    #[test]
    fn payload_too_big_rejected() {
        let big = [0u8; SLOT_PAYLOAD + 1];
        assert_eq!(
            seal_sector(&KEY, &NONCE, 0, &big),
            Err(CryptError::BadArgument)
        );
    }

    #[test]
    fn ciphertext_hides_plaintext() {
        let needle = b"SUPER_SECRET_MARKER";
        let sector = seal_sector(&KEY, &NONCE, 7, needle).unwrap();
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
