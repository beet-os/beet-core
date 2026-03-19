// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS interactive shell (bsh) — runs as a userspace process.
//!
//! Receives UART characters from the kernel via IPC, writes output
//! directly to UART MMIO (mapped into our address space by the kernel).

#![no_std]
#![no_main]

mod ramfs;

use core::fmt::Write;
use core::panic::PanicInfo;

// ============================================================================
// UART output via mapped MMIO
// ============================================================================

/// PL011 register offsets.
const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

/// UART base address in our virtual address space.
/// Set by the kernel via x0 before ERET.
static mut UART_BASE: usize = 0;

fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 {
            return;
        }
        let base = UART_BASE;
        // Wait for TX FIFO to have space
        while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        // Add CR before LF for terminal compatibility
        if c == b'\n' {
            core::ptr::write_volatile((base + UART_DR) as *mut u32, b'\r' as u32);
            while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        }
        core::ptr::write_volatile((base + UART_DR) as *mut u32, c as u32);
    }
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

fn put_usize(mut n: usize) {
    if n == 0 {
        putc(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        putc(buf[i]);
    }
}

/// Writer for `core::fmt::Write`.
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

struct Shell {
    line: [u8; MAX_LINE],
    pos: usize,
}

static mut SHELL: Shell = Shell {
    line: [0u8; MAX_LINE],
    pos: 0,
};

fn prompt() {
    puts("bsh> ");
}

fn process_char(c: u8) {
    unsafe {
        match c {
            0x7F | 0x08 => {
                if SHELL.pos > 0 {
                    SHELL.pos -= 1;
                    putc(0x08);
                    putc(b' ');
                    putc(0x08);
                }
            }
            b'\r' | b'\n' => {
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
            0x03 => {
                puts("^C\n");
                SHELL.pos = 0;
                prompt();
            }
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
    if line_str.is_empty() {
        return;
    }

    let mut args: [&str; MAX_ARGS] = [""; MAX_ARGS];
    let mut argc = 0;
    for part in line_str.split_ascii_whitespace() {
        if argc < MAX_ARGS {
            args[argc] = part;
            argc += 1;
        }
    }
    if argc == 0 {
        return;
    }

    let cmd = args[0];
    let cmd_args = &args[1..argc];

    match cmd {
        "help" => cmd_help(),
        "echo" => cmd_echo(cmd_args),
        "info" => cmd_info(),
        "mem" => cmd_mem(),
        "pid" => cmd_pid(),
        "ls" => cmd_ls(cmd_args),
        "cat" => cmd_cat(cmd_args),
        "write" => cmd_write(cmd_args, line_str),
        "rm" => cmd_rm(cmd_args),
        "mkdir" => cmd_mkdir(cmd_args),
        _ => {
            // Try to spawn via procman
            try_spawn_via_procman(cmd);
        }
    }
}

// ============================================================================
// Built-in commands
// ============================================================================

fn cmd_help() {
    puts("BeetOS shell commands:\n");
    puts("  help              Show this help\n");
    puts("  echo [text...]    Print text\n");
    puts("  info              System information\n");
    puts("  mem               Memory/filesystem statistics\n");
    puts("  pid               Show current process ID\n");
    puts("  ls [path]         List directory contents\n");
    puts("  cat <path>        Display file contents\n");
    puts("  write <path> <text>  Write text to a file\n");
    puts("  rm <path>         Remove a file or empty directory\n");
    puts("  mkdir <path>      Create a directory\n");
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
    let result = xous::rsyscall(xous::SysCall::GetProcessId);
    match result {
        Ok(xous::Result::Scalar1(pid)) => {
            let _ = write!(UartWriter, "PID: {}\n", pid);
        }
        _ => {
            puts("pid: syscall failed\n");
        }
    }
}

fn cmd_mem() {
    let (used, total, bytes) = ramfs::stats();
    let _ = write!(UartWriter, "RAM filesystem:\n");
    let _ = write!(UartWriter, "  Files: {}/{}\n", used, total);
    let _ = write!(UartWriter, "  Used:  {} bytes\n", bytes);
}

fn cmd_ls(args: &[&str]) {
    let path = if args.is_empty() { "/" } else { args[0] };
    match ramfs::list(path, |name, is_dir, size| {
        if is_dir {
            let _ = write!(UartWriter, "  {}/\n", name);
        } else {
            let _ = write!(UartWriter, "  {} ({} bytes)\n", name, size);
        }
    }) {
        Ok(()) => {}
        Err(ramfs::FsError::NotFound) => {
            let _ = write!(UartWriter, "ls: {}: not found\n", path);
        }
        Err(ramfs::FsError::NotDirectory) => {
            let _ = write!(UartWriter, "ls: {}: not a directory\n", path);
        }
        Err(e) => {
            let _ = write!(UartWriter, "ls: error: {:?}\n", e);
        }
    }
}

fn cmd_cat(args: &[&str]) {
    if args.is_empty() {
        puts("usage: cat <path>\n");
        return;
    }
    match ramfs::read(args[0]) {
        Ok(data) => match core::str::from_utf8(data) {
            Ok(text) => {
                puts(text);
                if !text.ends_with('\n') { putc(b'\n'); }
            }
            Err(_) => {
                let _ = write!(UartWriter, "<binary: {} bytes>\n", data.len());
            }
        },
        Err(ramfs::FsError::NotFound) => {
            let _ = write!(UartWriter, "cat: {}: not found\n", args[0]);
        }
        Err(ramfs::FsError::IsDirectory) => {
            let _ = write!(UartWriter, "cat: {}: is a directory\n", args[0]);
        }
        Err(e) => {
            let _ = write!(UartWriter, "cat: error: {:?}\n", e);
        }
    }
}

fn cmd_write(args: &[&str], full_line: &str) {
    if args.len() < 2 {
        puts("usage: write <path> <text>\n");
        return;
    }
    let path = args[0];
    let content = if let Some(pos) = full_line.find(path) {
        let after_path = pos + path.len();
        full_line[after_path..].trim_start()
    } else {
        args[1]
    };
    match ramfs::write(path, content.as_bytes()) {
        Ok(()) => {
            let _ = write!(UartWriter, "wrote {} bytes to {}\n", content.len(), path);
        }
        Err(ramfs::FsError::IsDirectory) => {
            let _ = write!(UartWriter, "write: {}: is a directory\n", path);
        }
        Err(e) => {
            let _ = write!(UartWriter, "write: error: {:?}\n", e);
        }
    }
}

fn cmd_rm(args: &[&str]) {
    if args.is_empty() {
        puts("usage: rm <path>\n");
        return;
    }
    match ramfs::remove(args[0]) {
        Ok(()) => {}
        Err(ramfs::FsError::NotFound) => {
            let _ = write!(UartWriter, "rm: {}: not found\n", args[0]);
        }
        Err(ramfs::FsError::NotEmpty) => {
            let _ = write!(UartWriter, "rm: {}: directory not empty\n", args[0]);
        }
        Err(e) => {
            let _ = write!(UartWriter, "rm: error: {:?}\n", e);
        }
    }
}

fn cmd_mkdir(args: &[&str]) {
    if args.is_empty() {
        puts("usage: mkdir <path>\n");
        return;
    }
    match ramfs::mkdir(args[0]) {
        Ok(()) => {}
        Err(ramfs::FsError::AlreadyExists) => {
            let _ = write!(UartWriter, "mkdir: {}: already exists\n", args[0]);
        }
        Err(e) => {
            let _ = write!(UartWriter, "mkdir: error: {:?}\n", e);
        }
    }
}

// ============================================================================
// Process spawning via procman
// ============================================================================

/// Connection ID to the procman service (lazily initialized).
static mut PROCMAN_CID: u32 = 0;

fn get_procman_cid() -> u32 {
    unsafe {
        if PROCMAN_CID != 0 {
            return PROCMAN_CID;
        }
        // Connect to procman (blocks until procman creates its server)
        let sid = xous::SID::from_array(beetos_api_procman::PROCMAN_SID);
        match xous::rsyscall(xous::SysCall::Connect(sid)) {
            Ok(xous::Result::ConnectionID(cid)) => {
                PROCMAN_CID = cid;
                cid
            }
            _ => 0,
        }
    }
}

fn try_spawn_via_procman(cmd: &str) {
    let cid = get_procman_cid();
    if cid == 0 {
        let _ = write!(UartWriter, "bsh: {}: procman not available\n", cmd);
        return;
    }

    let name_packed = beetos_api_procman::pack_name(cmd);
    let result = xous::rsyscall(xous::SysCall::SendMessage(
        cid,
        xous::Message::BlockingScalar(xous::ScalarMessage {
            id: beetos_api_procman::ProcManOp::SpawnAndWait as usize,
            arg1: name_packed[0],
            arg2: name_packed[1],
            arg3: name_packed[2],
            arg4: name_packed[3],
        }),
    ));

    match result {
        Ok(xous::Result::Scalar1(exit_code)) | Ok(xous::Result::Scalar2(exit_code, _)) => {
            if exit_code == usize::MAX {
                puts("bsh: ");
                puts(cmd);
                puts(": not found\n");
            } else {
                puts("[exited: ");
                put_usize(exit_code);
                puts("]\n");
            }
        }
        Err(_) => {
            puts("bsh: ");
            puts(cmd);
            puts(": spawn failed\n");
        }
        _ => {
            puts("bsh: ");
            puts(cmd);
            puts(": unexpected result\n");
        }
    }
}

// ============================================================================
// Entry point
// ============================================================================

/// Entry point. The kernel passes the UART MMIO VA in x0.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // x0 = UART MMIO base VA (set by kernel before ERET)
    let uart_base: usize;
    unsafe {
        core::arch::asm!("mov {}, x0", out(reg) uart_base, options(nomem, nostack));
        UART_BASE = uart_base;
    }

    // Initialize ramfs
    ramfs::init();
    let _ = ramfs::mkdir("/tmp");
    let _ = ramfs::mkdir("/etc");
    let _ = ramfs::write("/etc/motd", b"Welcome to BeetOS!\n");

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

    // Create a server and receive characters from the kernel's UART IRQ handler.
    let sid = xous::SID::from_array(beetos_api_console::CONSOLE_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));
        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                // Extract char from Scalar message
                if let xous::Message::Scalar(scalar) = env.body {
                    if scalar.id == beetos_api_console::ConsoleOp::Char as usize {
                        process_char(scalar.arg1 as u8);
                    }
                }
            }
            _ => {
                // Yield on error and retry
                xous::yield_slice();
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in shell!\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
