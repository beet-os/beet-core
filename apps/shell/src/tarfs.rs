// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read-only tar filesystem parser (POSIX ustar format).
//!
//! Parses a tar archive from a memory-mapped byte slice. Used by the shell
//! to read files from the virtio-blk disk image.

/// Tar header size (always 512 bytes).
const HEADER_SIZE: usize = 512;

/// A tar entry header (ustar format).
struct TarHeader<'a> {
    data: &'a [u8; HEADER_SIZE],
}

impl<'a> TarHeader<'a> {
    /// File name (first 100 bytes, NUL-terminated).
    fn name(&self) -> &str {
        let name_bytes = &self.data[0..100];
        let len = name_bytes.iter().position(|&b| b == 0).unwrap_or(100);
        core::str::from_utf8(&name_bytes[..len]).unwrap_or("")
    }

    /// File size in bytes (octal string at offset 124, 12 bytes).
    fn size(&self) -> usize {
        parse_octal(&self.data[124..136])
    }

    /// Type flag (offset 156).
    /// '0' or '\0' = regular file, '5' = directory.
    fn type_flag(&self) -> u8 {
        self.data[156]
    }

    /// Is this a regular file?
    fn is_file(&self) -> bool {
        self.type_flag() == b'0' || self.type_flag() == 0
    }

    /// Is this a directory?
    fn is_dir(&self) -> bool {
        self.type_flag() == b'5'
    }

    /// Is this a valid header? (Check for zeroed block = end of archive.)
    fn is_valid(&self) -> bool {
        // A zeroed 512-byte block marks end of archive.
        self.data[0] != 0
    }
}

/// Parse an octal ASCII string into a usize.
fn parse_octal(bytes: &[u8]) -> usize {
    let mut result: usize = 0;
    for &b in bytes {
        if b == 0 || b == b' ' {
            break;
        }
        if b >= b'0' && b <= b'7' {
            result = result * 8 + (b - b'0') as usize;
        }
    }
    result
}

/// Round up to the next multiple of 512.
fn round_up_512(n: usize) -> usize {
    (n + 511) & !511
}

/// A tar archive backed by a byte slice.
pub struct TarArchive<'a> {
    data: &'a [u8],
}

impl<'a> TarArchive<'a> {
    /// Create a new tar archive from a byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        TarArchive { data }
    }

    /// Find a file by path and return its contents.
    pub fn find(&self, path: &str) -> Option<&'a [u8]> {
        let normalized = path.strip_prefix('/').unwrap_or(path);
        let mut offset = 0;
        while offset + HEADER_SIZE <= self.data.len() {
            let header_bytes: &[u8; HEADER_SIZE] =
                self.data[offset..offset + HEADER_SIZE].try_into().ok()?;
            let header = TarHeader { data: header_bytes };

            if !header.is_valid() {
                break;
            }

            let size = header.size();
            let data_offset = offset + HEADER_SIZE;

            let name = header.name();
            let name_trimmed = name.strip_suffix('/').unwrap_or(name);
            let normalized_trimmed = normalized.strip_suffix('/').unwrap_or(normalized);

            if name_trimmed == normalized_trimmed && header.is_file() {
                if data_offset + size <= self.data.len() {
                    return Some(&self.data[data_offset..data_offset + size]);
                }
            }

            offset = data_offset + round_up_512(size);
        }
        None
    }

    /// List entries in a directory. Calls `callback(name, is_dir, size)` for each.
    pub fn list<F: FnMut(&str, bool, usize)>(&self, dir: &str, mut callback: F) {
        let normalized = dir.strip_prefix('/').unwrap_or(dir);
        // Ensure trailing slash for non-root.
        let prefix = if normalized.is_empty() {
            ""
        } else if normalized.ends_with('/') {
            normalized
        } else {
            // We can't dynamically allocate, so we handle this case carefully.
            // For now, support only root listing (empty prefix) or exact prefix matches.
            normalized
        };

        let mut offset = 0;
        while offset + HEADER_SIZE <= self.data.len() {
            let header_bytes: &[u8; HEADER_SIZE] = match self.data[offset..offset + HEADER_SIZE].try_into() {
                Ok(h) => h,
                Err(_) => break,
            };
            let header = TarHeader { data: header_bytes };

            if !header.is_valid() {
                break;
            }

            let size = header.size();
            let name = header.name();

            // Check if this entry is a direct child of the prefix.
            let relative = if prefix.is_empty() {
                name
            } else if let Some(rest) = name.strip_prefix(prefix) {
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                if rest.is_empty() {
                    // This is the directory itself, skip it.
                    offset = offset + HEADER_SIZE + round_up_512(size);
                    continue;
                }
                rest
            } else {
                offset = offset + HEADER_SIZE + round_up_512(size);
                continue;
            };

            // Only show direct children (no nested slashes except trailing).
            let clean = relative.strip_suffix('/').unwrap_or(relative);
            if !clean.contains('/') {
                callback(clean, header.is_dir(), size);
            }

            offset = offset + HEADER_SIZE + round_up_512(size);
        }
    }

    /// Return total number of entries.
    pub fn count(&self) -> usize {
        let mut n = 0;
        let mut offset = 0;
        while offset + HEADER_SIZE <= self.data.len() {
            let header_bytes: &[u8; HEADER_SIZE] = match self.data[offset..offset + HEADER_SIZE].try_into() {
                Ok(h) => h,
                Err(_) => break,
            };
            let header = TarHeader { data: header_bytes };
            if !header.is_valid() {
                break;
            }
            n += 1;
            let size = header.size();
            offset = offset + HEADER_SIZE + round_up_512(size);
        }
        n
    }
}
