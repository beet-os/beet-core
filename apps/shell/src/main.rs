// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS interactive shell (bsh) — runs as a userspace process.
//!
//! Receives UART characters from the kernel via IPC, writes output to both
//! UART MMIO (mapped by kernel, x0) and the framebuffer console (mapped at
//! SHELL_FB_VA after calling AcquireDisplay).
//!
//! File operations (ls, cat, mkdir, rm, write) are delegated to the
//! filesystem service via Xous IPC (BlockingScalar).

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use beetos_api_fs::{BUF_STATUS_OFFSET, BUF_TEXT_OFFSET, FsError, FsOp, FS_SID};

// ============================================================================
// UART output via mapped MMIO (delegated to the shared PL011 driver in
// `beetos::pl011`). The shell wraps UART output below in `DualWriter`
// so every byte also lands on the framebuffer console.
// ============================================================================

use beetos::pl011::putc as uart_putc;

// ============================================================================
// Framebuffer console
// ============================================================================

use beetos::fb_console::FbConsole;

static mut FB_CONSOLE: Option<FbConsole> = None;

fn fb_putc(c: u8) {
    unsafe {
        if let Some(ref mut con) = FB_CONSOLE {
            con.putc(c);
        }
    }
}

/// Read the current cursor position from the FB console.
fn fb_cursor() -> (usize, usize) {
    unsafe {
        if let Some(ref con) = FB_CONSOLE {
            con.cursor()
        } else {
            (0, 0)
        }
    }
}

/// Acquire exclusive display ownership.
/// Blocks until the display is free, then returns the cursor position
/// left by the previous owner.  After this call the FB is mapped at
/// `SHELL_FB_VA` in this process.
fn acquire_display() -> (usize, usize) {
    match xous::rsyscall(xous::SysCall::AcquireDisplay) {
        Ok(xous::Result::Scalar2(row, col)) => (row, col),
        _ => (0, 0),
    }
}

/// Release exclusive display ownership, recording the current cursor.
/// After this call the FB is unmapped from this process.
fn release_display(row: usize, col: usize) {
    xous::rsyscall(xous::SysCall::ReleaseDisplay(row, col)).ok();
}

/// Claim keyboard input focus.
///
/// Must be called immediately after `acquire_display`.  Passes `CONSOLE_SID`
/// so the kernel routes subsequent keystrokes here, and drains any characters
/// buffered since the previous owner released the display.
fn acquire_input_focus() {
    let [a, b, c, d] = beetos_api_console::CONSOLE_SID;
    xous::rsyscall(xous::SysCall::AcquireInputFocus(a, b, c, d)).ok();
}

/// Release keyboard input focus before blocking in SpawnAndWait.
///
/// Subsequent keystrokes go to the kernel ring buffer so they are not
/// delivered to our (blocked) IPC queue.  The child will claim focus via
/// `AcquireInputFocus`; on return we reclaim it with `acquire_input_focus`.
fn release_input_focus() {
    xous::rsyscall(xous::SysCall::ReleaseInputFocus).ok();
}

// ============================================================================
// Combined output (FB + console service, UART fallback)
// ============================================================================
//
// Phase 2 of the console migration: the shell no longer writes the
// UART directly on the steady-state path — `os/console` owns the UART
// (and the TCP remote-console mirror). The shell keeps rendering the
// framebuffer console itself because it owns the display while it
// holds it (AcquireDisplay protocol). Until the service has registered
// (first instants of boot), bytes fall back to the direct UART write
// so early output is never lost.

fn putc(c: u8) {
    fb_putc(c);
    if console_out_cid() != 0 {
        tap_byte(c);
    } else {
        uart_putc(c);
    }
}

fn puts(s: &str) {
    for b in s.bytes() {
        fb_putc(b);
    }
    if console_out_cid() != 0 {
        // One IPC per 32-byte chunk: amortises the cost vs. one per
        // byte and matches how the shell batches output (a prompt, a
        // path, a help line — all small contiguous strings).
        tap_chunks(s.as_bytes());
    } else {
        for b in s.bytes() {
            uart_putc(b);
        }
    }
}

// ============================================================================
// Console output service
// ============================================================================
//
// CID is cached after the first successful Connect; before that the
// fallback above writes the UART directly (Connect behaves like
// TryConnect in BeetOS, so probing it on every call until the service
// appears costs one fast syscall, not a block).
static mut CONSOLE_OUT_CID: u32 = 0;

fn console_out_cid() -> u32 {
    unsafe {
        if CONSOLE_OUT_CID != 0 {
            return CONSOLE_OUT_CID;
        }
        let sid = xous::SID::from_array(beetos_api_console::CONSOLE_OUT_SID);
        match xous::rsyscall(xous::SysCall::Connect(sid)) {
            Ok(xous::Result::ConnectionID(cid)) => {
                CONSOLE_OUT_CID = cid;
                cid
            }
            _ => 0,
        }
    }
}

fn tap_byte(c: u8) {
    let cid = console_out_cid();
    if cid == 0 {
        return;
    }
    let scalar = xous::ScalarMessage {
        id: beetos_api_console::ConsoleOp::Putc as usize,
        arg1: c as usize,
        arg2: 0,
        arg3: 0,
        arg4: 0,
    };
    let _ = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::Scalar(scalar),
    ));
}

fn tap_chunks(bytes: &[u8]) {
    let cid = console_out_cid();
    if cid == 0 {
        return;
    }
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + beetos_api_console::WRITE_CHUNK).min(bytes.len());
        let (args, len) = beetos_api_console::pack_write(&bytes[i..end]);
        let scalar = xous::ScalarMessage {
            id: beetos_api_console::encode_write_id(len),
            arg1: args[0],
            arg2: args[1],
            arg3: args[2],
            arg4: args[3],
        };
        let _ = xous::rsyscall(xous::SysCall::SendMessage(
            cid,
            xous::Message::Scalar(scalar),
        ));
        i = end;
    }
}

struct DualWriter;
impl Write for DualWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        puts(s);
        Ok(())
    }
}

// ============================================================================
// Shell state machine
// ============================================================================

const MAX_LINE: usize = 256;
const MAX_ARGS: usize = 16;
const MAX_PATH: usize = 256;

struct Shell {
    line: [u8; MAX_LINE],
    pos: usize,
}

static mut SHELL: Shell = Shell {
    line: [0u8; MAX_LINE],
    pos: 0,
};

// Current working directory
static mut CWD_BUF: [u8; MAX_PATH] = [0u8; MAX_PATH];
static mut CWD_LEN: usize = 0;

// Previous working directory (for `cd -`)
static mut PREV_BUF: [u8; MAX_PATH] = [0u8; MAX_PATH];
static mut PREV_LEN: usize = 0;

fn cwd_str() -> &'static str {
    unsafe { core::str::from_utf8(&CWD_BUF[..CWD_LEN]).unwrap_or("/") }
}

/// Resolve `input` against CWD into `buf`, normalizing `.` and `..`.
fn resolve_path<'a>(input: &str, buf: &'a mut [u8; MAX_PATH]) -> &'a str {
    let mut tmp = [0u8; MAX_PATH];
    let mut tlen = 0usize;

    if !input.starts_with('/') {
        let cwd = unsafe { &CWD_BUF[..CWD_LEN] };
        let n = cwd.len().min(tmp.len());
        tmp[..n].copy_from_slice(cwd);
        tlen = n;
        if tlen < tmp.len() { tmp[tlen] = b'/'; tlen += 1; }
    }

    for b in input.bytes() {
        if tlen < tmp.len() { tmp[tlen] = b; tlen += 1; }
    }

    // Normalize: build output component by component
    buf[0] = b'/';
    let mut olen = 1usize;

    for comp in tmp[..tlen].split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                if olen > 1 {
                    olen -= 1;
                    while olen > 1 && buf[olen - 1] != b'/' { olen -= 1; }
                    if olen > 1 { olen -= 1; }
                }
            }
            _ => {
                if olen > 1 { buf[olen] = b'/'; olen += 1; }
                let n = comp.len().min(MAX_PATH - olen);
                buf[olen..olen + n].copy_from_slice(&comp[..n]);
                olen += n;
            }
        }
    }

    core::str::from_utf8(&buf[..olen]).unwrap_or("/")
}

fn prompt() { puts("bsh> "); }

fn process_char(c: u8) {
    unsafe {
        match c {
            0x7F | 0x08 => {
                if SHELL.pos > 0 {
                    SHELL.pos -= 1;
                    putc(0x08); putc(b' '); putc(0x08);
                }
            }
            b'\r' | b'\n' => {
                static mut LAST_WAS_CR: bool = false;
                if c == b'\n' && LAST_WAS_CR { LAST_WAS_CR = false; return; }
                LAST_WAS_CR = c == b'\r';
                putc(b'\n');
                let line_len = SHELL.pos;
                SHELL.pos = 0;
                if line_len > 0 {
                    let mut cmd_buf = [0u8; MAX_LINE];
                    cmd_buf[..line_len].copy_from_slice(&SHELL.line[..line_len]);
                    execute_line(&cmd_buf[..line_len]);
                }
                prompt();
            }
            0x03 => { puts("^C\n"); SHELL.pos = 0; prompt(); }
            0x04 => {
                if SHELL.pos == 0 {
                    puts("\n(type 'reboot' to restart)\n");
                    prompt();
                }
            }
            0x20..=0x7E => {
                if SHELL.pos < MAX_LINE - 1 {
                    SHELL.line[SHELL.pos] = c;
                    SHELL.pos += 1;
                    putc(c);
                }
            }
            _ => {}
        }
    }
}

fn execute_line(line: &[u8]) {
    let line_str = match core::str::from_utf8(line) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };
    if line_str.is_empty() { return; }

    let mut args: [&str; MAX_ARGS] = [""; MAX_ARGS];
    let mut argc = 0;
    for part in line_str.split_ascii_whitespace() {
        if argc < MAX_ARGS { args[argc] = part; argc += 1; }
    }
    if argc == 0 { return; }

    let cmd = args[0];
    let cmd_args = &args[1..argc];

    match cmd {
        // Shell builtins
        "help" => cmd_help(),
        "echo" => cmd_echo(cmd_args),
        "info" => cmd_info(),
        "pid" => cmd_pid(),
        "pwd" => cmd_pwd(),
        "cd" => cmd_cd(cmd_args),
        "programs" => cmd_programs(),
        "reboot" => cmd_reboot(),

        // FS operations (via IPC to fs service)
        "ls" => cmd_ls(cmd_args),
        "cat" => cmd_cat(cmd_args),
        "write" => cmd_write(cmd_args, line_str),
        "rm" => cmd_rm(cmd_args),
        "mkdir" => cmd_mkdir(cmd_args),
        "blkinfo" => cmd_blkinfo(),
        "dread" => cmd_dread(cmd_args),
        "dwrite" => cmd_dwrite(cmd_args, line_str),
        "cryptfmt" => cmd_cryptfmt(cmd_args),
        "cryptopen" => cmd_cryptopen(cmd_args),
        "cryptlock" => cmd_cryptlock(),
        "mem" => cmd_mem(),
        "ifconfig" => cmd_ifconfig(),
        "ping" => cmd_ping(cmd_args),
        "nettest-listen" => cmd_nettest_listen(cmd_args),
        "nettest-connect" => cmd_nettest_connect(cmd_args),
        "bench" => cmd_bench(),

        // External programs (spawned via procman)
        _ => try_spawn_via_procman(cmd, cmd_args),
    }
}

// ============================================================================
// Builtins
// ============================================================================

fn cmd_help() {
    puts("BeetOS shell commands:\n");
    puts("  help              Show this help\n");
    puts("  echo [text...]    Print text\n");
    puts("  info              System information\n");
    puts("  pid               Show current process ID\n");
    puts("  pwd               Print working directory\n");
    puts("  cd [path]         Change directory (default: /)\n");
    puts("  programs          List spawnable programs\n");
    puts("  reboot            Reboot the system\n");
    puts("  ls [path]         List directory (ramfs or /disk/)\n");
    puts("  cat <path>        Display file contents\n");
    puts("  write <path> <text>  Write text to a file (ramfs only)\n");
    puts("  rm <path>         Remove a file or empty directory\n");
    puts("  mkdir <path>      Create a directory\n");
    puts("  blkinfo           Block device info\n");
    puts("  dread <lba>       Read one 512-byte block (printable preview)\n");
    puts("  dwrite <lba> <text>  Write one 512-byte block (zero-padded)\n");
    puts("  cryptfmt <pass>   Format the encrypted /data area (AES-256-GCM)\n");
    puts("  cryptopen <pass>  Unlock /data (then use write/cat/ls/rm on /data/...)\n");
    puts("  cryptlock         Lock /data (drop the key)\n");
    puts("  mem               Filesystem statistics\n");
    puts("  ifconfig          Show network interface configuration\n");
    puts("  ping <ip> [count] ICMP echo (default 4 packets)\n");
    puts("  nettest-listen <port>     Echo server: listen, accept one, echo, close\n");
    puts("  nettest-connect <ip> <port>   Connect, send 'hi', print reply, close\n");
    puts("  bench             Kernel micro-benchmarks (syscall/IPC/MMU)\n");
}

fn cmd_echo(args: &[&str]) {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 { putc(b' '); }
        puts(arg);
    }
    putc(b'\n');
}

fn cmd_info() {
    puts("BeetOS v0.1.0\n");
    puts("Kernel: Xous microkernel (AArch64)\n");
    puts("Platform: QEMU virt\n");
    let _ = write!(DualWriter, "Page size: {} bytes\n", 16384);
    puts("Shell: userspace process (EL0)\n");
}

fn cmd_pid() {
    match xous::rsyscall(xous::SysCall::GetProcessId) {
        Ok(xous::Result::Scalar1(pid)) => { let _ = write!(DualWriter, "PID: {}\n", pid); }
        _ => puts("pid: syscall failed\n"),
    }
}

fn cmd_programs() {
    let mut index = 0usize;

    loop {
        match xous::rsyscall(xous::SysCall::GetBinaryName(index)) {
            Ok(xous::Result::Scalar5(w0, w1, w2, w3, _)) => {
                let packed = [w0, w1, w2, w3];
                let name = beetos_api_procman::unpack_name(&packed);

                if !name.is_empty() {
                    puts(name);
                    putc(b'\n');
                }

                index += 1;
            }
            _ => break,
        }
    }
}

fn cmd_reboot() {
    xous::rsyscall(xous::SysCall::Shutdown(0)).ok();
}

fn cmd_pwd() {
    puts(cwd_str());
    putc(b'\n');
}

fn cmd_cd(args: &[&str]) {
    let target = if args.is_empty() { "/" } else { args[0] };

    // `cd -` switches to the previous directory
    let mut buf = [0u8; MAX_PATH];
    let resolved = if target == "-" {
        let prev_len = unsafe { PREV_LEN };
        if prev_len == 0 {
            puts("cd: no previous directory\n");
            return;
        }
        unsafe { core::str::from_utf8(&PREV_BUF[..prev_len]).unwrap_or("/") }
    } else {
        resolve_path(target, &mut buf)
    };

    let packed = beetos_api_fs::pack_path(resolved);
    match fs_scalar(FsOp::IsDir, packed[0], packed[1], packed[2], packed[3]) {
        Some(code) if code == FsError::Ok as usize => {
            unsafe {
                // Save current CWD as previous
                PREV_BUF[..CWD_LEN].copy_from_slice(&CWD_BUF[..CWD_LEN]);
                PREV_LEN = CWD_LEN;
                // Update CWD
                let bytes = resolved.as_bytes();
                let len = bytes.len().min(MAX_PATH);
                CWD_BUF[..len].copy_from_slice(&bytes[..len]);
                CWD_LEN = len;
            }
        }
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(DualWriter, "cd: {}: no such directory\n", target);
        }
        Some(code) if code == FsError::NotDirectory as usize => {
            let _ = write!(DualWriter, "cd: {}: not a directory\n", target);
        }
        Some(code) if code == FsError::InvalidPath as usize => {
            let _ = write!(DualWriter, "cd: {}: invalid path\n", target);
        }
        None => puts("cd: fs service not available\n"),
        _ => puts("cd: error\n"),
    }
}

// ============================================================================
// FS operations (BlockingScalar IPC to fs service)
// ============================================================================

static mut FS_CID: u32 = 0;

fn get_fs_cid() -> u32 {
    unsafe {
        if FS_CID != 0 { return FS_CID; }
        let sid = xous::SID::from_array(FS_SID);
        match xous::rsyscall(xous::SysCall::Connect(sid)) {
            Ok(xous::Result::ConnectionID(cid)) => { FS_CID = cid; cid }
            _ => 0,
        }
    }
}

/// Send a MutableBorrow to the FS service. Writes output to `puts()` on success.
/// Returns `Some(FsError code)` or `None` if the service is unavailable.
fn fs_buf_op(op: FsOp, path: &str) -> Option<usize> {
    let cid = get_fs_cid();
    if cid == 0 { return None; }

    let page_size = xous::MemorySize::new(beetos::PAGE_SIZE)?;
    let buf_range = match xous::rsyscall(xous::SysCall::MapMemory(
        None, None, page_size, xous::MemoryFlags::W,
    )) {
        Ok(xous::Result::MemoryRange(r)) => r,
        _ => return None,
    };

    unsafe { core::ptr::write_bytes(buf_range.as_mut_ptr(), 0, buf_range.len()); }

    let page_slice = unsafe { core::slice::from_raw_parts_mut(buf_range.as_mut_ptr(), buf_range.len()) };
    let path_bytes = path.as_bytes();
    let path_len = path_bytes.len().min(beetos_api_fs::MAX_PATH_LEN);
    page_slice[..path_len].copy_from_slice(&path_bytes[..path_len]);

    let result = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::MutableBorrow(xous::MemoryMessage {
            id: op as usize,
            buf: buf_range,
            offset: None,
            valid: None,
        }),
    ));

    let status = match result {
        Ok(xous::Result::MemoryReturned(_, _)) | Ok(xous::Result::Ok) => {
            let page_slice = unsafe { core::slice::from_raw_parts(buf_range.as_ptr(), buf_range.len()) };
            let code = page_slice[BUF_STATUS_OFFSET] as usize;
            if code == FsError::Ok as usize {
                let text = &page_slice[BUF_TEXT_OFFSET..];
                let len = text.iter().position(|&b| b == 0).unwrap_or(text.len());
                if let Ok(s) = core::str::from_utf8(&text[..len]) {
                    puts(s);
                }
            }
            Some(code)
        }
        _ => None,
    };

    xous::rsyscall(xous::SysCall::UnmapMemory(buf_range)).ok();
    status
}

/// Send a BlockingScalar to the FS service and return the result code.
fn fs_scalar(op: FsOp, arg1: usize, arg2: usize, arg3: usize, arg4: usize) -> Option<usize> {
    let cid = get_fs_cid();
    if cid == 0 { return None; }
    match xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: op as usize, arg1, arg2, arg3, arg4,
        }),
    )) {
        Ok(xous::Result::Scalar1(code)) => Some(code),
        _ => None,
    }
}

/// Query filesystem statistics, returning (ram_used, ram_total, ram_bytes, disk_size, disk_files).
fn fs_stats() -> Option<(usize, usize, usize, usize, usize)> {
    let cid = get_fs_cid();
    if cid == 0 { return None; }
    match xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: FsOp::Stats as usize, arg1: 0, arg2: 0, arg3: 0, arg4: 0,
        }),
    )) {
        Ok(xous::Result::Scalar5(a, b, c, d, e)) => Some((a, b, c, d, e)),
        _ => None,
    }
}

fn cmd_ls(args: &[&str]) {
    let mut buf = [0u8; MAX_PATH];
    let path = if args.is_empty() {
        resolve_path(".", &mut buf)
    } else {
        resolve_path(args[0], &mut buf)
    };
    match fs_buf_op(FsOp::LsBuf, path) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(DualWriter, "ls: {}: not found\n", path);
        }
        Some(code) if code == FsError::Locked as usize => {
            let _ = write!(DualWriter, "ls: {}: locked (cryptopen first)\n", path);
        }
        Some(code) if code == FsError::NotDirectory as usize => {
            let _ = write!(DualWriter, "ls: {}: not a directory\n", path);
        }
        Some(code) if code == FsError::InvalidPath as usize => {
            let _ = write!(DualWriter, "ls: {}: invalid path\n", path);
        }
        None => puts("ls: fs service not available\n"),
        _ => puts("ls: error\n"),
    }
}

fn cmd_cat(args: &[&str]) {
    if args.is_empty() { puts("usage: cat <path>\n"); return; }
    let mut buf = [0u8; MAX_PATH];
    let path = resolve_path(args[0], &mut buf);
    match fs_buf_op(FsOp::CatBuf, path) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(DualWriter, "cat: {}: not found\n", path);
        }
        Some(code) if code == FsError::IsDirectory as usize => {
            let _ = write!(DualWriter, "cat: {}: is a directory\n", path);
        }
        Some(code) if code == FsError::InvalidPath as usize => {
            let _ = write!(DualWriter, "cat: {}: invalid path\n", path);
        }
        Some(code) if code == FsError::Locked as usize => {
            let _ = write!(DualWriter, "cat: {}: locked (cryptopen first)\n", path);
        }
        Some(code) if code == FsError::Corrupt as usize => {
            let _ = write!(DualWriter, "cat: {}: CORRUPT (auth failed)\n", path);
        }
        None => puts("cat: fs service not available\n"),
        _ => puts("cat: error\n"),
    }
}

fn cmd_write(args: &[&str], full_line: &str) {
    if args.len() < 2 { puts("usage: write <path> <text>\n"); return; }
    let mut path_buf = [0u8; MAX_PATH];
    let path = resolve_path(args[0], &mut path_buf);
    let content = if let Some(pos) = full_line.find(path) {
        let after_path = pos + path.len();
        full_line[after_path..].trim_start()
    } else {
        args[1]
    };

    // Pack path into arg1-arg2 (16 bytes max) and content into arg3-arg4 (16 bytes max)
    let path_packed = beetos_api_fs::pack_path(path);
    let content_bytes = content.as_bytes();
    let ws = core::mem::size_of::<usize>();
    let mut c_args = [0usize; 2];
    for (i, chunk) in content_bytes.chunks(ws).enumerate() {
        if i >= 2 { break; }
        let mut buf = [0u8; core::mem::size_of::<usize>()];
        buf[..chunk.len()].copy_from_slice(chunk);
        c_args[i] = usize::from_le_bytes(buf);
    }

    match fs_scalar(FsOp::WriteShort, path_packed[0], path_packed[1], c_args[0], c_args[1]) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::ReadOnly as usize => {
            let _ = write!(DualWriter, "write: {}: read-only\n", path);
        }
        Some(code) if code == FsError::IsDirectory as usize => {
            let _ = write!(DualWriter, "write: {}: is a directory\n", path);
        }
        Some(code) if code == FsError::Locked as usize => {
            let _ = write!(DualWriter, "write: {}: locked (cryptopen first)\n", path);
        }
        Some(code) if code == FsError::NoSpace as usize => {
            puts("write: no space left\n");
        }
        Some(code) if code == FsError::InvalidPath as usize => {
            let _ = write!(DualWriter, "write: {}: invalid path\n", path);
        }
        None => puts("write: fs service not available\n"),
        _ => puts("write: error\n"),
    }
}

fn cmd_mkdir(args: &[&str]) {
    if args.is_empty() { puts("usage: mkdir <path>\n"); return; }
    let mut buf = [0u8; MAX_PATH];
    let path = resolve_path(args[0], &mut buf);
    let packed = beetos_api_fs::pack_path(path);
    match fs_scalar(FsOp::Mkdir, packed[0], packed[1], packed[2], packed[3]) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::AlreadyExists as usize => {
            let _ = write!(DualWriter, "mkdir: {}: already exists\n", args[0]);
        }
        Some(code) if code == FsError::ReadOnly as usize => {
            let _ = write!(DualWriter, "mkdir: {}: read-only\n", args[0]);
        }
        Some(code) if code == FsError::NoSpace as usize => {
            puts("mkdir: no space left\n");
        }
        Some(code) if code == FsError::InvalidPath as usize => {
            let _ = write!(DualWriter, "mkdir: {}: invalid path\n", args[0]);
        }
        None => puts("mkdir: fs service not available\n"),
        _ => puts("mkdir: error\n"),
    }
}

fn cmd_rm(args: &[&str]) {
    if args.is_empty() { puts("usage: rm <path>\n"); return; }
    let mut buf = [0u8; MAX_PATH];
    let path = resolve_path(args[0], &mut buf);
    let packed = beetos_api_fs::pack_path(path);
    match fs_scalar(FsOp::Remove, packed[0], packed[1], packed[2], packed[3]) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(DualWriter, "rm: {}: not found\n", args[0]);
        }
        Some(code) if code == FsError::NotEmpty as usize => {
            let _ = write!(DualWriter, "rm: {}: directory not empty\n", args[0]);
        }
        Some(code) if code == FsError::Locked as usize => {
            let _ = write!(DualWriter, "rm: {}: locked (cryptopen first)\n", args[0]);
        }
        Some(code) if code == FsError::ReadOnly as usize => {
            let _ = write!(DualWriter, "rm: {}: read-only\n", args[0]);
        }
        Some(code) if code == FsError::InvalidPath as usize => {
            let _ = write!(DualWriter, "rm: {}: invalid path\n", args[0]);
        }
        None => puts("rm: fs service not available\n"),
        _ => puts("rm: error\n"),
    }
}

fn cmd_blkinfo() {
    match fs_stats() {
        Some((_, _, _, disk_size, disk_files)) => {
            if disk_size == 0 {
                puts("No block device\n");
            } else {
                let _ = write!(DualWriter, "Block device: {} bytes\n", disk_size);
                puts("Mounted at: /disk/ (read-only, tar)\n");
                let _ = write!(DualWriter, "Files: {}\n", disk_files);
            }
        }
        None => puts("blkinfo: fs service not available\n"),
    }
}

/// Allocate a one-page buffer and acquire a block-service client.
/// Returns `None` and prints an error if either step fails.
fn open_block(buf_label: &str) -> Option<(beetos_api_block::BlockClient, xous::MemoryRange)> {
    let client = match beetos_api_block::BlockClient::connect_with_retries(8) {
        Ok(c) => c,
        Err(_) => {
            let _ = write!(DualWriter, "{}: block service unavailable\n", buf_label);
            return None;
        }
    };
    let page = match xous::MemorySize::new(beetos::PAGE_SIZE) {
        Some(s) => s,
        None => {
            let _ = write!(DualWriter, "{}: bad page size\n", buf_label);
            return None;
        }
    };
    let buf = match xous::rsyscall(xous::SysCall::MapMemory(
        None, None, page, xous::MemoryFlags::W,
    )) {
        Ok(xous::Result::MemoryRange(r)) => r,
        _ => {
            let _ = write!(DualWriter, "{}: alloc FAILED\n", buf_label);
            return None;
        }
    };
    Some((client, buf))
}

fn cmd_dread(args: &[&str]) {
    let lba = match args.first().and_then(|s| parse_u64(s)) {
        Some(v) => v,
        None => { puts("usage: dread <lba>\n"); return; }
    };
    let Some((client, buf)) = open_block("dread") else { return };

    let res = client.read_blocks(lba, 1, buf);
    if let Err(e) = res {
        let _ = write!(DualWriter, "dread: read FAILED ({:?})\n", e);
        xous::rsyscall(xous::SysCall::UnmapMemory(buf)).ok();
        return;
    }
    let slice = unsafe {
        core::slice::from_raw_parts(buf.as_ptr(), buf.len())
    };
    let data = beetos_api_block::data(slice);
    // 512 bytes is enough to dominate the line; just print up to the
    // first NUL or 64 chars, whichever comes first, then a printable
    // count summary.
    let stop = data.iter().position(|&b| b == 0).unwrap_or(data.len()).min(64);
    let _ = write!(DualWriter, "lba {}: ", lba);
    for &b in &data[..stop] {
        if (0x20..=0x7e).contains(&b) {
            putc(b);
        } else {
            putc(b'.');
        }
    }
    putc(b'\n');
    let nz = data.iter().filter(|&&b| b != 0).count();
    let _ = write!(DualWriter, "  {} non-zero byte(s) in block\n", nz);
    xous::rsyscall(xous::SysCall::UnmapMemory(buf)).ok();
}

fn cmd_dwrite(args: &[&str], line: &str) {
    let lba = match args.first().and_then(|s| parse_u64(s)) {
        Some(v) => v,
        None => { puts("usage: dwrite <lba> <text>\n"); return; }
    };
    // Re-extract the payload from the original command line so embedded
    // spaces survive (the splitter would shred them).
    let mut tokens = line.split_whitespace();
    tokens.next(); // dwrite
    tokens.next(); // lba
    let payload = match tokens.next() {
        Some(p) => p,
        None => { puts("usage: dwrite <lba> <text>\n"); return; }
    };

    let Some((client, buf)) = open_block("dwrite") else { return };

    let slice = unsafe {
        core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len())
    };
    let data = beetos_api_block::data_mut(slice);
    // Zero-pad so the on-disk block is deterministic — any host-side
    // verifier sees exactly what was written, no stale tail bytes.
    for b in data.iter_mut() { *b = 0; }
    let n = payload.len().min(data.len());
    data[..n].copy_from_slice(&payload.as_bytes()[..n]);

    match client.write_blocks(lba, 1, buf) {
        Ok(()) => {
            let _ = write!(DualWriter, "dwrite: wrote {} byte(s) at lba {}\n", n, lba);
        }
        Err(e) => {
            let _ = write!(DualWriter, "dwrite: FAILED ({:?})\n", e);
        }
    }
    xous::rsyscall(xous::SysCall::UnmapMemory(buf)).ok();
}

// ============================================================================
// Encrypted /data area (owned by the fs service; we just send ops)
// ============================================================================

fn cmd_cryptfmt(args: &[&str]) {
    let pass = match args.first() {
        Some(p) if !p.is_empty() => *p,
        _ => { puts("usage: cryptfmt <passphrase>\n"); return; }
    };
    // The buffer op's "path" field carries the passphrase.
    match fs_buf_op(FsOp::CryptFormat, pass) {
        Some(code) if code == FsError::Ok as usize => {
            puts("cryptfmt: /data formatted and unlocked\n");
        }
        Some(code) => { let _ = write!(DualWriter, "cryptfmt: error {}\n", code); }
        None => puts("cryptfmt: fs service not available\n"),
    }
}

fn cmd_cryptopen(args: &[&str]) {
    let pass = match args.first() {
        Some(p) if !p.is_empty() => *p,
        _ => { puts("usage: cryptopen <passphrase>\n"); return; }
    };
    match fs_buf_op(FsOp::CryptOpen, pass) {
        Some(code) if code == FsError::Ok as usize => {
            puts("cryptopen: /data unlocked\n");
        }
        Some(code) if code == FsError::Locked as usize => {
            puts("cryptopen: bad passphrase\n");
        }
        Some(code) if code == FsError::NotFound as usize => {
            puts("cryptopen: /data not formatted (cryptfmt first)\n");
        }
        Some(code) => { let _ = write!(DualWriter, "cryptopen: error {}\n", code); }
        None => puts("cryptopen: fs service not available\n"),
    }
}

fn cmd_cryptlock() {
    match fs_scalar(FsOp::CryptLock, 0, 0, 0, 0) {
        Some(code) if code == FsError::Ok as usize => puts("cryptlock: /data locked\n"),
        _ => puts("cryptlock: failed\n"),
    }
}

fn cmd_ifconfig() {
    match xous::rsyscall(xous::SysCall::NetGetInfo) {
        Ok(xous::Result::Scalar5(ip_u32, mac_hi, mac_lo, _, _)) => {
            let ip = [
                ((ip_u32 >> 24) & 0xFF) as u8,
                ((ip_u32 >> 16) & 0xFF) as u8,
                ((ip_u32 >> 8) & 0xFF) as u8,
                (ip_u32 & 0xFF) as u8,
            ];
            let mac = [
                ((mac_hi >> 24) & 0xFF) as u8,
                ((mac_hi >> 16) & 0xFF) as u8,
                ((mac_hi >> 8) & 0xFF) as u8,
                (mac_hi & 0xFF) as u8,
                ((mac_lo >> 24) & 0xFF) as u8,
                ((mac_lo >> 16) & 0xFF) as u8,
            ];
            let _ = write!(
                DualWriter,
                "eth0: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
            );
            if ip == [0, 0, 0, 0] {
                puts("      inet: (no address — DHCP pending)\n");
            } else {
                let _ = write!(
                    DualWriter,
                    "      inet: {}.{}.{}.{}\n",
                    ip[0], ip[1], ip[2], ip[3],
                );
            }
        }
        _ => puts("ifconfig: NetGetInfo syscall failed\n"),
    }
}

fn parse_u64(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for b in s.bytes() {
        if !(b'0'..=b'9').contains(&b) {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    }
    Some(n)
}

fn parse_u16(s: &str) -> Option<u16> {
    let mut n: u32 = 0;
    if s.is_empty() {
        return None;
    }
    for b in s.bytes() {
        if !(b'0'..=b'9').contains(&b) {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
        if n > 65535 {
            return None;
        }
    }
    Some(n as u16)
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in out.iter_mut() {
        let part = parts.next()?;
        let mut n: u32 = 0;
        if part.is_empty() {
            return None;
        }
        for b in part.bytes() {
            if !(b'0'..=b'9').contains(&b) {
                return None;
            }
            n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
            if n > 255 {
                return None;
            }
        }
        *slot = n as u8;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

/// Kernel uptime in 100 Hz ticks (rides in NetGetInfo's 4th scalar).
/// The only wall-clock the shell has — yield-spin counts run at
/// CPU speed (hundreds of thousands per second on an idle system)
/// and are useless as a time base.
fn now_ticks() -> u64 {
    match xous::rsyscall(xous::SysCall::NetGetInfo) {
        Ok(xous::Result::Scalar5(_, _, _, ticks, _)) => ticks as u64,
        _ => 0,
    }
}

fn cmd_ping(args: &[&str]) {
    use xous::arch::perf;

    let ip = match args.first().and_then(|s| parse_ipv4(s)) {
        Some(ip) => ip,
        None => {
            puts("usage: ping <ip> [count]\n");
            return;
        }
    };
    let count = args
        .get(1)
        .and_then(|s| parse_u16(s))
        .map(|n| n as u64)
        .unwrap_or(4)
        .max(1);

    let freq = perf::frequency().max(1);
    let mut received = 0u64;

    for seq in 0..count {
        // Send — the kernel returns 0 while it resolves the next hop's
        // MAC (ARP), so retry against a real 1 s deadline.
        let send_deadline = now_ticks() + 100;
        let sent_at = loop {
            match xous::rsyscall(xous::SysCall::NetPingSend(
                u32::from_be_bytes(ip) as usize,
                seq as usize,
            )) {
                Ok(xous::Result::Scalar1(1)) => break Some(perf::counter()),
                Ok(xous::Result::Scalar1(_)) => {} // ARP pending
                _ => break None,                   // syscall refused (no platform?)
            }
            if now_ticks() > send_deadline {
                break None;
            }
            xous::yield_slice();
        };
        let Some(sent_at) = sent_at else {
            let _ = write!(DualWriter, "ping: seq={} send failed (no route)\n", seq);
            continue;
        };

        // Wait up to 1 s for the matching reply.
        let reply_deadline = now_ticks() + 100;
        let mut got = false;
        loop {
            if let Ok(xous::Result::Scalar2(src, rseq)) =
                xous::rsyscall(xous::SysCall::NetPingPoll)
            {
                if src != 0 && rseq == seq as usize {
                    let dt = perf::counter().wrapping_sub(sent_at);
                    let us = (dt as u128 * 1_000_000 / freq as u128) as u64;
                    let s = (src as u32).to_be_bytes();
                    let _ = write!(
                        DualWriter,
                        "reply from {}.{}.{}.{}: seq={} time={} us\n",
                        s[0], s[1], s[2], s[3], seq, us,
                    );
                    received += 1;
                    got = true;
                    break;
                }
            }
            if now_ticks() > reply_deadline {
                break;
            }
            xous::yield_slice();
        }
        if !got {
            let _ = write!(DualWriter, "ping: seq={} timeout\n", seq);
        }
    }

    let _ = write!(DualWriter, "ping: {} sent, {} received\n", count, received);
}

fn cmd_nettest_listen(args: &[&str]) {
    use beetos_api_net::{SockState, TcpListener};

    let port = match args.first().and_then(|s| parse_u16(s)) {
        Some(p) if p != 0 && p != 2323 => p,
        _ => {
            puts("usage: nettest-listen <port>  (1..65535, not 2323)\n");
            return;
        }
    };

    let listener = match TcpListener::bind(port) {
        Some(l) => l,
        None => {
            puts("nettest-listen: bind failed\n");
            return;
        }
    };

    let _ = write!(DualWriter, "nettest: listening on port {}\n", port);

    // Poll try_accept until a client shows up (the kernel answers the
    // SYN on its own) or a real 30 s deadline passes.
    let deadline = now_ticks() + 3_000; // 30 s at 100 Hz
    let mut stream = loop {
        if let Some(s) = listener.try_accept() {
            break s;
        }
        if now_ticks() > deadline {
            puts("nettest-listen: timeout waiting for client\n");
            return;
        }
        xous::yield_slice();
    };

    puts("nettest: client connected, echoing...\n");

    let mut buf = [0u8; 32];
    let mut idle_deadline = now_ticks() + 1_000; // 10 s of client silence
    loop {
        let n = stream.recv(&mut buf);
        if n > 0 {
            stream.send_all(&buf[..n]);
            idle_deadline = now_ticks() + 1_000;
        } else {
            // Nothing buffered. If the peer already sent FIN
            // (CloseWait) — or the connection died — we're done as
            // soon as the ring is drained; only a live connection
            // waits out the idle timeout.
            if !matches!(stream.state(), SockState::Established) {
                break;
            }
            if now_ticks() > idle_deadline {
                break;
            }
            xous::yield_slice();
        }
        match stream.state() {
            SockState::Established | SockState::CloseWait => {}
            _ => break,
        }
    }

    puts("nettest: client done, closing\n");
    stream.close();
}

fn cmd_nettest_connect(args: &[&str]) {
    use beetos_api_net::{SockState, TcpStream};

    let (ip, port) = match (
        args.first().and_then(|s| parse_ipv4(s)),
        args.get(1).and_then(|s| parse_u16(s)),
    ) {
        (Some(ip), Some(port)) if port != 0 => (ip, port),
        _ => {
            puts("usage: nettest-connect <ip> <port>\n");
            return;
        }
    };

    let _ = write!(
        DualWriter,
        "nettest: connecting to {}.{}.{}.{}:{}...\n",
        ip[0], ip[1], ip[2], ip[3], port,
    );

    let mut stream = match TcpStream::connect(ip, port) {
        Some(s) => s,
        None => {
            puts("nettest-connect: connect failed\n");
            return;
        }
    };

    // Wait for the handshake (ARP request + SYN/SYN-ACK/ACK) with a
    // real 10 s deadline.
    let deadline = now_ticks() + 1_000;
    loop {
        match stream.state() {
            SockState::Established => break,
            SockState::SynSent => {}
            other => {
                let _ = write!(DualWriter, "nettest-connect: unexpected state {:?}\n", other);
                return;
            }
        }
        if now_ticks() > deadline {
            puts("nettest-connect: timeout waiting for Established\n");
            return;
        }
        xous::yield_slice();
    }
    puts("nettest: connected\n");

    let sent = stream.send_all(b"hi from BeetOS\n");
    let _ = write!(DualWriter, "nettest: sent {} bytes\n", sent);

    let mut buf = [0u8; 32];
    let mut idle_deadline = now_ticks() + 1_000; // 10 s without a reply
    let mut got_any = false;
    loop {
        let n = stream.recv(&mut buf);
        if n > 0 {
            puts("nettest: recv: ");
            for &b in &buf[..n] {
                if (0x20..=0x7e).contains(&b) || b == b'\n' {
                    putc(b);
                } else {
                    putc(b'.');
                }
            }
            if buf[n - 1] != b'\n' {
                putc(b'\n');
            }
            got_any = true;
            idle_deadline = now_ticks() + 1_000;
        } else {
            if now_ticks() > idle_deadline {
                break;
            }
            xous::yield_slice();
        }
        if got_any && !matches!(stream.state(), SockState::Established) {
            break;
        }
    }
    puts("nettest: done\n");
    stream.close();
}

// ============================================================================
// Kernel micro-benchmarks
// ============================================================================

/// Print one benchmark result. Format is parsed by `cargo xtask
/// qemu-bench` — keep `[bench] <name> <ns> ns/op n=<iters>` stable.
fn bench_report(name: &str, total_ticks: u64, freq: u64, iters: u64) {
    let ns = (total_ticks as u128)
        .saturating_mul(1_000_000_000)
        / (freq as u128).max(1)
        / (iters as u128).max(1);
    let _ = write!(DualWriter, "[bench] {} {} ns/op n={}\n", name, ns as u64, iters);
}

/// Micro-benchmarks for the kernel hot paths, timed with the virtual
/// counter (CNTVCT_EL0, EL0-readable thanks to CNTKCTL_EL1.EL0VCTEN).
/// Under `cargo xtask qemu-bench` QEMU runs with `-icount`, making the
/// counter advance with the instruction count — results are then
/// deterministic and comparable against a checked-in baseline.
fn cmd_bench() {
    use core::hint::black_box;
    use xous::arch::perf;

    let freq = perf::frequency();
    if freq == 0 {
        puts("bench: cycle counter unavailable\n");
        return;
    }
    let _ = write!(DualWriter, "[bench] counter {} Hz\n", freq);

    // Pure-CPU integer mix. Calibration point: insensitive to kernel
    // changes, so a shift here means the *measurement environment*
    // moved (QEMU version, icount config), not the kernel.
    {
        const N: u64 = 200;
        let t0 = perf::counter();
        let mut acc = 0u64;
        for i in 0..N {
            let mut x = i.wrapping_add(0x9E37_79B9);
            for j in 0..2048u64 {
                x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(13) ^ j;
            }
            acc = acc.wrapping_add(x);
        }
        let t1 = perf::counter();
        black_box(acc);
        bench_report("cpu_mix", t1.wrapping_sub(t0), freq, N);
    }

    // Cheapest possible syscall: EL0→EL1 trap, dispatch, return.
    // Watches the exception vectors + context save/restore.
    {
        const N: u64 = 10_000;
        let t0 = perf::counter();
        for _ in 0..N {
            let _ = black_box(xous::rsyscall(xous::SysCall::GetThreadId));
        }
        let t1 = perf::counter();
        bench_report("syscall_null", t1.wrapping_sub(t0), freq, N);
    }

    // Scheduler round-trip (immediate return when nothing else is
    // runnable — measures the pick-next path, not a real switch).
    {
        const N: u64 = 10_000;
        let t0 = perf::counter();
        for _ in 0..N {
            xous::yield_slice();
        }
        let t1 = perf::counter();
        bench_report("yield", t1.wrapping_sub(t0), freq, N);
    }

    // Full IPC round-trip: BlockingScalar to the fs service — sender
    // blocks, server wakes, replies, sender wakes. Two context
    // switches plus the message plumbing.
    {
        const N: u64 = 1_000;
        if fs_stats().is_some() {
            let t0 = perf::counter();
            for _ in 0..N {
                black_box(fs_stats());
            }
            let t1 = perf::counter();
            bench_report("ipc_scalar", t1.wrapping_sub(t0), freq, N);
        } else {
            puts("[bench] ipc_scalar skipped (fs unavailable)\n");
        }
    }

    // Map one page, touch it, unmap it: page allocator, page tables,
    // TLB maintenance.
    {
        const N: u64 = 1_000;
        let mut ok = true;
        let t0 = perf::counter();
        match xous::MemorySize::new(beetos::PAGE_SIZE) {
            Some(page_size) => {
                for _ in 0..N {
                    match xous::rsyscall(xous::SysCall::MapMemory(
                        None, None, page_size, xous::MemoryFlags::W,
                    )) {
                        Ok(xous::Result::MemoryRange(r)) => {
                            // Touch so the mapping is realised even if
                            // the kernel ever goes demand-paged.
                            unsafe { core::ptr::write_volatile(r.as_mut_ptr(), 0xA5u8) };
                            let _ = xous::rsyscall(xous::SysCall::UnmapMemory(r));
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            None => ok = false,
        }
        let t1 = perf::counter();
        if ok {
            bench_report("map_unmap", t1.wrapping_sub(t0), freq, N);
        } else {
            puts("[bench] map_unmap skipped (MapMemory failed)\n");
        }
    }

    // Copy one 16 KiB page between two kernel-allocated pages —
    // memory subsystem sanity (and a second calibration point).
    {
        const N: u64 = 500;
        let page_size = match xous::MemorySize::new(beetos::PAGE_SIZE) {
            Some(s) => s,
            None => {
                puts("[bench] memcpy_16k skipped\n");
                puts("[bench] done\n");
                return;
            }
        };
        let map = |_: ()| -> Option<xous::MemoryRange> {
            match xous::rsyscall(xous::SysCall::MapMemory(
                None, None, page_size, xous::MemoryFlags::W,
            )) {
                Ok(xous::Result::MemoryRange(r)) => Some(r),
                _ => None,
            }
        };
        match (map(()), map(())) {
            (Some(src), Some(dst)) => {
                unsafe { core::ptr::write_bytes(src.as_mut_ptr(), 0x5A, src.len()) };
                let t0 = perf::counter();
                for _ in 0..N {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            src.as_ptr(),
                            dst.as_mut_ptr(),
                            beetos::PAGE_SIZE,
                        );
                    }
                    black_box(unsafe { core::ptr::read_volatile(dst.as_ptr()) });
                }
                let t1 = perf::counter();
                bench_report("memcpy_16k", t1.wrapping_sub(t0), freq, N);
                let _ = xous::rsyscall(xous::SysCall::UnmapMemory(src));
                let _ = xous::rsyscall(xous::SysCall::UnmapMemory(dst));
            }
            (s, d) => {
                if let Some(r) = s {
                    let _ = xous::rsyscall(xous::SysCall::UnmapMemory(r));
                }
                if let Some(r) = d {
                    let _ = xous::rsyscall(xous::SysCall::UnmapMemory(r));
                }
                puts("[bench] memcpy_16k skipped (MapMemory failed)\n");
            }
        }
    }

    puts("[bench] done\n");
}

fn cmd_mem() {
    match fs_stats() {
        Some((used, total, bytes, disk_size, _)) => {
            puts("RAM filesystem:\n");
            let _ = write!(DualWriter, "  Files: {}/{}\n", used, total);
            let _ = write!(DualWriter, "  Used:  {} bytes\n", bytes);
            if disk_size > 0 {
                let _ = write!(DualWriter, "Disk: {} bytes\n", disk_size);
            }
        }
        None => puts("mem: fs service not available\n"),
    }
}

// ============================================================================
// Process spawning via procman
// ============================================================================

static mut PROCMAN_CID: u32 = 0;

fn get_procman_cid() -> u32 {
    unsafe {
        if PROCMAN_CID != 0 { return PROCMAN_CID; }
        let sid = xous::SID::from_array(beetos_api_procman::PROCMAN_SID);
        match xous::rsyscall(xous::SysCall::Connect(sid)) {
            Ok(xous::Result::ConnectionID(cid)) => { PROCMAN_CID = cid; cid }
            _ => 0,
        }
    }
}

fn try_spawn_via_procman(cmd: &str, args: &[&str]) {
    let cid = get_procman_cid();
    if cid == 0 {
        let _ = write!(DualWriter, "bsh: {}: procman not available\n", cmd);
        return;
    }

    if args.is_empty() {
        let name_packed = beetos_api_procman::pack_name(cmd);

        // Release display and input focus before blocking on procman — the
        // spawned process needs to acquire both.  Deadlock otherwise.
        let (row, col) = fb_cursor();
        release_display(row, col);
        release_input_focus();

        let result = xous::rsyscall(xous::SysCall::SendMessage(
            cid,
            xous::Message::BlockingScalar(xous::ScalarMessage {
                id: beetos_api_procman::ProcManOp::SpawnAndWait as usize,
                arg1: name_packed[0], arg2: name_packed[1],
                arg3: name_packed[2], arg4: name_packed[3],
            }),
        ));

        // Re-acquire display after the child ran; cursor is now updated.
        let (row, col) = acquire_display();
        acquire_input_focus();
        unsafe {
            if let Some(ref mut con) = FB_CONSOLE {
                con.set_cursor(row, col);
            }
        }

        match result {
            Ok(xous::Result::Scalar1(exit_code)) | Ok(xous::Result::Scalar2(exit_code, _)) => {
                if exit_code == usize::MAX {
                    let _ = write!(DualWriter, "bsh: {}: not found\n", cmd);
                } else if exit_code != 0 {
                    let _ = write!(DualWriter, "[exited: {}]\n", exit_code);
                }
            }
            Err(_) => { let _ = write!(DualWriter, "bsh: {}: spawn failed\n", cmd); }
            _ => { let _ = write!(DualWriter, "bsh: {}: unexpected result\n", cmd); }
        }
    } else {
        // Has args — allocate a page and send via MutableBorrow
        let page_size = xous::MemorySize::new(beetos::PAGE_SIZE);
        let page = if let Some(size) = page_size {
            xous::rsyscall(xous::SysCall::MapMemory(
                None, None, size, xous::MemoryFlags::W,
            ))
        } else {
            let _ = write!(DualWriter, "bsh: {}: internal error\n", cmd);
            return;
        };

        let buf = match page {
            Ok(xous::Result::MemoryRange(range)) => range,
            _ => {
                let _ = write!(DualWriter, "bsh: {}: out of memory\n", cmd);
                return;
            }
        };

        // Format the command line into the page: "name\0arg1\0arg2\0..."
        let page_slice = unsafe {
            core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len())
        };
        let valid_len = beetos_api_procman::format_cmdline(page_slice, cmd, args);

        let valid = xous::MemorySize::new(valid_len);

        // Release display and input focus before blocking on procman.
        let (row, col) = fb_cursor();
        release_display(row, col);
        release_input_focus();

        let result = xous::rsyscall(xous::SysCall::SendMessage(
            cid,
            xous::Message::MutableBorrow(xous::MemoryMessage {
                id: beetos_api_procman::ProcManOp::SpawnAndWaitWithArgs as usize,
                buf,
                offset: None,
                valid,
            }),
        ));

        // Re-acquire display after the child ran.
        let (row, col) = acquire_display();
        acquire_input_focus();
        unsafe {
            if let Some(ref mut con) = FB_CONSOLE {
                con.set_cursor(row, col);
            }
        }

        // Read exit code from the returned buffer (first usize)
        match result {
            Ok(xous::Result::MemoryReturned(_, _)) | Ok(xous::Result::Ok) => {
                let exit_code = usize::from_le_bytes({
                    let mut b = [0u8; core::mem::size_of::<usize>()];
                    let slice = unsafe { core::slice::from_raw_parts(buf.as_ptr(), b.len()) };
                    b.copy_from_slice(slice);
                    b
                });
                if exit_code == usize::MAX {
                    let _ = write!(DualWriter, "bsh: {}: not found\n", cmd);
                } else if exit_code != 0 {
                    let _ = write!(DualWriter, "[exited: {}]\n", exit_code);
                }
            }
            Err(_) => { let _ = write!(DualWriter, "bsh: {}: spawn failed\n", cmd); }
            _ => { let _ = write!(DualWriter, "bsh: {}: unexpected result\n", cmd); }
        }

        // Free the page
        xous::rsyscall(xous::SysCall::UnmapMemory(buf)).ok();
    }
}

// ============================================================================
// Boot-time block-service self-test
// ============================================================================
//
// Runs once during shell startup, after the banner is printed but before
// the prompt. Verifies the kernel → block-service → IPC client datapath
// by connecting to `BLOCK_SID`, asking for geometry, reading LBA 0, and
// checking the on-disk tar archive's `ustar` magic at offset 257.
//
// This is the first real IPC consumer of `os/block`; once the FS service
// is migrated off direct disk-mapping (next commit) the same code path
// will be exercised on every file read instead.

/// Runs the self-test and prints exactly one line summarising the result.
/// Never panics — IPC failure just emits a "FAILED" line and continues
/// the shell's normal boot.
fn block_selftest() {
    use beetos_api_block::{BlockClient, ClientError};

    let client = match BlockClient::connect_with_retries(32) {
        Ok(c) => c,
        Err(_) => { puts("[shell] block self-test: connect FAILED\n"); return; }
    };

    let info = match client.info() {
        Ok(i) => i,
        Err(_) => { puts("[shell] block self-test: info FAILED\n"); return; }
    };

    // No backing device — the block service is up but has nothing to
    // serve (QEMU launched without `-drive`, or platform without disk).
    // That's not an error; just don't pretend we can read LBA 0.
    if info.capacity_blocks == 0 {
        puts("[shell] block self-test: no disk attached (skipped)\n");
        return;
    }

    // A page is plenty for the one-block read (header + 512 bytes).
    let page_size = match xous::MemorySize::new(beetos::PAGE_SIZE) {
        Some(s) => s,
        None => { puts("[shell] block self-test: bad page size\n"); return; }
    };
    let buf = match xous::rsyscall(xous::SysCall::MapMemory(
        None, None, page_size, xous::MemoryFlags::W,
    )) {
        Ok(xous::Result::MemoryRange(r)) => r,
        _ => { puts("[shell] block self-test: alloc FAILED\n"); return; }
    };

    let result = client.read_blocks(0, 1, buf);

    let slice = unsafe { core::slice::from_raw_parts(buf.as_ptr(), buf.len()) };
    let data = beetos_api_block::data(slice);

    match result {
        Ok(()) => {
            // Standard POSIX ustar header: magic at bytes 257..262.
            if data.len() >= 263 && &data[257..262] == b"ustar" {
                let _ = write!(
                    DualWriter,
                    "[shell] block self-test: OK ({} B blocks, {} total, ustar @ 257)\n",
                    info.block_size, info.capacity_blocks,
                );
            } else {
                puts("[shell] block self-test: read OK but no ustar magic\n");
            }
        }
        Err(ClientError::Block(status)) => {
            let _ = write!(
                DualWriter,
                "[shell] block self-test: read FAILED (status={:?})\n",
                status,
            );
        }
        Err(_) => puts("[shell] block self-test: read FAILED\n"),
    }

    xous::rsyscall(xous::SysCall::UnmapMemory(buf)).ok();
}

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start(uart_base: usize) -> ! {
    beetos::pl011::init(uart_base);
    unsafe {
        CWD_BUF[0] = b'/';
        CWD_LEN = 1;
        // FB is mapped at a fixed VA by AcquireDisplay — use it directly.
        FB_CONSOLE = Some(FbConsole::new(
            beetos::SHELL_FB_VA as *mut u32,
            beetos::FB_WIDTH, beetos::FB_HEIGHT, beetos::FB_WIDTH,
        ));
    }

    // Acquire the display and keyboard focus, print banner + prompt, then release.
    let (row, col) = acquire_display();
    acquire_input_focus();
    unsafe {
        if let Some(ref mut con) = FB_CONSOLE {
            con.set_cursor(row, col);
        }
    }
    puts("\n");
    puts("  ____            _    ___  ____\n");
    puts(" | __ )  ___  ___| |_ / _ \\/ ___|\n");
    puts(" |  _ \\ / _ \\/ _ \\ __| | | \\___ \\\n");
    puts(" | |_) |  __/  __/ |_| |_| |___) |\n");
    puts(" |____/ \\___|\\___|\\__|\\___/|____/\n");
    puts("\n");
    puts("BeetOS v0.1.0 — Type 'help' for commands.\n");
    puts("Shell running as userspace process (EL0)\n");
    block_selftest();
    puts("\n");
    prompt();
    let (row, col) = fb_cursor();
    release_display(row, col);

    // Create console server and receive characters from UART IRQ handler
    let sid = xous::SID::from_array(beetos_api_console::CONSOLE_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    loop {
        // Wait for a keypress — display is NOT held here.
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));
        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                if let xous::Message::Scalar(scalar) = env.body {
                    if scalar.id == beetos_api_console::ConsoleOp::Char as usize {
                        // Acquire display, sync cursor, process char, release.
                        // (Input focus is already held — no need to re-acquire.)
                        let (row, col) = acquire_display();
                        unsafe {
                            if let Some(ref mut con) = FB_CONSOLE {
                                con.set_cursor(row, col);
                            }
                        }
                        process_char(scalar.arg1 as u8);
                        let (row, col) = fb_cursor();
                        release_display(row, col);
                    }
                }
            }
            _ => { xous::yield_slice(); }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Direct UART, bypassing the console service: mid-panic the IPC
    // machinery can't be trusted, and the service might be the victim.
    for b in b"PANIC in shell!\n" {
        uart_putc(*b);
    }
    loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }; }
}
