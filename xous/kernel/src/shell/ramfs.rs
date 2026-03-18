// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory RAM filesystem (ramfs) for BeetOS.
//!
//! Uses fixed-size static buffers — no heap allocator needed.
//! Files are stored in a flat table. Paths support a single `/` level
//! (e.g., `/tmp/hello.txt`). Hierarchical directories are tracked
//! implicitly via path prefixes.
//!
//! Limits:
//!   - MAX_FILES: 64 files
//!   - MAX_NAME_LEN: 63 bytes (+ null terminator)
//!   - MAX_FILE_SIZE: 4096 bytes per file

/// Maximum number of files in the filesystem.
const MAX_FILES: usize = 64;

/// Maximum filename/path length (excluding null terminator).
const MAX_NAME_LEN: usize = 63;

/// Maximum file content size in bytes.
const MAX_FILE_SIZE: usize = 4096;

/// A single file entry in the filesystem.
struct FileEntry {
    /// File path (null-terminated, e.g., "/tmp/hello.txt").
    name: [u8; MAX_NAME_LEN + 1],
    /// File content bytes.
    data: [u8; MAX_FILE_SIZE],
    /// Number of valid bytes in `data`.
    len: usize,
    /// Whether this slot is in use.
    used: bool,
    /// Whether this entry is a directory (not a regular file).
    is_dir: bool,
}

impl FileEntry {
    const fn empty() -> Self {
        FileEntry {
            name: [0u8; MAX_NAME_LEN + 1],
            data: [0u8; MAX_FILE_SIZE],
            len: 0,
            used: false,
            is_dir: false,
        }
    }

    fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        // SAFETY: we only store valid UTF-8 names
        unsafe { core::str::from_utf8_unchecked(&self.name[..end]) }
    }

    fn set_name(&mut self, name: &str) {
        let len = name.len().min(MAX_NAME_LEN);
        self.name[..len].copy_from_slice(&name.as_bytes()[..len]);
        self.name[len] = 0;
    }
}

/// The filesystem table. Static so no allocator needed.
static mut FS: [FileEntry; MAX_FILES] = {
    const EMPTY: FileEntry = FileEntry::empty();
    [EMPTY; MAX_FILES]
};

/// Error type for filesystem operations.
#[derive(Debug)]
pub enum FsError {
    NotFound,
    AlreadyExists,
    NoSpace,
    NameTooLong,
    FileTooLarge,
    IsDirectory,
    NotDirectory,
    NotEmpty,
}

/// Normalize a path: ensure it starts with `/`, remove trailing `/`.
fn normalize_path<'a>(path: &'a str, buf: &'a mut [u8; MAX_NAME_LEN + 1]) -> &'a str {
    let path = path.trim();
    let mut pos = 0;

    if !path.starts_with('/') {
        buf[0] = b'/';
        pos = 1;
    }

    let bytes = path.as_bytes();
    let copy_len = bytes.len().min(MAX_NAME_LEN - pos);
    buf[pos..pos + copy_len].copy_from_slice(&bytes[..copy_len]);
    pos += copy_len;

    // Remove trailing slash (unless root)
    if pos > 1 && buf[pos - 1] == b'/' {
        pos -= 1;
    }

    buf[pos] = 0;
    unsafe { core::str::from_utf8_unchecked(&buf[..pos]) }
}

/// Find a file by path. Returns the index if found.
fn find_entry(path: &str) -> Option<usize> {
    unsafe {
        for i in 0..MAX_FILES {
            if FS[i].used && FS[i].name_str() == path {
                return Some(i);
            }
        }
    }
    None
}

/// Find a free slot.
fn find_free() -> Option<usize> {
    unsafe {
        for i in 0..MAX_FILES {
            if !FS[i].used {
                return Some(i);
            }
        }
    }
    None
}

/// Initialize the filesystem with a root directory.
pub fn init() {
    unsafe {
        FS[0].set_name("/");
        FS[0].used = true;
        FS[0].is_dir = true;
    }
}

/// Create a directory.
pub fn mkdir(path: &str) -> Result<(), FsError> {
    let mut buf = [0u8; MAX_NAME_LEN + 1];
    let path = normalize_path(path, &mut buf);

    if path.len() > MAX_NAME_LEN {
        return Err(FsError::NameTooLong);
    }
    if find_entry(path).is_some() {
        return Err(FsError::AlreadyExists);
    }

    let idx = find_free().ok_or(FsError::NoSpace)?;
    unsafe {
        FS[idx] = FileEntry::empty();
        FS[idx].set_name(path);
        FS[idx].used = true;
        FS[idx].is_dir = true;
    }
    Ok(())
}

/// Write (create or overwrite) a file with the given content.
pub fn write(path: &str, content: &[u8]) -> Result<(), FsError> {
    let mut buf = [0u8; MAX_NAME_LEN + 1];
    let path = normalize_path(path, &mut buf);

    if path.len() > MAX_NAME_LEN {
        return Err(FsError::NameTooLong);
    }
    if content.len() > MAX_FILE_SIZE {
        return Err(FsError::FileTooLarge);
    }

    // Check if file exists
    if let Some(idx) = find_entry(path) {
        unsafe {
            if FS[idx].is_dir {
                return Err(FsError::IsDirectory);
            }
            FS[idx].data[..content.len()].copy_from_slice(content);
            FS[idx].len = content.len();
        }
        return Ok(());
    }

    // Create new file
    let idx = find_free().ok_or(FsError::NoSpace)?;
    unsafe {
        FS[idx] = FileEntry::empty();
        FS[idx].set_name(path);
        FS[idx].data[..content.len()].copy_from_slice(content);
        FS[idx].len = content.len();
        FS[idx].used = true;
        FS[idx].is_dir = false;
    }
    Ok(())
}

/// Read a file's contents. Returns a slice into the static buffer.
pub fn read(path: &str) -> Result<&'static [u8], FsError> {
    let mut buf = [0u8; MAX_NAME_LEN + 1];
    let path = normalize_path(path, &mut buf);

    let idx = find_entry(path).ok_or(FsError::NotFound)?;
    unsafe {
        if FS[idx].is_dir {
            return Err(FsError::IsDirectory);
        }
        Ok(&FS[idx].data[..FS[idx].len])
    }
}

/// Remove a file or empty directory.
pub fn remove(path: &str) -> Result<(), FsError> {
    let mut buf = [0u8; MAX_NAME_LEN + 1];
    let path = normalize_path(path, &mut buf);

    if path == "/" {
        return Err(FsError::NotEmpty); // can't remove root
    }

    let idx = find_entry(path).ok_or(FsError::NotFound)?;

    // If it's a directory, check it's empty
    unsafe {
        if FS[idx].is_dir {
            // Check for children by seeing if any entry starts with "path/"
            let plen = path.len();
            for i in 0..MAX_FILES {
                if FS[i].used && i != idx {
                    let n = FS[i].name_str();
                    if n.len() > plen + 1
                        && n.as_bytes()[..plen] == path.as_bytes()[..plen]
                        && n.as_bytes()[plen] == b'/'
                    {
                        return Err(FsError::NotEmpty);
                    }
                }
            }
        }

        FS[idx].used = false;
    }
    Ok(())
}

/// List entries under a directory path. Calls `callback` for each entry
/// with (name, is_dir, size).
pub fn list<F>(dir_path: &str, mut callback: F) -> Result<(), FsError>
where
    F: FnMut(&str, bool, usize),
{
    let mut buf = [0u8; MAX_NAME_LEN + 1];
    let dir_path = normalize_path(dir_path, &mut buf);

    // Verify directory exists
    let dir_idx = find_entry(dir_path).ok_or(FsError::NotFound)?;
    unsafe {
        if !FS[dir_idx].is_dir {
            return Err(FsError::NotDirectory);
        }
    }

    unsafe {
        for i in 0..MAX_FILES {
            if !FS[i].used {
                continue;
            }
            let name = FS[i].name_str();

            // Skip the directory itself
            if name == dir_path {
                continue;
            }

            // Check if it's a direct child
            let is_child = if dir_path == "/" {
                // Root: direct children have exactly one `/` at position 0
                name.starts_with('/')
                    && name[1..].find('/').is_none()
            } else {
                name.starts_with(dir_path)
                    && name.as_bytes().get(dir_path.len()) == Some(&b'/')
                    && name[dir_path.len() + 1..].find('/').is_none()
            };

            if is_child {
                // Extract just the basename
                let basename = if let Some(pos) = name.rfind('/') {
                    &name[pos + 1..]
                } else {
                    name
                };
                callback(basename, FS[i].is_dir, FS[i].len);
            }
        }
    }
    Ok(())
}

/// Get filesystem statistics: (used_files, total_files, used_bytes).
pub fn stats() -> (usize, usize, usize) {
    let mut used = 0;
    let mut bytes = 0;
    unsafe {
        for i in 0..MAX_FILES {
            if FS[i].used {
                used += 1;
                bytes += FS[i].len;
            }
        }
    }
    (used, MAX_FILES, bytes)
}
