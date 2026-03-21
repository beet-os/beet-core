// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS interactive shell (bsh) — runs as a userspace process.
//!
//! Receives UART characters from the kernel via IPC, writes output
//! directly to UART MMIO (mapped into our address space by the kernel).
//!
//! File operations (ls, cat, mkdir, rm, write) are delegated to the
//! filesystem service via Xous IPC (BlockingScalar).

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use beetos_api_fs::{FsError, FsOp, FS_SID};

// ============================================================================
// UART output via mapped MMIO
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 { return; }
        let base = UART_BASE;
        while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        if c == b'\n' {
            core::ptr::write_volatile((base + UART_DR) as *mut u32, b'\r' as u32);
            while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        }
        core::ptr::write_volatile((base + UART_DR) as *mut u32, c as u32);
    }
}

fn puts(s: &str) { for b in s.bytes() { putc(b); } }

struct UartWriter;
impl Write for UartWriter {
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
                    puts("\n(use 'reboot' to restart)\n");
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

        // FS operations (via IPC to fs service)
        "ls" => cmd_ls(cmd_args),
        "cat" => cmd_cat(cmd_args),
        "write" => cmd_write(cmd_args, line_str),
        "rm" => cmd_rm(cmd_args),
        "mkdir" => cmd_mkdir(cmd_args),
        "blkinfo" => cmd_blkinfo(),
        "mem" => cmd_mem(),
        "ifconfig" => cmd_ifconfig(),

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
    puts("  ls [path]         List directory (ramfs or /disk/)\n");
    puts("  cat <path>        Display file contents\n");
    puts("  write <path> <text>  Write text to a file (ramfs only)\n");
    puts("  rm <path>         Remove a file or empty directory\n");
    puts("  mkdir <path>      Create a directory\n");
    puts("  blkinfo           Block device info\n");
    puts("  mem               Filesystem statistics\n");
    puts("  ifconfig          Show network interface configuration\n");
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
    let _ = write!(UartWriter, "Page size: {} bytes\n", 16384);
    puts("Shell: userspace process (EL0)\n");
}

fn cmd_pid() {
    match xous::rsyscall(xous::SysCall::GetProcessId) {
        Ok(xous::Result::Scalar1(pid)) => { let _ = write!(UartWriter, "PID: {}\n", pid); }
        _ => puts("pid: syscall failed\n"),
    }
}

fn cmd_pwd() {
    puts(cwd_str());
    putc(b'\n');
}

fn cmd_cd(args: &[&str]) {
    let target = if args.is_empty() { "/" } else { args[0] };
    let mut buf = [0u8; MAX_PATH];
    let resolved = resolve_path(target, &mut buf);

    let packed = beetos_api_fs::pack_path(resolved);
    match fs_scalar(FsOp::IsDir, packed[0], packed[1], packed[2], packed[3]) {
        Some(code) if code == FsError::Ok as usize => {
            unsafe {
                let bytes = resolved.as_bytes();
                let len = bytes.len().min(MAX_PATH);
                CWD_BUF[..len].copy_from_slice(&bytes[..len]);
                CWD_LEN = len;
            }
        }
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(UartWriter, "cd: {}: no such directory\n", target);
        }
        Some(code) if code == FsError::NotDirectory as usize => {
            let _ = write!(UartWriter, "cd: {}: not a directory\n", target);
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
        Ok(xous::Result::Scalar5(a, b, c, d, e)) => {
            // Store for Stats — caller reads LAST_STATS
            unsafe { LAST_STATS = (a, b, c, d, e); }
            Some(0)
        }
        _ => None,
    }
}

static mut LAST_STATS: (usize, usize, usize, usize, usize) = (0, 0, 0, 0, 0);

fn cmd_ls(args: &[&str]) {
    let mut buf = [0u8; MAX_PATH];
    let path = if args.is_empty() {
        resolve_path(".", &mut buf)
    } else {
        resolve_path(args[0], &mut buf)
    };
    let packed = beetos_api_fs::pack_path(path);
    match fs_scalar(FsOp::Ls, packed[0], packed[1], packed[2], packed[3]) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(UartWriter, "ls: {}: not found\n", path);
        }
        Some(code) if code == FsError::NotDirectory as usize => {
            let _ = write!(UartWriter, "ls: {}: not a directory\n", path);
        }
        None => puts("ls: fs service not available\n"),
        _ => puts("ls: error\n"),
    }
}

fn cmd_cat(args: &[&str]) {
    if args.is_empty() { puts("usage: cat <path>\n"); return; }
    let mut buf = [0u8; MAX_PATH];
    let path = resolve_path(args[0], &mut buf);
    let packed = beetos_api_fs::pack_path(path);
    match fs_scalar(FsOp::Cat, packed[0], packed[1], packed[2], packed[3]) {
        Some(code) if code == FsError::Ok as usize => {}
        Some(code) if code == FsError::NotFound as usize => {
            let _ = write!(UartWriter, "cat: {}: not found\n", path);
        }
        Some(code) if code == FsError::IsDirectory as usize => {
            let _ = write!(UartWriter, "cat: {}: is a directory\n", path);
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
            let _ = write!(UartWriter, "write: {}: read-only\n", path);
        }
        Some(code) if code == FsError::IsDirectory as usize => {
            let _ = write!(UartWriter, "write: {}: is a directory\n", path);
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
            let _ = write!(UartWriter, "mkdir: {}: already exists\n", args[0]);
        }
        Some(code) if code == FsError::ReadOnly as usize => {
            let _ = write!(UartWriter, "mkdir: {}: read-only\n", args[0]);
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
            let _ = write!(UartWriter, "rm: {}: not found\n", args[0]);
        }
        Some(code) if code == FsError::NotEmpty as usize => {
            let _ = write!(UartWriter, "rm: {}: directory not empty\n", args[0]);
        }
        Some(code) if code == FsError::ReadOnly as usize => {
            let _ = write!(UartWriter, "rm: {}: read-only\n", args[0]);
        }
        None => puts("rm: fs service not available\n"),
        _ => puts("rm: error\n"),
    }
}

fn cmd_blkinfo() {
    match fs_scalar(FsOp::Stats, 0, 0, 0, 0) {
        Some(_) => {
            let (_, _, _, disk_size, disk_files) = unsafe { LAST_STATS };
            if disk_size == 0 {
                puts("No block device\n");
            } else {
                let _ = write!(UartWriter, "Block device: {} bytes\n", disk_size);
                puts("Mounted at: /disk/ (read-only, tar)\n");
                let _ = write!(UartWriter, "Files: {}\n", disk_files);
            }
        }
        None => puts("blkinfo: fs service not available\n"),
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
                UartWriter,
                "eth0: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
            );
            if ip == [0, 0, 0, 0] {
                puts("      inet: (no address — DHCP pending)\n");
            } else {
                let _ = write!(
                    UartWriter,
                    "      inet: {}.{}.{}.{}\n",
                    ip[0], ip[1], ip[2], ip[3],
                );
            }
        }
        _ => puts("ifconfig: NetGetInfo syscall failed\n"),
    }
}

fn cmd_mem() {
    match fs_scalar(FsOp::Stats, 0, 0, 0, 0) {
        Some(_) => {
            let (used, total, bytes, disk_size, _) = unsafe { LAST_STATS };
            puts("RAM filesystem:\n");
            let _ = write!(UartWriter, "  Files: {}/{}\n", used, total);
            let _ = write!(UartWriter, "  Used:  {} bytes\n", bytes);
            if disk_size > 0 {
                let _ = write!(UartWriter, "Disk: {} bytes\n", disk_size);
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
        let _ = write!(UartWriter, "bsh: {}: procman not available\n", cmd);
        return;
    }

    if args.is_empty() {
        // No args — use the simple scalar path (SpawnAndWait)
        let name_packed = beetos_api_procman::pack_name(cmd);
        let result = xous::rsyscall(xous::SysCall::SendMessage(
            cid,
            xous::Message::BlockingScalar(xous::ScalarMessage {
                id: beetos_api_procman::ProcManOp::SpawnAndWait as usize,
                arg1: name_packed[0], arg2: name_packed[1],
                arg3: name_packed[2], arg4: name_packed[3],
            }),
        ));
        match result {
            Ok(xous::Result::Scalar1(exit_code)) | Ok(xous::Result::Scalar2(exit_code, _)) => {
                if exit_code == usize::MAX {
                    let _ = write!(UartWriter, "bsh: {}: not found\n", cmd);
                } else if exit_code != 0 {
                    let _ = write!(UartWriter, "[exited: {}]\n", exit_code);
                }
            }
            Err(_) => { let _ = write!(UartWriter, "bsh: {}: spawn failed\n", cmd); }
            _ => { let _ = write!(UartWriter, "bsh: {}: unexpected result\n", cmd); }
        }
    } else {
        // Has args — allocate a page and send via MutableBorrow
        let page_size = xous::MemorySize::new(beetos::PAGE_SIZE);
        let page = if let Some(size) = page_size {
            xous::rsyscall(xous::SysCall::MapMemory(
                None, None, size, xous::MemoryFlags::W,
            ))
        } else {
            let _ = write!(UartWriter, "bsh: {}: internal error\n", cmd);
            return;
        };

        let buf = match page {
            Ok(xous::Result::MemoryRange(range)) => range,
            _ => {
                let _ = write!(UartWriter, "bsh: {}: out of memory\n", cmd);
                return;
            }
        };

        // Format the command line into the page: "name\0arg1\0arg2\0..."
        let page_slice = unsafe {
            core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len())
        };
        let valid_len = beetos_api_procman::format_cmdline(page_slice, cmd, args);

        // Send MutableBorrow to procman
        let valid = xous::MemorySize::new(valid_len);
        let result = xous::rsyscall(xous::SysCall::SendMessage(
            cid,
            xous::Message::MutableBorrow(xous::MemoryMessage {
                id: beetos_api_procman::ProcManOp::SpawnAndWaitWithArgs as usize,
                buf,
                offset: None,
                valid,
            }),
        ));

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
                    let _ = write!(UartWriter, "bsh: {}: not found\n", cmd);
                } else if exit_code != 0 {
                    let _ = write!(UartWriter, "[exited: {}]\n", exit_code);
                }
            }
            Err(_) => { let _ = write!(UartWriter, "bsh: {}: spawn failed\n", cmd); }
            _ => { let _ = write!(UartWriter, "bsh: {}: unexpected result\n", cmd); }
        }

        // Free the page
        xous::rsyscall(xous::SysCall::UnmapMemory(buf)).ok();
    }
}

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let uart_base: usize;
    unsafe {
        core::arch::asm!("mov {}, x0", out(reg) uart_base, options(nomem, nostack));
        UART_BASE = uart_base;
        CWD_BUF[0] = b'/';
        CWD_LEN = 1;
    }

    // Print banner
    puts("\n");
    puts("  ____            _    ___  ____\n");
    puts(" | __ )  ___  ___| |_ / _ \\/ ___|\n");
    puts(" |  _ \\ / _ \\/ _ \\ __| | | \\___ \\\n");
    puts(" | |_) |  __/  __/ |_| |_| |___) |\n");
    puts(" |____/ \\___|\\___|\\__|\\___/|____/\n");
    puts("\n");
    puts("BeetOS v0.1.0 — Type 'help' for commands.\n");
    puts("Shell running as userspace process (EL0)\n");
    puts("\n");
    prompt();

    // Create console server and receive characters from UART IRQ handler
    let sid = xous::SID::from_array(beetos_api_console::CONSOLE_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));
        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                if let xous::Message::Scalar(scalar) = env.body {
                    if scalar.id == beetos_api_console::ConsoleOp::Char as usize {
                        process_char(scalar.arg1 as u8);
                    }
                }
            }
            _ => { xous::yield_slice(); }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in shell!\n");
    loop { unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }; }
}
