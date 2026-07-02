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

/// Parse a tar numeric field into a usize.
///
/// Handles the awkward corners of the ustar size field:
/// - **Leading padding:** some writers space-pad the field (`"   12345\0"`).
///   The old code broke on the first space and returned 0, which made the
///   archive walk step by only one block and desync into the file data.
/// - **Trailing padding:** the field ends at the first space or NUL.
/// - **Invalid bytes:** stop rather than silently fold them into the value.
/// - **GNU base-256:** if the high bit of the first byte is set, the field is a
///   big-endian binary integer, not octal ASCII.
fn parse_octal(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }

    // GNU base-256 extension: high bit of the first byte marks binary encoding.
    if bytes[0] & 0x80 != 0 {
        let mut result: usize = (bytes[0] & 0x7f) as usize;
        for &b in &bytes[1..] {
            result = (result << 8) | b as usize;
        }
        return result;
    }

    let mut result: usize = 0;
    let mut seen_digit = false;
    for &b in bytes {
        match b {
            b'0'..=b'7' => {
                result = result * 8 + (b - b'0') as usize;
                seen_digit = true;
            }
            // Space or NUL: skip while still in the leading pad, stop once the
            // number has started (trailing terminator).
            b' ' | 0 => {
                if seen_digit {
                    break;
                }
            }
            // Anything else is malformed — stop rather than corrupt the value.
            _ => break,
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
        // Normalise the requested directory to a slash-free path component
        // (e.g. "/bin/" → "bin").  Entry names are likewise stripped of any
        // leading slash before comparison.
        let normalized = dir.strip_prefix('/').unwrap_or(dir);
        let prefix = normalized.strip_suffix('/').unwrap_or(normalized);

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
            let name = name.strip_prefix('/').unwrap_or(name);
            let step = HEADER_SIZE + round_up_512(size);

            // Check if this entry is a direct child of the prefix.  The match
            // must land on a path boundary: prefix "bin" matches "bin/ls" but
            // NOT "binary.dat" (which would otherwise be listed as "ary.dat").
            let relative = if prefix.is_empty() {
                name
            } else if let Some(rest) = name.strip_prefix(prefix) {
                match rest.strip_prefix('/') {
                    Some(after) if !after.is_empty() => after,
                    // Either the directory entry itself (rest == "" or "/"),
                    // or a partial-component false match — skip in both cases.
                    _ => {
                        offset += step;
                        continue;
                    }
                }
            } else {
                offset += step;
                continue;
            };

            // Only show direct children (no nested slashes except trailing).
            let clean = relative.strip_suffix('/').unwrap_or(relative);
            if !clean.contains('/') {
                callback(clean, header.is_dir(), size);
            }

            offset += step;
        }
    }

    /// Returns true if `dir` is a valid directory in the archive.
    ///
    /// Accepts both explicit directory entries (type `5`) and implicit
    /// directories inferred from any entry whose path starts with `dir/`.
    pub fn has_dir(&self, dir: &str) -> bool {
        let dir = dir.strip_prefix('/').unwrap_or(dir);
        let dir = dir.strip_suffix('/').unwrap_or(dir);

        if dir.is_empty() {
            return true; // archive root always exists
        }

        let mut offset = 0;
        while offset + HEADER_SIZE <= self.data.len() {
            let header_bytes: &[u8; HEADER_SIZE] = match self.data[offset..offset + HEADER_SIZE].try_into() {
                Ok(h) => h,
                Err(_) => break,
            };
            let header = TarHeader { data: header_bytes };
            if !header.is_valid() { break; }
            let size = header.size();
            let name = header.name();
            let name = name.strip_prefix('/').unwrap_or(name);
            let name_clean = name.strip_suffix('/').unwrap_or(name);

            // Explicit directory entry
            if name_clean == dir && header.is_dir() {
                return true;
            }
            // Implicit directory: any entry whose path starts with dir/
            if name.starts_with(dir) && name.as_bytes().get(dir.len()) == Some(&b'/') {
                return true;
            }

            offset += HEADER_SIZE + round_up_512(size);
        }
        false
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
