# OpenBSD arm64 — Lessons for BeetOS

Reference: `/home/user/openbsd-src/sys/arch/arm64/arm64/`

Compared OpenBSD's arm64 asm (exception.S, cpuswitch.S, locore.S, support.S, trampoline.S, pmap.c, fpu.c)
against BeetOS's `xous/kernel/src/arch/aarch64/`. Key takeaways below, ordered by priority.

---

## Priority 1: `dc zva` page zeroing

OpenBSD uses `dc zva` (data cache zero by VA) to zero pages at cache-line granularity
(typically 64 bytes per `dc zva`). ~4-8x faster than `stp xzr, xzr` loops.

- File: `support.S` → `pagezero_cache` (lines 107-122)
- Query cache line size at boot from DCZID_EL0 register
- Fallback to `stp xzr, xzr` loop if `dc zva` not available (DZP bit)

BeetOS currently uses `core::ptr::write_bytes()` in Rust (boot.rs:186). Should add an
asm `pagezero_fast` routine and call it from the memory manager's page zeroing path.

Effort: ~20 lines of asm. Impact: significant on page fault handling latency.

---

## Priority 2: Spectre-BHB mitigations

OpenBSD implements 6 selectable strategies in `trampoline.S` (lines 72-80, 159-170):
- `spectre_bhb_loop_8/11/24/32/132` — branch thrashing loop to saturate BHB
- `spectre_bhb_psci_hvc/smc` — firmware SMCCC workaround
- `spectre_bhb_clrbhb` — ARMv8.9+ instruction

Pattern (loop_8):
```asm
mov x18, #8
1: b . + 4
   subs x18, x18, #1
   b.ne 1b
   dsb nsh
   isb
```

Applied per-exception on EL0 entry. Critical for Apple M1 (real hardware, has BHB).
Not needed for QEMU virt (emulated CPU), but good practice.

Effort: ~50 lines of asm. Impact: **high security** — prevents speculative execution attacks.

---

## Priority 3: RETGUARD (anti-ROP)

OpenBSD XORs a random cookie into return addresses in asm functions (cpuswitch.S lines 50-51, 78-80).
Pattern:
- `RETGUARD_LOAD_RANDOM(func, x20)` at entry — loads per-function random value
- `RETGUARD_CALC_COOKIE(x15)` — XOR LR with random → store on stack
- `RETGUARD_CHECK` before `ret` — verify cookie matches

Protects against ROP (Return-Oriented Programming) attacks via stack corruption.
Minimal overhead (2-3 instructions per function).

Effort: ~10 lines per protected function. Impact: **high security**.

---

## Priority 4: Lazy FP/NEON save

BeetOS saves all 32 NEON registers (512 bytes) + FPCR/FPSR on every exception = 816 byte frame.
OpenBSD doesn't save FP registers on exception at all — handles them via lazy trap (fpu.c lines 35-154):

1. FPU disabled by default (CPACR_EL1.FPEN = TRAP_ALL)
2. First FP instruction in userspace → trap to kernel → `fpu_load()` enables FP, loads state
3. On context switch → `fpu_save()` only if process has used FP, then disable again

Optimization: set CPACR_EL1 to trap FP access, only save/restore NEON when the process
actually uses FP. Reduces common-case context frame from 816 to ~288 bytes.

Effort: ~100 lines of Rust + minor asm changes. Impact: interrupt latency improvement.

---

## Priority 5: KPTI / Trampoline vectors

OpenBSD hides kernel VA from userspace using trampoline vectors (trampoline.S):
- EL0 exceptions land on a trampoline page (user-visible, separate VA)
- Trampoline switches TTBR1 to kernel page tables
- Jumps to real handler in kernel VA space
- Return path restores user TTBR1

Similar to Linux KPTI. Prevents kernel address leakage via side channels.

Effort: ~200 lines of asm + VBAR switching logic. Impact: medium security.

---

## Priority 6: Explicit DAIF interrupt masking

OpenBSD explicitly saves/restores interrupt state in exception handlers:
```asm
mrs x19, daif       // save current interrupt mask
msr daifset, #3     // mask IRQ + FIQ
...
msr daif, x19       // restore exact prior state
```

BeetOS has no visible DAIF manipulation — relies on implicit interrupt disable across
exception boundary. OpenBSD approach is safer: prevents subtle interrupt re-enable bugs.

Effort: ~5 lines per handler. Impact: correctness.

---

## Lower priority (future milestones)

### Debug single-step
OpenBSD toggles `DBG_MDSCR_SS` bit in `mdscr_el1` on exception entry/exit.
`disable_ss` on entry, `allow_ss` before eret if `PCB_SINGLESTEP` set.
Needed for GDB/debugger support.

### AST (Asynchronous System Traps)
OpenBSD checks `P_ASTPENDING` flag before returning to EL0 (`do_ast` macro in exception.S).
Calls `ast()` handler, loops until no more pending. Pattern for deferred exception/signal delivery.

### Better page table laziness
OpenBSD pmap.c uses hierarchical L0-L3 table allocation on demand, plus reverse
mappings (pte_desc) for phys→virt lookups. BeetOS allocates tables more eagerly.
Would help reduce memory usage for processes with sparse address spaces.

### Large page block mappings
OpenBSD explicitly handles 2MB and 1GB block mappings in pmap. BeetOS doesn't.
Useful for kernel linear map and large DMA buffers.

---

## Key architectural differences summary

| Aspect | OpenBSD | BeetOS | Notes |
|--------|---------|--------|-------|
| Context frame size | ~280 bytes (GPR only) | 816 bytes (GPR + NEON) | BeetOS safer but slower |
| Context switch | asm, callee-saved only | Rust, full frame | BeetOS simpler, OpenBSD faster |
| x18 register | Platform register (scratch) | Normal GPR | BeetOS approach simpler |
| EL1 vs EL0 paths | Separate macros | Unified | BeetOS approach cleaner |
| Exception barriers | `dsb nsh; isb` after handlers | None visible | BeetOS should add these |
| Page granule | 4KB | 16KB | BeetOS correct for Apple M1 |
| FP save strategy | Lazy (trap on access) | Eager (always save) | BeetOS simpler, OpenBSD faster |
| Security hardening | RETGUARD + KPTI + Spectre BHB | W^X only | Significant gap |
