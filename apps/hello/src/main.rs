// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS "hello" process.
//!
//! When used as "idle" (PID 2): yields in a loop forever, absorbing CPU
//! when no other process is ready.
//!
//! When spawned by name as "hello": prints a greeting with its PID,
//! tests heap allocation (Box, Vec, String), then exits cleanly.
//!
//! The kernel determines the behavior by checking the process name.
//! Both cases use the same binary — the name is set during create_process.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::panic::PanicInfo;

// ============================================================================
// Heap allocator — uses Xous IncreaseHeap syscall
// ============================================================================

mod heap {
    use core::alloc::{GlobalAlloc, Layout};
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Simple bump allocator backed by IncreaseHeap syscall.
    ///
    /// Allocates 16KB pages from the kernel on demand. Does not support
    /// deallocation (freed memory is leaked). Sufficient for demos and
    /// short-lived processes.
    pub struct BumpAllocator {
        /// Current allocation pointer (bumps upward).
        next: AtomicUsize,
        /// End of the current heap region.
        end: AtomicUsize,
    }

    impl BumpAllocator {
        pub const fn new() -> Self {
            Self {
                next: AtomicUsize::new(0),
                end: AtomicUsize::new(0),
            }
        }

        fn grow(&self, min_bytes: usize) -> bool {
            let page_size = 16384; // beetos::PAGE_SIZE
            let pages_needed = (min_bytes + page_size - 1) / page_size;
            let size = pages_needed * page_size;

            let size_nz = match core::num::NonZeroUsize::new(size) {
                Some(s) => s,
                None => return false,
            };

            match xous::rsyscall(xous::SysCall::IncreaseHeap(size_nz)) {
                Ok(xous::Result::MemoryRange(range)) => {
                    let base = range.as_ptr() as usize;
                    let len = range.len();
                    // If current heap is empty or exhausted, reset to new region.
                    // Note: this is a simple approach — non-contiguous regions
                    // waste the gap, but for demos this is fine.
                    self.next.store(base, Ordering::SeqCst);
                    self.end.store(base + len, Ordering::SeqCst);
                    true
                }
                _ => false,
            }
        }
    }

    unsafe impl GlobalAlloc for BumpAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            loop {
                let next = self.next.load(Ordering::SeqCst);
                let end = self.end.load(Ordering::SeqCst);

                let aligned = (next + layout.align() - 1) & !(layout.align() - 1);
                let new_next = aligned + layout.size();

                if new_next <= end {
                    // Try to bump the pointer
                    if self.next.compare_exchange(
                        next, new_next, Ordering::SeqCst, Ordering::SeqCst,
                    ).is_ok() {
                        return aligned as *mut u8;
                    }
                    // CAS failed — another allocation raced us, retry
                    continue;
                }

                // Need more memory
                if !self.grow(layout.size() + layout.align()) {
                    return core::ptr::null_mut();
                }
                // Retry with the new region
            }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // Bump allocator does not reclaim memory.
        }
    }
}

#[global_allocator]
static ALLOCATOR: heap::BumpAllocator = heap::BumpAllocator::new();

// ============================================================================
// UART output
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

fn uart_putc(c: u8) {
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

// ============================================================================
// Framebuffer output
// ============================================================================

use beetos::fb_console::FbConsole;

const FB_WIDTH:  usize = 1280;
const FB_HEIGHT: usize = 800;

static mut FB_CONSOLE: Option<FbConsole> = None;

fn fb_putc(c: u8) {
    unsafe {
        if let Some(ref mut con) = FB_CONSOLE {
            con.putc(c);
            // Keep shared cursor page up to date.
            let (row, col) = con.cursor();
            let ptr = beetos::SHARED_CURSOR_VA as *mut u32;
            core::ptr::write_volatile(ptr, row as u32);
            core::ptr::write_volatile(ptr.add(1), col as u32);
        }
    }
}

fn putc(c: u8) {
    uart_putc(c);
    fb_putc(c);
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

fn put_hex(n: usize) {
    puts("0x");
    for i in (0..16).rev() {
        let d = ((n >> (i * 4)) & 0xf) as u8;
        putc(if d < 10 { b'0' + d } else { b'a' + d - 10 });
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

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Read boot parameters from registers BEFORE any function call or syscall.
    // The kernel sets x0=uart_va, x1=fb_va, x2=argv_ptr, x3=argv_len before ERET.
    // Any syscall (including GetProcessId) will clobber x0-x7.
    let uart_base: usize;
    let fb_base: usize;
    let argv_ptr: usize;
    let argv_len: usize;
    unsafe {
        core::arch::asm!(
            "mov {0}, x0",
            "mov {1}, x1",
            "mov {2}, x2",
            "mov {3}, x3",
            out(reg) uart_base,
            out(reg) fb_base,
            out(reg) argv_ptr,
            out(reg) argv_len,
            options(nomem, nostack),
        );
        UART_BASE = uart_base;
        if fb_base != 0 {
            FB_CONSOLE = Some(FbConsole::new(fb_base as *mut u32, FB_WIDTH, FB_HEIGHT, FB_WIDTH));
            // Restore the cursor from the shared page so we write after
            // the shell's output, not at row 0.
            let ptr = beetos::SHARED_CURSOR_VA as *const u32;
            let row = core::ptr::read_volatile(ptr) as usize;
            let col = core::ptr::read_volatile(ptr.add(1)) as usize;
            if let Some(ref mut con) = FB_CONSOLE {
                con.set_cursor(row, col);
            }
        }
    }

    // Get our PID
    let pid = match xous::rsyscall(xous::SysCall::GetProcessId) {
        Ok(xous::Result::ProcessID(pid)) => pid.get() as usize,
        _ => 0,
    };

    // If we're PID 2 (idle), just yield forever
    if pid == 2 {
        loop {
            xous::yield_slice();
        }
    }

    // Otherwise, we were spawned as "hello" — print greeting and exit
    puts("Hello, BeetOS!\n");
    puts("I am PID ");
    put_usize(pid);
    puts(", running at EL0.\n");

    // Display argv if present
    if argv_ptr != 0 && argv_len > 0 {
        let argv_data = unsafe { core::slice::from_raw_parts(argv_ptr as *const u8, argv_len) };
        puts("argv: [");
        let mut first = true;
        for arg in argv_data.split(|&b| b == 0) {
            if arg.is_empty() { continue; }
            if !first { puts(", "); }
            first = false;
            puts("\"");
            if let Ok(s) = core::str::from_utf8(arg) {
                puts(s);
            } else {
                puts("<invalid utf8>");
            }
            puts("\"");
        }
        puts("]\n");
    }

    // Test heap allocation
    puts("Creating Box<u64>...\n");
    let b: Box<u64> = Box::new(42);
    puts("Box = ");
    put_usize(*b as usize);
    puts(" at ");
    put_hex(&*b as *const u64 as usize);
    puts("\n");

    puts("Creating Vec...\n");
    let v: Vec<u32> = vec![1, 2, 3, 4, 5];
    puts("Vec len=");
    put_usize(v.len());
    puts(" [");
    for (i, val) in v.iter().enumerate() {
        if i > 0 { puts(", "); }
        put_usize(*val as usize);
    }
    puts("]\n");

    puts("Creating String...\n");
    let s = String::from("alloc works on BeetOS!");
    puts(&s);
    puts("\n");

    puts("Testing format!...\n");
    let msg = format!("{} + {} = {}", 40, 2, 40 + 2);
    puts(&msg);
    puts("\n");

    puts("[done]\n");

    // Clean exit
    xous::terminate_process(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in hello!\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
