# Kernel benchmarks (`cargo xtask qemu-bench`)

Regression gate for the kernel's hot paths. Run it before and after
any change to syscall dispatch, the scheduler, IPC, or memory
management — it tells you in ~3 minutes whether you made the kernel
slower, with deterministic numbers you can quote in a commit message.

```bash
cargo xtask qemu-bench                  # compare against the baseline
cargo xtask qemu-bench --update         # rewrite the baseline (intentional change)
cargo xtask qemu-bench --tolerance 10   # tighter gate (default ±30%)
```

The baseline lives in `xtask/qemu-bench-baseline.txt` and is checked
in. CI fails the gate when any benchmark regresses beyond tolerance;
improvements beyond tolerance print a "consider --update" note but
pass.

## How it works

- The shell has a `bench` builtin that runs each micro-benchmark in a
  loop and times it with **CNTVCT_EL0**, the ARM virtual counter. The
  kernel enables EL0 reads at boot (`CNTKCTL_EL1.EL0VCTEN`, see
  `platform/qemu_virt/timer.rs::init`), so sampling costs no syscall.
  Userspace reads it through `xous::arch::perf::{counter, frequency}`
  (hosted builds get a monotonic-ns stand-in, so the code compiles
  everywhere).
- xtask boots QEMU with **`-icount shift=0,sleep=off`**: one guest
  instruction = one virtual nanosecond. Timing becomes a deterministic
  function of the instruction trace — independent of host load, host
  CPU, or how many CI jobs run next to you. Two consecutive runs agree
  within ±0.1%. (`sleep=off` lets idle WFI warp virtual time forward,
  so boot doesn't take minutes of wall clock.)
- xtask drives the `bench` command over the TCP remote console (port
  2323), parses the `[bench] <name> <ns> ns/op n=<iters>` lines, and
  compares. Don't change that output format without updating the
  parser.

Because 1 ns = 1 instruction under icount, **read every number as an
instruction count**. That's often more useful than time: "a null
syscall is ~3.4k instructions in a debug build" transfers across
machines; microseconds don't.

## The benchmarks

| name | what it measures | notes |
|---|---|---|
| `cpu_mix` | pure-CPU integer loop | **Calibration point.** No kernel involvement at all — if it moves, the *measurement environment* changed (QEMU version, icount config), not the kernel. Investigate the harness, not your diff. |
| `syscall_null` | `GetThreadId` round-trip | The EL0→EL1→EL0 floor: vector entry, context save/restore to the process table, dispatch, result marshalling. Everything else pays at least this. |
| `yield` | `yield_slice()` with nothing else runnable | Scheduler rotate + `activate_current` fast path. Healthy value ≈ `syscall_null` + ~1.1k. |
| `ipc_scalar` | `BlockingScalar` to the fs service | Full IPC round-trip: sender blocks, server wakes, replies, sender wakes. Two context switches + message plumbing. |
| `map_unmap` | `MapMemory` + touch + `UnmapMemory`, one 16 KiB page | Dominated (~90%) by the **synchronous page zeroing on the dirty path**: the tight loop starves the background zeroer (it runs at idle, the loop never idles), so almost every iteration takes the "give out a dirty page" path and memsets 16 KiB inline. That's the realistic sustained-allocation worst case — but don't read the number as "MMU cost". |
| `memcpy_16k` | 16 KiB copy between mapped pages | Memory-subsystem constant; second calibration point. |

All numbers are **debug-build** values (`cargo xtask build` uses the
dev profile): roughly 10× a release build, with ratios distorted
accordingly. That's fine for a regression gate — it compares debug to
debug, deterministically — but don't quote them as the kernel's real
performance.

## Pitfall: in-guest timing vs. the GUI (read this before adding a benchmark)

The bench boots QEMU **without `-device ramfb` — on purpose**. With a
framebuffer present, the kernel repaints the boot desktop at 10 Hz
from the idle loop, and one full software compose of 1280×800 costs
~90 ms of virtual time in a debug build.

Before the compose was moved out of IRQ context, this produced a
spectacular measurement artifact: any benchmark window crossing a
10 Hz boundary absorbed one or more full composes. `yield` read
43.5k ns/op when its true cost was 4.5k — and because icount is
deterministic, the corrupted numbers were *perfectly reproducible*,
which made them look trustworthy. The giveaway was the pattern, not
the variance: benchmarks alternated cheap/expensive by **position**,
and the same `GetThreadId` loop benched three times in a row read
3.4k / 42.3k / 3.4k.

Rules that follow:

1. **Any in-guest timing must either boot headless or account for the
   10 Hz idle compose.** The compose runs only when the system idles,
   so a CPU-bound measured loop is safe *since the deferred-compose
   change* — but boundary work (prints, IPC) yields, and the catch-up
   compose lands somewhere. Headless is the only clean configuration.
2. **Suspicious numbers: bisect end-to-end, don't trust fine-grained
   counters.** Under icount, counter reads inside a translation block
   can be rounded/stale — section timings inside the kernel measured
   "275 ticks" while the true cost was 2,700. Removing code and
   re-running the full bench (integrated over 10k iterations) gives
   reliable deltas; sprinkling CNTPCT reads does not.
3. **Bench the same call twice in a row** when a number looks odd. If
   the two readings differ wildly, the cost is positional (something
   periodic landing in one window) — not the code you're staring at.

## Adding a benchmark

1. Add a block to `cmd_bench` in `apps/shell/src/main.rs`, print via
   `bench_report(name, total_ticks, freq, iters)`. Keep the loop free
   of prints and IPC (boundary effects — see above).
2. Run `cargo xtask qemu-bench --update` to extend the baseline.
3. The gate fails on names present in only one of baseline/results,
   so baseline and code can't drift apart silently.
