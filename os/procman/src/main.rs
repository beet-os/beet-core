// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Process Manager service.
//!
//! Userspace service that handles process lifecycle via Xous IPC.
//! Receives spawn/wait requests from the shell (or other processes),
//! calls kernel syscalls (SpawnByName, WaitProcess), and returns results.

#![no_std]
#![no_main]

use core::fmt::Write;
use core::panic::PanicInfo;

use beetos_api_procman::{ProcManOp, PROCMAN_SID};

// ============================================================================
// UART output (mapped by kernel at boot)
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 {
            return;
        }
        let base = UART_BASE;
        while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
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

struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        puts(s);
        Ok(())
    }
}

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // x0 = UART MMIO base VA (set by kernel before ERET)
    let uart_base: usize;
    unsafe {
        core::arch::asm!("mov {}, x0", out(reg) uart_base, options(nomem, nostack));
        UART_BASE = uart_base;
    }

    // Create the PROCMAN server with well-known SID
    let sid = xous::SID::from_array(PROCMAN_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    // Main message loop
    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));
        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                match &env.body {
                    xous::Message::BlockingScalar(scalar) => {
                        handle_blocking_scalar(env.sender, *scalar);
                    }
                    xous::Message::Scalar(scalar) => {
                        handle_scalar(env.sender, *scalar);
                    }
                    xous::Message::MutableBorrow(mem) => {
                        // Handle mutable borrow messages manually.
                        // We must NOT let the Envelope Drop auto-return memory
                        // because we need to write the exit code first.
                        handle_mutable_borrow_ref(env.sender, mem);
                        // Prevent the Envelope's Drop from returning memory again
                        core::mem::forget(env);
                    }
                    _ => {
                        // Ignore other message types
                    }
                }
            }
            _ => {
                xous::yield_slice();
            }
        }
    }
}

fn handle_blocking_scalar(sender: xous::MessageSender, scalar: xous::ScalarMessage) {
    match scalar.id {
        id if id == ProcManOp::SpawnAndWait as usize => {
            // Unpack name from arg1-arg4
            let name_args = [scalar.arg1, scalar.arg2, scalar.arg3, scalar.arg4];
            let name = beetos_api_procman::unpack_name(&name_args);

            // SpawnByName syscall
            let spawn_result = xous::rsyscall(xous::SysCall::SpawnByName(
                name_args[0], name_args[1], name_args[2], name_args[3],
            ));

            match spawn_result {
                Ok(xous::Result::ProcessID(pid)) => {
                    // WaitProcess syscall — blocks until the spawned process exits
                    let wait_result = xous::rsyscall(xous::SysCall::WaitProcess(pid));
                    let exit_code = match wait_result {
                        Ok(xous::Result::Scalar1(code)) => code,
                        _ => usize::MAX, // error sentinel
                    };
                    // Return exit code to caller
                    xous::return_scalar(sender, exit_code).ok();
                }
                Err(_e) => {
                    let _ = write!(UartWriter, "[procman] spawn failed for '{}'\n", name);
                    // Return error sentinel
                    xous::return_scalar(sender, usize::MAX).ok();
                }
                _ => {
                    xous::return_scalar(sender, usize::MAX).ok();
                }
            }
        }
        id if id == ProcManOp::Wait as usize => {
            // arg1 = pid
            if let Some(pid) = xous::PID::new(scalar.arg1 as u8) {
                let wait_result = xous::rsyscall(xous::SysCall::WaitProcess(pid));
                let exit_code = match wait_result {
                    Ok(xous::Result::Scalar1(code)) => code,
                    _ => usize::MAX,
                };
                xous::return_scalar(sender, exit_code).ok();
            } else {
                xous::return_scalar(sender, usize::MAX).ok();
            }
        }
        _ => {
            xous::return_scalar(sender, usize::MAX).ok();
        }
    }
}

fn handle_scalar(sender: xous::MessageSender, scalar: xous::ScalarMessage) {
    let _ = sender;
    match scalar.id {
        id if id == ProcManOp::Spawn as usize => {
            // Unpack name from arg1-arg4
            let name_args = [scalar.arg1, scalar.arg2, scalar.arg3, scalar.arg4];
            let _name = beetos_api_procman::unpack_name(&name_args);

            // SpawnByName — fire and forget
            let _spawn_result = xous::rsyscall(xous::SysCall::SpawnByName(
                name_args[0], name_args[1], name_args[2], name_args[3],
            ));
        }
        _ => {}
    }
}

fn handle_mutable_borrow_ref(sender: xous::MessageSender, mem: &xous::MemoryMessage) {
    match mem.id {
        id if id == ProcManOp::SpawnAndWaitWithArgs as usize => {
            let valid_len = mem.valid.map(|v| v.get()).unwrap_or(0);
            let buf = unsafe {
                core::slice::from_raw_parts(mem.buf.as_ptr(), mem.buf.len())
            };
            let (name, args_start, args_len) = beetos_api_procman::parse_cmdline(buf, valid_len);

            // Prepare argv data (portion after the name)
            let argv_data = if args_len > 0 {
                &buf[args_start..args_start + args_len]
            } else {
                &[]
            };

            // Call SpawnByNameWithArgs syscall
            let name_packed = beetos_api_procman::pack_name_short(name);
            let argv_ptr = if argv_data.is_empty() { 0 } else { argv_data.as_ptr() as usize };
            let spawn_result = xous::rsyscall(xous::SysCall::SpawnByNameWithArgs(
                name_packed[0], name_packed[1], argv_ptr, argv_data.len(),
            ));

            // Write the exit code into the buffer's first usize (so caller can read it)
            let exit_code = match spawn_result {
                Ok(xous::Result::ProcessID(pid)) => {
                    // Wait for the process to exit
                    let wait_result = xous::rsyscall(xous::SysCall::WaitProcess(pid));
                    match wait_result {
                        Ok(xous::Result::Scalar1(code)) => code,
                        _ => usize::MAX,
                    }
                }
                Err(_e) => {
                    let _ = write!(UartWriter, "[procman] spawn failed for '{}'\n", name);
                    usize::MAX
                }
                _ => usize::MAX,
            };

            // Write exit code into the buffer so the caller can read it
            let buf_mut = unsafe {
                core::slice::from_raw_parts_mut(mem.buf.as_mut_ptr(), mem.buf.len())
            };
            if buf_mut.len() >= core::mem::size_of::<usize>() {
                let exit_bytes = exit_code.to_le_bytes();
                buf_mut[..exit_bytes.len()].copy_from_slice(&exit_bytes);
            }

            // Return the memory to unblock the caller
            xous::return_memory_offset_valid(sender, mem.buf, None, None).ok();
        }
        _ => {
            // Unknown opcode — just return memory
            xous::return_memory_offset_valid(sender, mem.buf, None, None).ok();
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in procman!\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
