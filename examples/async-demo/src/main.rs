//! Async service demo for BeetOS.
//!
//! Demonstrates `xous-async-rt` capabilities:
//!
//! 1. **Two servers on one thread** — impossible with blocking
//!    `receive_message` without extra threads/stacks.
//! 2. **select with timeout** — handle a message OR timeout, whichever
//!    comes first.
//! 3. **join** — drive independent operations concurrently.
//! 4. **Reactor-based scheduling** — only woken tasks are polled,
//!    idle CPU is yielded to other Xous processes.

use xous_async_rt::{select, AsyncServer, Either, Executor, Timer};

fn main() {
    let sid_a = xous::create_server().expect("create server A");
    let sid_b = xous::create_server().expect("create server B");

    println!("async-demo: server A = {:?}", sid_a);
    println!("async-demo: server B = {:?}", sid_b);

    let mut executor = Executor::new();

    // Task 1: handle messages on server A with a timeout
    executor.spawn(async move {
        let mut server = AsyncServer::new(sid_a);
        loop {
            match select(server.next(), Timer::after(500)).await {
                Either::Left(envelope) => {
                    println!("  [A] message: {:?}", envelope.body);
                }
                Either::Right(()) => {
                    println!("  [A] timeout — no message in 500 ticks");
                }
            }
        }
    });

    // Task 2: handle messages on server B
    executor.spawn(async move {
        let mut server = AsyncServer::new(sid_b);
        loop {
            let envelope = server.next().await;
            println!("  [B] message: {:?}", envelope.body);
        }
    });

    // Task 3: periodic heartbeat
    executor.spawn(async {
        let mut count: u64 = 0;
        loop {
            Timer::after(200).await;
            count += 1;
            println!("  [heartbeat] tick #{}", count);
        }
    });

    println!("async-demo: starting executor (3 tasks, 1 thread)...");
    executor.run();
}
