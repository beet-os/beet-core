//! Async service demo for BeetOS.
//!
//! Shows how xous-async-rt lets a single thread handle messages from
//! **two independent servers** concurrently — something impossible with
//! blocking `xous::receive_message()` without spawning extra threads.
//!
//! ## What this demonstrates
//!
//! Traditional (blocking) Xous service — one server per thread:
//!
//! ```rust,no_run
//! // Thread 1
//! loop { let msg = xous::receive_message(server_a)?; handle_a(msg); }
//! // Thread 2  (needs a whole new stack, PID slot, etc.)
//! loop { let msg = xous::receive_message(server_b)?; handle_b(msg); }
//! ```
//!
//! Async Xous service — multiple servers, single thread:
//!
//! ```rust,no_run
//! let mut exec = Executor::new();
//! exec.spawn(async { loop { let msg = server_a.next().await; handle_a(msg); } });
//! exec.spawn(async { loop { let msg = server_b.next().await; handle_b(msg); } });
//! exec.run(); // one thread drives both
//! ```

fn main() {
    // --- Create two independent Xous servers ---
    let sid_a = xous::create_server().expect("create server A");
    let sid_b = xous::create_server().expect("create server B");

    println!("async-demo: server A = {:?}", sid_a);
    println!("async-demo: server B = {:?}", sid_b);

    // --- Build the executor and spawn async tasks ---
    let mut executor = xous_async_rt::Executor::new();

    // Task 1: handle messages on server A
    executor.spawn(async move {
        let mut server = xous_async_rt::AsyncServer::new(sid_a);
        loop {
            let envelope = server.next().await;
            println!("  [A] received: {:?}", envelope.body);
        }
    });

    // Task 2: handle messages on server B
    executor.spawn(async move {
        let mut server = xous_async_rt::AsyncServer::new(sid_b);
        loop {
            let envelope = server.next().await;
            println!("  [B] received: {:?}", envelope.body);
        }
    });

    // Task 3: a periodic "heartbeat" using the async timer
    executor.spawn(async {
        loop {
            xous_async_rt::Timer::after_ms(100).await;
            println!("  [heartbeat] tick");
        }
    });

    // --- Run the executor (drives all three tasks on one thread) ---
    println!("async-demo: starting executor with 3 concurrent tasks...");
    executor.run();
}
