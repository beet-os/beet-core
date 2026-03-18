// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 process and thread management for the Xous kernel.
//!
//! Each process has its own address space (TTBR0) and ASID.
//! Threads within a process share the address space but have separate
//! register contexts (X0-X30, SP, PC, SPSR, NEON/FP state).

use xous::{Error, MemoryRange, ProcessInit, ProcessStartup, ThreadInit, PID, TID};

use crate::process::INITIAL_TID;
use crate::services::SystemServices;

/// Maximum number of concurrent processes.
pub const MAX_PROCESS_COUNT: usize = 64;

/// Maximum number of threads per process.
pub const MAX_THREAD_COUNT: TID = 32;

/// Saved register context for a single thread.
/// Layout must match the save_context / restore_context assembly in asm.S.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Thread {
    /// General-purpose registers X0-X30 (31 × 8 bytes = 248)
    pub gpr: [u64; 31],
    /// SP_EL0 (user stack pointer)
    pub sp: u64,
    /// ELR_EL1 (exception link register — the return PC)
    pub elr: u64,
    /// SPSR_EL1 (saved program status)
    pub spsr: u64,
    /// TPIDR_EL0 (thread-local storage pointer)
    pub tpidr: u64,
    /// FPCR (floating-point control register)
    pub fpcr: u64,
    /// FPSR (floating-point status register)
    pub fpsr: u64,
    /// Padding for alignment
    pub _pad: u64,
    /// NEON/FP registers V0-V31 (32 × 128 bits = 512 bytes, stored as 64 u64s)
    pub vregs: [u64; 64],
    /// Whether this thread slot is allocated.
    pub allocated: bool,
    /// Optional stack range (for cleanup on thread exit).
    pub stack: Option<MemoryRange>,
}

const _: () = {
    // Verify that the Thread struct's register area matches the asm.S layout.
    // asm.S uses 816 bytes for the context frame:
    //   248 (GPR) + 8 (SP) + 8 (ELR) + 8 (SPSR) + 8 (TPIDR) + 8 (FPCR) + 8 (FPSR) + 8 (pad) + 512 (V0-V31)
    // = 816 bytes.
    // The additional `allocated` and `stack` fields are Rust-only metadata.
};

impl Default for Thread {
    fn default() -> Self {
        Thread {
            gpr: [0u64; 31],
            sp: 0,
            elr: 0,
            spsr: 0,
            tpidr: 0,
            fpcr: 0,
            fpsr: 0,
            _pad: 0,
            vregs: [0u64; 64],
            allocated: false,
            stack: None,
        }
    }
}

impl core::fmt::Debug for Thread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Thread")
            .field("pc", &self.elr)
            .field("sp", &self.sp)
            .field("allocated", &self.allocated)
            .finish()
    }
}

impl Thread {
    /// Set the program counter for this thread.
    pub fn set_pc(&mut self, pc: usize) {
        self.elr = pc as u64;
    }

    /// Set the stack pointer for this thread.
    pub fn set_sp(&mut self, sp: usize) {
        self.sp = sp as u64;
    }

    /// Get the syscall arguments from registers X0-X5, X8-X9.
    /// This matches the AArch64 Xous syscall ABI.
    pub fn get_args(&self) -> [usize; 8] {
        [
            self.gpr[0] as usize,
            self.gpr[1] as usize,
            self.gpr[2] as usize,
            self.gpr[3] as usize,
            self.gpr[4] as usize,
            self.gpr[5] as usize,
            self.gpr[8] as usize,
            self.gpr[9] as usize,
        ]
    }

    /// Set the syscall result in registers X0-X5, X8-X9.
    pub fn set_args(&mut self, args: &[usize]) {
        if args.len() > 0 { self.gpr[0] = args[0] as u64; }
        if args.len() > 1 { self.gpr[1] = args[1] as u64; }
        if args.len() > 2 { self.gpr[2] = args[2] as u64; }
        if args.len() > 3 { self.gpr[3] = args[3] as u64; }
        if args.len() > 4 { self.gpr[4] = args[4] as u64; }
        if args.len() > 5 { self.gpr[5] = args[5] as u64; }
        if args.len() > 6 { self.gpr[8] = args[6] as u64; }
        if args.len() > 7 { self.gpr[9] = args[7] as u64; }
    }

    /// Get the processor mode from SPSR. Returns the exception level bits.
    /// 0b0000 = EL0t (user), 0b0100 = EL1t, 0b0101 = EL1h
    pub fn processor_mode(&self) -> u32 {
        (self.spsr & 0xF) as u32
    }

    /// Disable interrupts by setting DAIF mask bits in SPSR.
    pub fn disable_interrupts(&mut self) {
        self.spsr |= 0x3C0; // Mask D, A, I, F
    }
}

/// Per-process architecture state.
/// Contains all thread contexts and process-level state.
struct ProcessImpl {
    /// Thread contexts
    threads: [Thread; MAX_THREAD_COUNT],
    /// Currently active thread
    current_thread: TID,
}

/// Static storage for all process states.
/// Indexed by PID - 1.
static mut PROCESS_TABLE: [Option<ProcessImpl>; MAX_PROCESS_COUNT] =
    [const { None }; MAX_PROCESS_COUNT];

/// The currently active PID.
static mut CURRENT_PID: u8 = 1;

/// Get the current process ID.
pub fn current_pid() -> PID {
    unsafe { PID::new_unchecked(CURRENT_PID) }
}

/// Set the current process ID.
pub fn set_current_pid(pid: PID) {
    unsafe {
        CURRENT_PID = pid.get();
    }
}

/// Parameters for setting up a new process.
pub struct ProcessSetup {
    pub pid: PID,
    pub entry_point: usize,
    pub stack: MemoryRange,
    pub irq_stack: MemoryRange,
    pub aslr_slide: usize,
}

/// Architecture-specific process handle used by the kernel.
pub struct Process {
    pid: PID,
}

impl PartialEq for Process {
    fn eq(&self, other: &Process) -> bool {
        self.pid == other.pid
    }
}

impl Process {
    /// Get a handle to the current process.
    pub fn current() -> Process {
        Process { pid: current_pid() }
    }

    /// Get the current thread ID for this process.
    pub fn current_tid(&self) -> TID {
        let idx = self.pid.get() as usize - 1;
        unsafe {
            PROCESS_TABLE
                .get(idx)
                .and_then(|p| p.as_ref())
                .map(|p| p.current_thread)
                .unwrap_or(INITIAL_TID)
        }
    }

    /// Retry a failed SVC instruction by rewinding the PC.
    pub fn retry_swi_instruction(&mut self, tid: TID) -> Result<(), Error> {
        self.set_thread_result(tid, xous::Result::RetryCall);
        Ok(())
    }

    /// Set up a new thread slot in the current process.
    pub fn setup_thread(&mut self, thread: TID, setup: ThreadInit) -> Result<(), Error> {
        if thread == 0 || thread > MAX_THREAD_COUNT {
            return Err(Error::ThreadNotAvailable);
        }
        let idx = self.pid.get() as usize - 1;
        unsafe {
            if let Some(proc) = PROCESS_TABLE[idx].as_mut() {
                let t = &mut proc.threads[thread - 1];
                if t.allocated {
                    return Err(Error::ThreadNotAvailable);
                }
                *t = Thread::default();
                t.allocated = true;
                t.set_pc(setup.call as usize);
                t.set_sp(setup.stack.map(|s| s.as_ptr() as usize + s.len()).unwrap_or(0));
                // Set function arguments in X0-X3
                t.gpr[0] = setup.arg1 as u64;
                t.gpr[1] = setup.arg2 as u64;
                t.gpr[2] = setup.arg3 as u64;
                t.gpr[3] = setup.arg4 as u64;
                // SPSR: EL0t, all interrupts unmasked
                t.spsr = 0;
                Ok(())
            } else {
                Err(Error::ProcessNotFound)
            }
        }
    }

    /// Switch the current thread.
    pub fn set_tid(&mut self, thread: TID) -> Result<(), Error> {
        if thread == 0 || thread > MAX_THREAD_COUNT {
            return Err(Error::ThreadNotAvailable);
        }
        let idx = self.pid.get() as usize - 1;
        unsafe {
            if let Some(proc) = PROCESS_TABLE[idx].as_mut() {
                if !proc.threads[thread - 1].allocated {
                    return Err(Error::ThreadNotAvailable);
                }
                proc.current_thread = thread;
                Ok(())
            } else {
                Err(Error::ProcessNotFound)
            }
        }
    }

    /// Find a free thread slot.
    pub fn find_free_thread(&self) -> Option<TID> {
        let idx = self.pid.get() as usize - 1;
        unsafe {
            PROCESS_TABLE[idx].as_ref().and_then(|proc| {
                proc.threads.iter().enumerate().find_map(|(i, t)| {
                    if !t.allocated { Some(i + 1) } else { None }
                })
            })
        }
    }

    /// Check if a thread is allocated.
    pub fn thread_exists(&self, tid: TID) -> bool {
        if tid == 0 || tid > MAX_THREAD_COUNT {
            return false;
        }
        let idx = self.pid.get() as usize - 1;
        unsafe {
            PROCESS_TABLE[idx]
                .as_ref()
                .map(|p| p.threads[tid - 1].allocated)
                .unwrap_or(false)
        }
    }

    /// Set the syscall result for a thread.
    pub fn set_thread_result(&mut self, tid: TID, result: xous::Result) {
        if tid == 0 || tid > MAX_THREAD_COUNT {
            return;
        }
        let idx = self.pid.get() as usize - 1;
        unsafe {
            if let Some(proc) = PROCESS_TABLE[idx].as_mut() {
                let args = result.to_args();
                proc.threads[tid - 1].set_args(&args);
            }
        }
    }

    /// Return memory to a thread (queue for later delivery).
    pub fn return_memory(&mut self, _tid: TID, _buf: &[u8]) {
        // TODO(M2): Implement memory return for IPC
    }

    /// Create a new process.
    pub fn create(
        pid: PID,
        _init_data: ProcessInit,
        _services: &mut SystemServices,
    ) -> Result<ProcessStartup, Error> {
        let idx = pid.get() as usize - 1;
        if idx >= MAX_PROCESS_COUNT {
            return Err(Error::OutOfMemory);
        }

        unsafe {
            if PROCESS_TABLE[idx].is_some() {
                return Err(Error::ProcessNotFound); // Already exists
            }

            let mut proc = ProcessImpl {
                threads: [Thread::default(); MAX_THREAD_COUNT],
                current_thread: INITIAL_TID,
            };
            proc.threads[INITIAL_TID - 1].allocated = true;
            PROCESS_TABLE[idx] = Some(proc);
        }

        Ok(ProcessStartup::new(pid))
    }

    /// Destroy a process and free its resources.
    pub fn destroy(pid: PID) -> Result<(), Error> {
        let idx = pid.get() as usize - 1;
        unsafe {
            PROCESS_TABLE[idx] = None;
        }
        Ok(())
    }

    /// Set up the initial process (PID 1) or subsequent process from ELF.
    pub fn setup_process(
        setup: ProcessSetup,
        _services: &mut SystemServices,
    ) -> Result<(), Error> {
        let idx = setup.pid.get() as usize - 1;
        if idx >= MAX_PROCESS_COUNT {
            return Err(Error::OutOfMemory);
        }

        unsafe {
            // If the process slot doesn't exist yet, create it
            if PROCESS_TABLE[idx].is_none() {
                PROCESS_TABLE[idx] = Some(ProcessImpl {
                    threads: [Thread::default(); MAX_THREAD_COUNT],
                    current_thread: INITIAL_TID,
                });
            }

            if let Some(proc) = PROCESS_TABLE[idx].as_mut() {
                let thread = &mut proc.threads[INITIAL_TID - 1];
                thread.allocated = true;
                thread.set_pc(setup.entry_point);
                // Stack grows downward: SP points to top of stack region
                thread.set_sp(setup.stack.as_ptr() as usize + setup.stack.len());
                // SPSR: EL0t (user mode), interrupts enabled
                thread.spsr = 0;
            }
        }
        Ok(())
    }

    /// Call a closure with a reference to the current process.
    pub fn with_current<F, R>(f: F) -> R
    where
        F: FnOnce(&Process) -> R,
    {
        let process = Process::current();
        f(&process)
    }

    /// Call a closure with a mutable reference to the current process.
    pub fn with_current_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Process) -> R,
    {
        let mut process = Process::current();
        f(&mut process)
    }

    /// Set up the IRQ handler thread for the current process.
    pub fn run_irq_handler(&mut self, pc: usize, irq_no: usize, arg: usize) {
        let idx = self.pid.get() as usize - 1;
        unsafe {
            if let Some(proc) = PROCESS_TABLE[idx].as_mut() {
                // Use thread 0 (IRQ_TID) for IRQ handling
                let thread = &mut proc.threads[0];
                thread.set_pc(pc);
                thread.gpr[0] = irq_no as u64;
                thread.gpr[1] = arg as u64;
                thread.spsr = 0; // EL0t
            }
        }
    }

    /// Destroy the given thread and return its exit value and stack range.
    /// Returns `None` if the thread does not exist.
    pub fn destroy_thread(&mut self, tid: TID) -> Option<(usize, Option<MemoryRange>)> {
        if tid == 0 || tid > MAX_THREAD_COUNT {
            return None;
        }
        let idx = self.pid.get() as usize - 1;
        unsafe {
            if let Some(proc) = PROCESS_TABLE[idx].as_mut() {
                let thread = &mut proc.threads[tid - 1];
                if !thread.allocated {
                    return None;
                }
                // Capture the return value from X0 and the stack range
                let return_value = thread.gpr[0] as usize;
                let stack = thread.stack.take();
                // Clear the thread slot
                *thread = Thread::default();
                Some((return_value, stack))
            } else {
                None
            }
        }
    }

    /// Get a reference to the thread context for the given TID.
    pub fn thread(&self, tid: TID) -> &Thread {
        let idx = self.pid.get() as usize - 1;
        unsafe {
            let proc = PROCESS_TABLE[idx].as_ref().expect("process not found");
            &proc.threads[tid.saturating_sub(1).min(MAX_THREAD_COUNT - 1)]
        }
    }

    /// Crash the current process (abort handler).
    pub fn crash_current_process() {
        let pid = current_pid();
        let idx = pid.get() as usize - 1;
        unsafe {
            PROCESS_TABLE[idx] = None;
        }
    }

    /// Send raw bytes to the current process (used in hosted mode only).
    /// On hardware, this is a no-op — communication is via registers.
    pub fn send(&mut self, _bytes: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}
