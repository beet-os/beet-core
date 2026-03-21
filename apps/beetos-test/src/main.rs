// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS self-test suite — run via `cargo xtask test`.
//!
//! Prints `PASS: <name>` or `FAIL: <name>` for each test, then either
//! `ALL TESTS PASSED (N/N)` or `SOME TESTS FAILED (P/T passed)`.
//! `cargo xtask test` launches QEMU with piped stdout and parses these lines.

use std::collections::HashMap;

fn main() {
    println!("BeetOS test suite starting...");

    let mut passed = 0u32;
    let mut failed = 0u32;

    macro_rules! check {
        ($name:expr, $expr:expr) => {
            if $expr {
                println!("PASS: {}", $name);
                passed += 1;
            } else {
                println!("FAIL: {}", $name);
                failed += 1;
            }
        };
    }

    // --- TLS ---
    thread_local! {
        static TLS_VAL: std::cell::Cell<u32> = std::cell::Cell::new(42);
    }

    check!("tls-basic", TLS_VAL.with(|v| v.get()) == 42);
    TLS_VAL.with(|v| v.set(100));
    check!("tls-mutation", TLS_VAL.with(|v| v.get()) == 100);

    // --- Heap: Box ---
    let b: Box<u64> = Box::new(0xDEAD_BEEF_1234_5678);
    check!("heap-box", *b == 0xDEAD_BEEF_1234_5678);

    // --- Heap: Vec ---
    let v: Vec<u32> = (0..50).collect();
    check!("heap-vec-len", v.len() == 50);
    check!("heap-vec-index", v[49] == 49);

    // --- Heap: String ---
    let s = String::from("hello BeetOS");
    check!("heap-string", s == "hello BeetOS");

    // --- format! ---
    let msg = format!("{} + {} = {}", 40, 2, 42);
    check!("format-basic", msg == "40 + 2 = 42");

    // --- HashMap ---
    let mut map: HashMap<&str, i32> = HashMap::new();
    map.insert("alpha", 1);
    map.insert("beta", 2);

    check!("hashmap-insert", map.get("alpha") == Some(&1));
    check!("hashmap-len", map.len() == 2);
    check!("hashmap-missing", map.get("gamma").is_none());

    // Summary
    println!();

    if failed == 0 {
        println!("ALL TESTS PASSED ({}/{})", passed, passed);
    } else {
        println!("SOME TESTS FAILED ({}/{} passed)", passed, passed + failed);
    }
}
