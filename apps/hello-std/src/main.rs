// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS "hello-std" — proof that alloc (Box, Vec, String, BTreeMap, format!) works on BeetOS.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, collections::BTreeMap, format, string::String, vec, vec::Vec};
use core::panic::PanicInfo;

// ============================================================================
// Heap allocator — uses Xous IncreaseHeap syscall
// ============================================================================

mod heap {
    use core::alloc::{GlobalAlloc, Layout};
    use core::sync::atomic::{AtomicUsize, Ordering};

    pub struct BumpAllocator {
        next: AtomicUsize,
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
            let page_size = 16384;
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
                    if self
                        .next
                        .compare_exchange(next, new_next, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        return aligned as *mut u8;
                    }
                    continue;
                }

                if !self.grow(layout.size() + layout.align()) {
                    return core::ptr::null_mut();
                }
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
    let uart_base: usize;
    unsafe {
        core::arch::asm!(
            "mov {0}, x0",
            out(reg) uart_base,
            options(nomem, nostack),
        );
        UART_BASE = uart_base;
    }

    let pid = match xous::rsyscall(xous::SysCall::GetProcessId) {
        Ok(xous::Result::ProcessID(pid)) => pid.get() as usize,
        _ => 0,
    };

    puts("Hello, BeetOS!\n");
    puts("I am PID ");
    put_usize(pid);
    puts(", running at EL0.\n");

    // Test Box
    puts("Creating Box<u64>...\n");
    let b: Box<u64> = Box::new(42);
    puts("Box = ");
    put_usize(*b as usize);
    puts(" at ");
    put_hex(&*b as *const u64 as usize);
    puts("\n");

    // Test Vec
    puts("Creating Vec...\n");
    let v: Vec<u32> = vec![1, 2, 3, 4, 5];
    puts("Vec len=");
    put_usize(v.len());
    puts(" [");
    for (i, val) in v.iter().enumerate() {
        if i > 0 {
            puts(", ");
        }
        put_usize(*val as usize);
    }
    puts("]\n");

    // Test String
    puts("Creating String...\n");
    let s = String::from("alloc works on BeetOS!");
    puts(&s);
    puts("\n");

    // Test format!
    puts("Testing format!...\n");
    let msg = format!("{} + {} = {}", 40, 2, 40 + 2);
    puts(&msg);
    puts("\n");

    // Test BTreeMap (alloc's ordered map — no std needed)
    puts("Creating BTreeMap...\n");
    let mut map: BTreeMap<&str, i32> = BTreeMap::new();
    map.insert("alloc", 1);
    map.insert("BTreeMap", 2);
    map.insert("String", 3);
    map.insert("format!", 4);
    map.insert("Vec", 5);
    puts("BTreeMap len=");
    put_usize(map.len());
    puts("\n");

    // BTreeMap keys are already sorted
    puts("Keys: [");
    for (i, k) in map.keys().enumerate() {
        if i > 0 {
            puts(", ");
        }
        puts(k);
    }
    puts("]\n");

    puts("[done]\n");

    xous::terminate_process(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in hello-std!\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
