# AArch64 Architecture Notes — OpenBSD & Linux vs BeetOS

Reference sources:
- OpenBSD: `/home/user/openbsd-src/sys/arch/arm64/arm64/`
- Linux: `/home/user/refs/linux/arch/arm64/`
- BeetOS: `xous/kernel/src/arch/aarch64/`

Key takeaways below, ordered by priority for BeetOS.

---

## Priority 1: `dc zva` page zeroing

Both OpenBSD and Linux use `dc zva` (data cache zero by VA) to zero pages at cache-line
granularity (typically 64 bytes per `dc zva`). ~4-8x faster than `stp xzr, xzr` loops.

**OpenBSD** — `support.S` → `pagezero_cache` (lines 107-122):
- Query cache line size at boot from DCZID_EL0
- Fallback to `stp xzr, xzr` loop if `dc zva` not available (DZP bit)

**Linux** — `arch/arm64/lib/clear_page.S` → `clear_page()`:
- Same approach: DC ZVA in a tight loop, cache-line aligned
- Used transparently by the page allocator's zeroing path

**BeetOS** currently uses `core::ptr::write_bytes()` in Rust (boot.rs:186). Should add an
asm `pagezero_fast` routine and call it from the memory manager's page zeroing path.

Effort: ~20 lines of asm. Impact: significant on page fault handling latency.

---

## Priority 2: Spectre-BHB mitigations

**OpenBSD** — `trampoline.S` (lines 72-80, 159-170), 6 strategies:
- `spectre_bhb_loop_8/11/24/32/132` — branch thrashing loop to saturate BHB
- `spectre_bhb_psci_hvc/smc` — firmware SMCCC workaround
- `spectre_bhb_clrbhb` — ARMv8.9+ instruction
- Fixed strategy selected at boot, no runtime patching

Pattern (loop_8):
```asm
mov x18, #8
1: b . + 4
   subs x18, x18, #1
   b.ne 1b
   dsb nsh
   isb
```

**Linux** — `proton-pack.c` + `entry.S` (lines 679-750), 4 vector variants:
- `BHB_MITIGATION_LOOP` — CPU-specific K value (K=8..132 depending on core)
- `BHB_MITIGATION_FW` — SMC/HVC to ARM_SMCCC_ARCH_WORKAROUND_3
- `BHB_MITIGATION_INSN` — `clearbhb` instruction (ARMv8.9+)
- `BHB_MITIGATION_NONE` — safe CPUs (CSV2.3 or ECBHB)
- **Runtime vector patching** via `alternative` mechanism — patches vectors at boot per CPU
- CPU-specific K values: Cortex-A57/A72=8, A77/N1=24, A78/A710/X2=32, X3/V2=132

Linux is more sophisticated: dynamic patching per CPU model. OpenBSD uses fixed strategies.

Applied per-exception on EL0 entry. Critical for Apple M1 (real hardware, has BHB).

Effort: ~50 lines of asm (fixed strategy like OpenBSD). Impact: **high security**.

---

## Priority 3: RETGUARD / return hardening

**OpenBSD** — `cpuswitch.S` (lines 50-51, 78-80), compiler-integrated RETGUARD:
- `RETGUARD_LOAD_RANDOM(func, x20)` at entry — loads per-function random value
- `RETGUARD_CALC_COOKIE(x15)` — XOR LR with random → store on stack
- `RETGUARD_CHECK` before `ret` — verify cookie matches
- Minimal overhead (2-3 instructions per function)

**Linux** — no direct RETGUARD equivalent, but uses:
- **Shadow Call Stack (SCS)**: `scs_save` / `scs_load_current` in `cpu_switch_to`
  - Stores return addresses on a separate, hidden stack
  - Hardware-assisted via x18 as SCS pointer (dedicated platform register)
- **Pointer Authentication (PAuth)**: `ptrauth_keys_install_kernel` in context switch
  - Signs/verifies return addresses with per-task PAC keys
  - Transparent to most code when enabled via compiler flag

Linux's approach is stronger (hardware-assisted). OpenBSD's RETGUARD is software-only but
works on all cores. BeetOS should consider SCS if targeting PAuth-capable cores (M1 has PAuth).

Effort: ~10 lines per protected function (RETGUARD), or ~50 lines for SCS infra.
Impact: **high security** — prevents ROP attacks.

---

## Priority 4: Lazy FP/NEON save

BeetOS saves all 32 NEON registers (512 bytes) + FPCR/FPSR on every exception = 816 byte frame.

**OpenBSD** — `fpu.c` (lines 35-154), simple lazy trap:
1. FPU disabled by default (CPACR_EL1.FPEN = TRAP_ALL)
2. First FP instruction in userspace → trap to kernel → `fpu_load()` enables FP
3. On context switch → `fpu_save()` only if process used FP, then disable again

**Linux** — `fpsimd.c`, sophisticated lazy evaluation with per-CPU tracking:
1. On context switch: don't restore FPSIMD. Set `TIF_FOREIGN_FPSTATE` flag.
2. On return to userspace: if `TIF_FOREIGN_FPSTATE` set, load from memory.
3. **Optimization**: if same task returns to same CPU and no kernel NEON ran, skip load entirely
   (tracks `fpsimd_last_state` per CPU).
4. **SVE/SME support**: distinguishes `FP_STATE_FPSIMD` (V0-V31 only) vs `FP_STATE_SVE`
   (full Z/P/FFR registers). Syscall ABI clears SVE state.
5. **Kernel NEON**: `kernel_neon_begin()`/`kernel_neon_end()` — save user state, set
   `TIF_KERNEL_FPSTATE`, allow NEON in softirq/preemptible context.

Linux's approach is the most optimized — avoids save/restore entirely when state is still live.
OpenBSD is simpler but still much better than eager save. BeetOS should start with OpenBSD's
approach (simple lazy trap) and evolve toward Linux's per-CPU tracking if needed.

Effort: ~100 lines of Rust + minor asm changes. Impact: interrupt latency improvement.

---

## Priority 5: KPTI / Trampoline vectors

**OpenBSD** — `trampoline.S`:
- EL0 exceptions land on a trampoline page (user-visible, separate VA)
- Trampoline switches TTBR1 to kernel page tables
- Return path restores user TTBR1

**Linux** — `entry.S` (lines 622-652, 704-724) + `proc.S` (lines 266-400+):
- **Dual page tables**: `swapper_pg_dir` (full kernel) vs `tramp_pg_dir` (minimal, vectors only)
- `tramp_map_kernel()` / `tramp_unmap_kernel()` macros switch TTBR1 at EL0 boundary
- **NG bit**: marks non-global PTEs with `PTE_NG` so TLB doesn't cache them across contexts
- **Break-before-make**: DSB between clearing and setting temp PTEs (TLB coherency)
- **Stop-machine**: CPU 0 waits for all secondaries to ack KPTI before proceeding

Linux's implementation is more robust (NG bits, break-before-make, multi-CPU coordination).
BeetOS is single-core for now, so OpenBSD's simpler approach suffices initially.

Effort: ~200 lines of asm + VBAR switching logic. Impact: medium security.

---

## Priority 6: Explicit DAIF interrupt masking

**OpenBSD** — explicit save/restore in every exception handler:
```asm
mrs x19, daif       // save current interrupt mask
msr daifset, #3     // mask IRQ + FIQ
...
msr daif, x19       // restore exact prior state
```

**Linux** — more nuanced, uses GIC PMR for pseudo-NMI:
- EL1 exceptions: disable all interrupts via DAIF
- EL0 exceptions: restore per-CPU GIC PMR via `ICC_PMR_EL1` (allows pseudo-NMI handling)
- `save_and_disable_daif` / `restore_irq` macros in `cpu_switch_to`
- Stack overflow detection uses TPIDR_EL0 bit trick (lines 59-94) before touching DAIF

BeetOS has no visible DAIF manipulation — relies on implicit interrupt disable across
exception boundary. Both OpenBSD and Linux explicitly manage DAIF. BeetOS should add this.

Effort: ~5 lines per handler. Impact: correctness.

---

## Priority 7: Context switch design

**OpenBSD** — `cpuswitch.S`:
- Saves callee-saved registers only (x19-x29, sp, lr)
- ~280 bytes context frame (GPR only, no NEON)
- RETGUARD on entry/exit

**Linux** — `entry.S` → `cpu_switch_to` (lines 823-850) + `process.c` → `__switch_to`:
- Assembly: save callee-saved x19-x29, sp, lr only (~200 bytes)
- C orchestrator does: FPSIMD switch, TLS (TPIDR_EL0/TPIDR2_EL0), hw breakpoints,
  Spectre v4 per-task SSBS toggle, MTE tag check logging
- **SCTLR_EL1 optimization**: skip write if value unchanged (expensive register)
- **DSB barrier** before switch to ensure all side effects visible
- **sp_el0 = task_struct**: Linux stores current task pointer in sp_el0

**BeetOS** — Rust, full frame save (GPR + NEON = 816 bytes):
- Simpler but slower. Once lazy FP is added, frame drops to ~288 bytes.

---

## Lower priority (future milestones)

### Debug single-step
- **OpenBSD**: toggles `DBG_MDSCR_SS` in `mdscr_el1` on exception entry/exit
- **Linux**: debug-monitors.c, software step management via `user_enable_single_step()`
- Needed for GDB/debugger support

### AST (Asynchronous System Traps) / TIF flags
- **OpenBSD**: checks `P_ASTPENDING` before returning to EL0 (`do_ast` macro)
- **Linux**: checks `TIF_NEED_RESCHED`, `TIF_SIGPENDING`, `TIF_NOTIFY_RESUME` in `ret_to_user`
- Pattern for deferred signal/preemption delivery

### Better page table laziness
- Both OpenBSD (`pmap.c`) and Linux use on-demand L0-L3 table allocation
- Linux adds NG bit management and break-before-make discipline
- BeetOS allocates tables more eagerly — could save memory for sparse address spaces

### Large page block mappings
- Both OpenBSD and Linux handle 2MB and 1GB block mappings
- Linux: `PMD_TYPE_SECT` / `PUD_TYPE_SECT` in `pgtable-hwdef.h`
- Useful for kernel linear map and large DMA buffers

### Pointer Authentication (PAuth)
- **Linux only**: `ptrauth_keys_install_kernel` on context switch
- Per-task PAC keys rotated; signs return addresses and data pointers
- M1 supports PAuth — BeetOS could leverage for additional ROP protection

### Shadow Call Stack (SCS)
- **Linux only**: `scs_save` / `scs_load_current` in `cpu_switch_to`
- Dedicated x18 register as SCS pointer
- Separate stack for return addresses, invisible to buffer overflows

---

## Key architectural differences summary

| Aspect | OpenBSD | Linux | BeetOS | Notes |
|--------|---------|-------|--------|-------|
| Context frame size | ~280 bytes (GPR only) | ~200 bytes (callee-saved) | 816 bytes (GPR + NEON) | BeetOS safest but slowest |
| Context switch | asm, callee-saved | asm + C orchestrator | Rust, full frame | Linux most featureful |
| FP save strategy | Lazy (trap on access) | Lazy (TIF_FOREIGN_FPSTATE + per-CPU tracking) | Eager (always save) | Linux most optimized |
| Spectre BHB | Fixed strategy at boot | Runtime vector patching per CPU | None | Significant gap |
| Return hardening | RETGUARD (software XOR cookie) | SCS + PAuth (hardware) | None | Significant gap |
| KPTI | Trampoline vectors | Dual TTBR1 + NG bits + break-before-make | None | Security gap |
| DAIF masking | Explicit save/restore | GIC PMR pseudo-NMI + DAIF | Implicit only | Correctness gap |
| x18 register | Platform register (scratch) | SCS pointer | Normal GPR | BeetOS simplest |
| Exception barriers | `dsb nsh; isb` after handlers | DSB + ISB + per-CPU barriers | None visible | BeetOS should add |
| Page granule | 4KB | 4KB/16KB/64KB configurable | 16KB | BeetOS correct for M1 |
| Page zeroing | `dc zva` | `dc zva` | `write_bytes()` | Easy win |
| Security hardening | RETGUARD + KPTI + Spectre BHB | SCS + PAuth + KPTI + Spectre BHB | W^X only | Biggest gap |
