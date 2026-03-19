// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS interactive shell (bsh).
//!
//! Currently runs in kernel context (no separate process) using UART
//! for I/O. Will be extracted into a proper Xous userspace service
//! when the full kernel init path is operational.
//!
//! Supports basic builtins: help, echo, info, mem, reboot, ls, cat,
//! write, rm, mkdir, uptime.

pub mod ramfs;

use core::fmt::Write;

/// Maximum line buffer length.
const MAX_LINE: usize = 256;

/// Maximum number of arguments per command.
const MAX_ARGS: usize = 16;

/// Shell state.
struct Shell {
    /// Line buffer for current input.
    line: [u8; MAX_LINE],
    /// Current position in line buffer.
    pos: usize,
}

static mut SHELL: Shell = Shell {
    line: [0u8; MAX_LINE],
    pos: 0,
};

/// Platform-abstracted output writer.
/// On QEMU: writes to PL011 UART.
#[cfg(feature = "platform-qemu-virt")]
fn writer() -> crate::platform::qemu_virt::uart::UartWriter {
    crate::platform::qemu_virt::uart::UartWriter
}

#[cfg(feature = "platform-qemu-virt")]
fn puts(s: &str) {
    crate::platform::qemu_virt::uart::puts(s);
}

#[cfg(feature = "platform-qemu-virt")]
fn putc(c: u8) {
    crate::platform::qemu_virt::uart::putc(c);
}

// Stubs for non-QEMU platforms (to be implemented)
#[cfg(not(feature = "platform-qemu-virt"))]
fn writer() -> NullWriter { NullWriter }
#[cfg(not(feature = "platform-qemu-virt"))]
fn puts(_s: &str) {}
#[cfg(not(feature = "platform-qemu-virt"))]
fn putc(_c: u8) {}

#[cfg(not(feature = "platform-qemu-virt"))]
struct NullWriter;
#[cfg(not(feature = "platform-qemu-virt"))]
impl Write for NullWriter {
    fn write_str(&mut self, _s: &str) -> core::fmt::Result { Ok(()) }
}

/// Initialize the shell: set up ramfs, print welcome banner.
#[allow(dead_code)]
pub fn init() {
    ramfs::init();

    // Create some default directories
    let _ = ramfs::mkdir("/tmp");
    let _ = ramfs::mkdir("/etc");

    // Create a welcome file
    let _ = ramfs::write("/etc/motd", b"Welcome to BeetOS!\n");

    puts("\n");
    puts("  ____            _    ___  ____\n");
    puts(" | __ )  ___  ___| |_ / _ \\/ ___|\n");
    puts(" |  _ \\ / _ \\/ _ \\ __| | | \\___ \\\n");
    puts(" | |_) |  __/  __/ |_| |_| |___) |\n");
    puts(" |____/ \\___|\\___|\\__|\\___/|____/\n");
    puts("\n");
    puts("BeetOS v0.1.0 — Type 'help' for commands.\n");
    puts("\n");
    prompt();
}

/// Print the shell prompt.
fn prompt() {
    puts("bsh> ");
}

/// Process a single character of input.
/// Called from the UART RX interrupt handler or polling loop.
pub fn process_char(c: u8) {
    unsafe {
        match c {
            // Backspace / DEL
            0x7F | 0x08 => {
                if SHELL.pos > 0 {
                    SHELL.pos -= 1;
                    // Erase character on terminal: BS, space, BS
                    putc(0x08);
                    putc(b' ');
                    putc(0x08);
                }
            }
            // Enter / newline
            b'\r' | b'\n' => {
                putc(b'\n');
                let line_len = SHELL.pos;
                SHELL.pos = 0;

                if line_len > 0 {
                    // Make a copy of the line for processing
                    let mut cmd_buf = [0u8; MAX_LINE];
                    cmd_buf[..line_len].copy_from_slice(&SHELL.line[..line_len]);
                    execute_line(&cmd_buf[..line_len]);
                }
                prompt();
            }
            // Ctrl-C
            0x03 => {
                puts("^C\n");
                SHELL.pos = 0;
                prompt();
            }
            // Ctrl-D (on empty line = info about ignoring)
            0x04 => {
                if SHELL.pos == 0 {
                    puts("\n(use 'reboot' to restart)\n");
                    prompt();
                }
            }
            // Printable ASCII
            0x20..=0x7E => {
                if SHELL.pos < MAX_LINE - 1 {
                    SHELL.line[SHELL.pos] = c;
                    SHELL.pos += 1;
                    putc(c); // echo
                }
            }
            // Ignore everything else (escape sequences, etc.)
            _ => {}
        }
    }
}

/// Parse and execute a command line.
fn execute_line(line: &[u8]) {
    // Convert to str (we only accepted ASCII)
    let line_str = match core::str::from_utf8(line) {
        Ok(s) => s.trim(),
        Err(_) => return,
    };

    if line_str.is_empty() {
        return;
    }

    // Split into args (simple space splitting)
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
        "uptime" => cmd_uptime(),
        "reboot" => cmd_reboot(),
        "ls" => cmd_ls(cmd_args),
        "cat" => cmd_cat(cmd_args),
        "write" => cmd_write(cmd_args, line_str),
        "rm" => cmd_rm(cmd_args),
        "mkdir" => cmd_mkdir(cmd_args),
        _ => {
            let mut w = writer();
            let _ = write!(w, "bsh: command not found: {}\n", cmd);
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
    puts("  uptime            Time since boot\n");
    puts("  reboot            Restart the system\n");
    puts("  ls [path]         List directory contents\n");
    puts("  cat <path>        Display file contents\n");
    puts("  write <path> <text>  Write text to a file\n");
    puts("  rm <path>         Remove a file or empty directory\n");
    puts("  mkdir <path>      Create a directory\n");
}

fn cmd_echo(args: &[&str]) {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            putc(b' ');
        }
        puts(arg);
    }
    putc(b'\n');
}

fn cmd_info() {
    puts("BeetOS v0.1.0\n");
    puts("Kernel: Xous microkernel (AArch64)\n");

    #[cfg(feature = "platform-qemu-virt")]
    puts("Platform: QEMU virt (cortex-a72)\n");

    #[cfg(feature = "platform-apple-t8103")]
    puts("Platform: Apple M1 (T8103)\n");

    let mut w = writer();
    let _ = write!(w, "Page size: {} bytes\n", beetos::PAGE_SIZE);
}

fn cmd_mem() {
    let (used, total, bytes) = ramfs::stats();
    let mut w = writer();
    let _ = write!(w, "RAM filesystem:\n");
    let _ = write!(w, "  Files: {}/{}\n", used, total);
    let _ = write!(w, "  Used:  {} bytes\n", bytes);
}

fn cmd_uptime() {
    #[cfg(feature = "platform-qemu-virt")]
    {
        let ticks = crate::platform::qemu_virt::timer::tick_count();
        let seconds = ticks / 100;
        let minutes = seconds / 60;
        let mut w = writer();
        let _ = write!(w, "up {}m {}s ({} ticks)\n", minutes, seconds % 60, ticks);
    }

    #[cfg(not(feature = "platform-qemu-virt"))]
    puts("uptime: not available\n");
}

fn cmd_reboot() {
    puts("Rebooting...\n");
    #[cfg(feature = "platform-qemu-virt")]
    {
        // QEMU virt: use PSCI SYSTEM_RESET via HVC
        unsafe {
            let psci_system_reset: u64 = 0x84000009;
            core::arch::asm!(
                "hvc #0",
                in("x0") psci_system_reset,
                options(noreturn)
            );
        }
    }

    #[cfg(not(feature = "platform-qemu-virt"))]
    puts("reboot: not implemented\n");
}

fn cmd_ls(args: &[&str]) {
    let path = if args.is_empty() { "/" } else { args[0] };

    match ramfs::list(path, |name, is_dir, size| {
        let mut w = writer();
        if is_dir {
            let _ = write!(w, "  {}/\n", name);
        } else {
            let _ = write!(w, "  {} ({} bytes)\n", name, size);
        }
    }) {
        Ok(()) => {}
        Err(ramfs::FsError::NotFound) => {
            let mut w = writer();
            let _ = write!(w, "ls: {}: not found\n", path);
        }
        Err(ramfs::FsError::NotDirectory) => {
            let mut w = writer();
            let _ = write!(w, "ls: {}: not a directory\n", path);
        }
        Err(e) => {
            let mut w = writer();
            let _ = write!(w, "ls: error: {:?}\n", e);
        }
    }
}

fn cmd_cat(args: &[&str]) {
    if args.is_empty() {
        puts("usage: cat <path>\n");
        return;
    }

    match ramfs::read(args[0]) {
        Ok(data) => {
            // Print as UTF-8 text, falling back to hex for non-UTF-8
            match core::str::from_utf8(data) {
                Ok(text) => {
                    puts(text);
                    // Ensure newline at end
                    if !text.ends_with('\n') {
                        putc(b'\n');
                    }
                }
                Err(_) => {
                    let mut w = writer();
                    let _ = write!(w, "<binary: {} bytes>\n", data.len());
                }
            }
        }
        Err(ramfs::FsError::NotFound) => {
            let mut w = writer();
            let _ = write!(w, "cat: {}: not found\n", args[0]);
        }
        Err(ramfs::FsError::IsDirectory) => {
            let mut w = writer();
            let _ = write!(w, "cat: {}: is a directory\n", args[0]);
        }
        Err(e) => {
            let mut w = writer();
            let _ = write!(w, "cat: error: {:?}\n", e);
        }
    }
}

fn cmd_write(args: &[&str], full_line: &str) {
    if args.len() < 2 {
        puts("usage: write <path> <text>\n");
        return;
    }

    let path = args[0];
    // Get everything after "write <path> " as the content
    // Find the start of the content by skipping "write" and the path
    let content = if let Some(pos) = full_line.find(path) {
        let after_path = pos + path.len();
        full_line[after_path..].trim_start()
    } else {
        args[1..].join_with_spaces()
    };

    match ramfs::write(path, content.as_bytes()) {
        Ok(()) => {
            let mut w = writer();
            let _ = write!(w, "wrote {} bytes to {}\n", content.len(), path);
        }
        Err(ramfs::FsError::IsDirectory) => {
            let mut w = writer();
            let _ = write!(w, "write: {}: is a directory\n", path);
        }
        Err(e) => {
            let mut w = writer();
            let _ = write!(w, "write: error: {:?}\n", e);
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
            let mut w = writer();
            let _ = write!(w, "rm: {}: not found\n", args[0]);
        }
        Err(ramfs::FsError::NotEmpty) => {
            let mut w = writer();
            let _ = write!(w, "rm: {}: directory not empty\n", args[0]);
        }
        Err(e) => {
            let mut w = writer();
            let _ = write!(w, "rm: error: {:?}\n", e);
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
            let mut w = writer();
            let _ = write!(w, "mkdir: {}: already exists\n", args[0]);
        }
        Err(e) => {
            let mut w = writer();
            let _ = write!(w, "mkdir: error: {:?}\n", e);
        }
    }
}

// ============================================================================
// Helper trait for joining string slices without alloc
// ============================================================================

trait JoinWithSpaces {
    fn join_with_spaces(&self) -> &str;
}

impl JoinWithSpaces for [&str] {
    fn join_with_spaces(&self) -> &str {
        if self.is_empty() {
            ""
        } else {
            self[0]
        }
    }
}

#[cfg(test)]
#[allow(static_mut_refs)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Reuse ramfs test lock since we share the same static state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ramfs::reset();
        ramfs::init();
        guard
    }

    #[test]
    fn test_shell_init() {
        let _g = setup();
        init();
        // init creates /tmp, /etc, /etc/motd
        let data = ramfs::read("/etc/motd").expect("motd should exist after init");
        assert_eq!(data, b"Welcome to BeetOS!\n");
    }

    #[test]
    fn test_mkdir_command() {
        let _g = setup();
        execute_line(b"mkdir /test");
        // Verify directory was created via ramfs
        let mut found = false;
        ramfs::list("/", |name, is_dir, _| {
            if name == "test" && is_dir {
                found = true;
            }
        })
        .expect("list");
        assert!(found, "mkdir command should create directory");
    }

    #[test]
    fn test_write_command() {
        let _g = setup();
        execute_line(b"write /hello.txt hello world");
        let data = ramfs::read("/hello.txt").expect("read after write cmd");
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn test_rm_command() {
        let _g = setup();
        ramfs::write("/tmp.txt", b"data").expect("setup write");
        execute_line(b"rm /tmp.txt");
        assert!(ramfs::read("/tmp.txt").is_err(), "rm should delete the file");
    }

    #[test]
    fn test_empty_line_does_nothing() {
        let _g = setup();
        execute_line(b"");
        execute_line(b"   ");
        // Should not panic or crash
    }

    #[test]
    fn test_unknown_command_does_not_crash() {
        let _g = setup();
        execute_line(b"nonexistent_command arg1 arg2");
        // Should not panic
    }

    #[test]
    fn test_process_char_basic() {
        let _g = setup();
        // Process printable chars and verify they're buffered
        unsafe {
            SHELL.pos = 0;
        }
        process_char(b'h');
        process_char(b'i');
        unsafe {
            assert_eq!(SHELL.pos, 2);
            assert_eq!(SHELL.line[0], b'h');
            assert_eq!(SHELL.line[1], b'i');
        }
    }

    #[test]
    fn test_process_char_backspace() {
        let _g = setup();
        unsafe {
            SHELL.pos = 0;
        }
        process_char(b'a');
        process_char(b'b');
        process_char(0x7F); // DEL/backspace
        unsafe {
            assert_eq!(SHELL.pos, 1);
            assert_eq!(SHELL.line[0], b'a');
        }
    }

    #[test]
    fn test_process_char_backspace_at_start() {
        let _g = setup();
        unsafe {
            SHELL.pos = 0;
        }
        process_char(0x7F); // backspace at empty buffer
        unsafe {
            assert_eq!(SHELL.pos, 0); // should stay at 0
        }
    }

    #[test]
    fn test_process_char_ctrl_c() {
        let _g = setup();
        unsafe {
            SHELL.pos = 0;
        }
        process_char(b'a');
        process_char(b'b');
        process_char(0x03); // Ctrl-C
        unsafe {
            assert_eq!(SHELL.pos, 0, "Ctrl-C should reset line buffer");
        }
    }
}
