//! Async service demo for BeetOS.
//!
//! Demonstrates `xous-async-rt` capabilities:
//!
//! 1. **Two servers on one thread** — impossible with blocking
//!    `receive_message` without extra threads/stacks.
//! 2. **select with timeout** — handle a message OR timeout, whichever
//!    comes first.
//! 3. **Runtime spawning** — the heartbeat task spawns a one-shot child.
//! 4. **Error handling** — `server.next().await` returns `Result`.

use xous_async_rt::{select, AsyncServer, Either, Executor, Timer};

fn main() {
    let sid_a = xous::create_server().expect("create server A");
    let sid_b = xous::create_server().expect("create server B");

    println!("async-demo: server A = {:?}", sid_a);
    println!("async-demo: server B = {:?}", sid_b);

    let mut executor = Executor::new();
    let spawner = executor.spawner();

    // Task 1: handle messages on server A with a timeout
    executor.spawn(async move {
        let mut server = AsyncServer::new(sid_a);
        loop {
            match select(server.next(), Timer::after(500)).await {
                Either::Left(Ok(envelope)) => {
                    println!("  [A] message: {:?}", envelope.body);
                }
                Either::Left(Err(e)) => {
                    println!("  [A] server error: {}", e);
                    break;
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
            match server.next().await {
                Ok(envelope) => println!("  [B] message: {:?}", envelope.body),
                Err(e) => {
                    println!("  [B] error: {}", e);
                    break;
                }
            }
        }
    });

    // Task 3: periodic heartbeat that spawns a child task
    executor.spawn(async move {
        let mut count: u64 = 0;
        loop {
            Timer::after(200).await;
            count += 1;
            println!("  [heartbeat] tick #{}", count);

            // Demonstrate runtime spawning: spawn a one-shot task
            if count == 3 {
                let s = spawner.clone();
                s.spawn(async {
                    println!("  [dynamic] spawned at runtime, running once");
                });
            }
        }
    });

    println!("async-demo: starting executor (3 tasks, 1 thread)...");
    executor.run();
}
