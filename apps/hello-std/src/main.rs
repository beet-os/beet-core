// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS "hello-std" — proof that Rust std works on BeetOS.
//!
//! Uses println! via the log server (IPC → UART), no direct UART access.

use std::collections::HashMap;

fn main() {
    println!("Hello, BeetOS!");

    // Test Box
    println!("Creating Box<u64>...");
    let b: Box<u64> = Box::new(42);
    println!("Box = {} at {:p}", *b, &*b);

    // Test String
    println!("Creating String...");
    let s = String::from("std works on BeetOS!");
    println!("{}", s);

    // Test Vec
    println!("Creating Vec...");
    let v: Vec<u32> = vec![1, 2, 3, 4, 5];
    println!("Vec len={} {:?}", v.len(), v);

    // Test format!
    println!("Testing format!...");
    let msg = format!("Formatted: {} + {} = {}", 40, 2, 42);
    println!("{}", msg);

    // Test HashMap
    println!("Creating HashMap...");
    let mut map: HashMap<&str, i32> = HashMap::new();
    map.insert("std", 1);
    map.insert("HashMap", 2);
    map.insert("String", 3);
    map.insert("format!", 4);
    map.insert("Vec", 5);
    println!("HashMap len={}", map.len());

    let mut keys: Vec<&&str> = map.keys().collect();
    keys.sort();
    println!("Keys: {:?}", keys);

    println!("[done]");
}
