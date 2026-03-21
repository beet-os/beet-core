// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS "hello-std" — proof that Rust std works on BeetOS.

#![no_main]

const UART_VA: usize = 0x0000_0010_0100_0000;

fn putc(c: u8) {
    unsafe {
        let base = UART_VA;
        while (core::ptr::read_volatile((base + 0x18) as *const u32) & (1 << 5)) != 0 {}
        if c == b'\n' {
            core::ptr::write_volatile(base as *mut u32, b'\r' as u32);
            while (core::ptr::read_volatile((base + 0x18) as *const u32) & (1 << 5)) != 0 {}
        }
        core::ptr::write_volatile(base as *mut u32, c as u32);
    }
}

fn puts(s: &str) { for b in s.bytes() { putc(b); } }
fn put_hex(n: usize) {
    puts("0x");
    for i in (0..16).rev() {
        let d = ((n >> (i * 4)) & 0xf) as u8;
        putc(if d < 10 { b'0' + d } else { b'a' + d - 10 });
    }
}
fn put_usize(mut n: usize) {
    if n == 0 { putc(b'0'); return; }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 { buf[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
    while i > 0 { i -= 1; putc(buf[i]); }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> u32 {
    puts("Hello, BeetOS!\n");

    // Test Box
    puts("Creating Box<u64>...\n");
    let b: Box<u64> = Box::new(42);
    puts("Box = ");
    put_usize(*b as usize);
    puts(" at ");
    put_hex(&*b as *const u64 as usize);
    puts("\n");

    // Test String
    puts("Creating String...\n");
    let s = String::from("std works on BeetOS!");
    puts(&s);
    puts("\n");

    // Test Vec
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

    // Test format!
    puts("Testing format!...\n");
    let msg = format!("Formatted: {} + {} = {}", 40, 2, 42);
    puts(&msg);
    puts("\n");

    // Test HashMap
    puts("Creating HashMap...\n");
    use std::collections::HashMap;
    let mut map: HashMap<&str, i32> = HashMap::new();
    map.insert("std", 1);
    map.insert("HashMap", 2);
    map.insert("String", 3);
    map.insert("format!", 4);
    map.insert("Vec", 5);
    puts("HashMap len=");
    put_usize(map.len());
    puts("\n");

    // Collect and sort keys
    let mut keys: Vec<&&str> = map.keys().collect();
    keys.sort();
    puts("Keys: [");
    for (i, k) in keys.iter().enumerate() {
        if i > 0 { puts(", "); }
        puts(k);
    }
    puts("]\n");

    puts("[done]\n");
    0
}
