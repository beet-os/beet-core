// SPDX-FileCopyrightText: 2020 Sean Cross <sean@xobs.io>
// SPDX-License-Identifier: Apache-2.0

#[cfg(beetos)]
use core::ptr::{addr_of, addr_of_mut};

#[cfg(beetos)]
use xous::arch::MAX_PROCESS_NAME_LEN;
use xous::{
    arch::ProcessStartup, pid_from_usize, AppId, Error, MemoryAddress, MemoryRange, Message, ProcessInit,
    SystemEvent, ThreadInit, CID, PID, SID, TID,
};

use crate::arch::mem::MemoryMapping;
pub use crate::arch::process::Process as ArchProcess;
pub use crate::arch::process::MAX_PROCESS_COUNT;
#[cfg(beetos)]
pub use crate::arch::process::MAX_THREAD_COUNT;
use crate::debug::BufStr;
use crate::platform;
use crate::process::{current_pid, ConnectionSlot, Process, ThreadState, INITIAL_TID, PANIC_MESSAGE_SIZE};
use crate::scheduler::Scheduler;
use crate::server::Server;

const MAX_SERVER_COUNT: usize = 128;

/// A big unifying struct containing all of the system state.
#[allow(dead_code)]
pub struct SystemServices {
    /// A table of all processes in the system
    pub processes: [Option<Process>; MAX_PROCESS_COUNT],

    /// A table of all servers in the system
    pub servers: [Option<Server>; MAX_SERVER_COUNT],

    /// Panic message buffer, shared across all processes and only one can panic at a time
    panic_message: BufStr<[u8; PANIC_MESSAGE_SIZE]>,

    /// PID of the process that "owns" the current panic message
    panic_message_pid: Option<PID>,
}

#[cfg(not(beetos))]
std::thread_local!(static SYSTEM_SERVICES: core::cell::RefCell<SystemServices> = core::cell::RefCell::new(SystemServices {
    processes: [const { None }; MAX_PROCESS_COUNT],
    servers: [const { None }; 128],
    panic_message: BufStr::new(),
    panic_message_pid: None,
}));

#[cfg(beetos)]
#[no_mangle]
static mut SYSTEM_SERVICES: SystemServices = SystemServices {
    processes: [const { None }; MAX_PROCESS_COUNT],
    servers: [const { None }; MAX_SERVER_COUNT],
    panic_message: BufStr::new(),
    panic_message_pid: None,
};

#[allow(dead_code)]
impl SystemServices {
    /// Calls the provided function with the current inner process state.
    pub fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&SystemServices) -> R,
    {
        #[cfg(beetos)]
        unsafe {
            f(&*addr_of!(SYSTEM_SERVICES))
        }
        #[cfg(not(beetos))]
        SYSTEM_SERVICES.with(|ss| f(&ss.borrow()))
    }

    pub fn with_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut SystemServices) -> R,
    {
        #[cfg(beetos)]
        unsafe {
            f(&mut *addr_of_mut!(SYSTEM_SERVICES))
        }

        #[cfg(not(beetos))]
        SYSTEM_SERVICES.with(|ss| f(&mut ss.borrow_mut()))
    }

    /// Append bytes to the panic message for the current process.
    /// If another process previously owned the buffer, ownership is transferred
    /// and the buffer is cleared.
    /// This prevents a process from blocking others from recording panic messages.
    pub fn append_panic_message(&mut self, msg_bytes: &[u8]) -> Result<(), Error> {
        use core::fmt::Write;

        let pid = current_pid();

        // If a new process is claiming the buffer, clear it and transfer ownership
        if self.panic_message_pid != Some(pid) {
            self.panic_message = BufStr::new();
            self.panic_message_pid = Some(pid);
        }

        let str = core::str::from_utf8(msg_bytes).map_err(|_| Error::InvalidString)?;
        self.panic_message.write_str(str).map_err(|_| Error::InvalidString)?;

        Ok(())
    }

    /// Returns the panic message for a process if there's one
    #[cfg(not(beetos))]
    pub fn get_panic_message(&self, pid: PID) -> Option<&BufStr<[u8; PANIC_MESSAGE_SIZE]>> {
        if self.panic_message_pid == Some(pid) {
            Some(&self.panic_message)
        } else {
            None
        }
    }

    pub fn take_panic_message(&mut self) -> (Option<PID>, &[u8]) {
        let pid = self.panic_message_pid.take();
        (pid, self.panic_message.as_slice())
    }

    /// Initialize PID1 (kernel process) without loading any services.
    ///
    /// Used when booting directly (e.g., QEMU `-kernel` without a loader).
    /// The kernel runs as PID1 with full permissions. Services can be
    /// dynamically loaded later via CreateProcess syscalls.
    #[cfg(beetos)]
    pub fn init_pid1(&mut self) {
        use beetos::{
            KERNEL_IRQ_HANDLER_STACK_BOTTOM, KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT, KERNEL_STACK_BOTTOM,
            KERNEL_STACK_PAGE_COUNT, PAGE_SIZE,
        };

        const PID1: PID = PID::new(1).unwrap();

        let mut process = Process::new(
            MemoryMapping::current(),
            PID1,
            PID1,
            [0x31444950u32, 0, 0, 0].into(), // 'PID1'
        );
        process.set_thread_priority(INITIAL_TID, xous::ThreadPriority::Idle);
        process.set_thread_state(INITIAL_TID, ThreadState::Ready);
        process.set_name(b"kernel").expect("Couldn't set the PID1 name");
        process.set_syscall_permissions(u64::MAX);
        self.processes[0] = Some(process);

        let stack = unsafe {
            MemoryRange::new(
                KERNEL_STACK_BOTTOM - KERNEL_STACK_PAGE_COUNT * PAGE_SIZE,
                KERNEL_STACK_PAGE_COUNT * PAGE_SIZE,
            )
            .expect("stack")
        };
        let irq_stack = unsafe {
            MemoryRange::new(
                KERNEL_IRQ_HANDLER_STACK_BOTTOM - KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT * PAGE_SIZE,
                KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT * PAGE_SIZE,
            )
            .expect("irq stack")
        };

        ArchProcess::setup_process(
            crate::arch::process::ProcessSetup {
                pid: PID1,
                entry_point: 0,
                stack,
                irq_stack,
                aslr_slide: 0,
            },
            self,
        )
        .expect("couldn't setup PID1 process");
    }

    /// Create a new "System Services" object based on the arguments from the
    /// kernel loader. These arguments decide where the memory spaces are located, as
    /// well as where the stack and program counter should initially go.
    #[cfg(beetos)]
    pub fn init_from_memory(&mut self, args: &crate::args::KernelArguments) {
        use beetos::{
            KERNEL_IRQ_HANDLER_STACK_BOTTOM, KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT, KERNEL_STACK_BOTTOM,
            KERNEL_STACK_PAGE_COUNT, PAGE_SIZE,
        };
        use xous::AppId;

        const PID1: PID = PID::new(1).unwrap();

        let mut process = Process::new(
            MemoryMapping::current(),
            PID1,
            PID1,
            [0x31444950u32, 0, 0, 0].into(), // 'PID1'
        );
        process.set_thread_priority(INITIAL_TID, xous::ThreadPriority::Idle);
        process.set_thread_state(INITIAL_TID, ThreadState::Ready);
        process.set_name(b"kernel").expect("Couldn't set the PID1 name");
        process.set_syscall_permissions(u64::MAX);
        self.processes[0] = Some(process);

        let stack = unsafe {
            MemoryRange::new(
                KERNEL_STACK_BOTTOM - KERNEL_STACK_PAGE_COUNT * PAGE_SIZE,
                KERNEL_STACK_PAGE_COUNT * PAGE_SIZE,
            )
            .expect("stack")
        };
        let irq_stack = unsafe {
            MemoryRange::new(
                KERNEL_IRQ_HANDLER_STACK_BOTTOM - KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT * PAGE_SIZE,
                KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT * PAGE_SIZE,
            )
            .expect("irq stack")
        };
        // Set up our handle with a bogus sp and pc.  These will get updated
        // once a context switch _away_ from the kernel occurs, however we need
        // to make sure other fields such as "thread number" are all valid.
        ArchProcess::setup_process(
            crate::arch::process::ProcessSetup {
                pid: PID::new(1).unwrap(),
                entry_point: 0,
                stack,
                irq_stack,
                aslr_slide: 0,
            },
            self,
        )
        .expect("couldn't setup PID1 process");

        for arg in args.iter().filter(|arg| arg.name == u32::from_le_bytes(*b"BElf")) {
            // Restart the watchdog per process loaded.
            // This gives the entire watchdog time period for the process to load
            crate::platform::wdt::restart();

            let mut pname: [u8; MAX_PROCESS_NAME_LEN] = [0; MAX_PROCESS_NAME_LEN];

            let app_id = AppId::from([arg.data[2], arg.data[3], arg.data[4], arg.data[5]]);
            pname
                .iter_mut()
                .zip(arg.data[6..].iter().flat_map(|x| x.to_le_bytes()))
                .for_each(|(a, b)| *a = b);

            let name_len = pname.iter().position(|b| *b == 0).unwrap_or(MAX_PROCESS_NAME_LEN);
            let _name = core::str::from_utf8(&pname[..name_len]).unwrap_or("N/A");
            println!("[>] Loading `{}` size: {} k", _name, arg.data[1] / 1024);

            self.create_process(ProcessInit {
                elf: unsafe {
                    MemoryRange::new(args.base as usize + arg.data[0] as usize, arg.data[1] as usize).unwrap()
                },
                name_addr: MemoryAddress::new(&pname as *const u8 as _).unwrap(),
                app_id,
            })
            .unwrap();
        }

        for arg in args.iter() {
            if arg.name == u32::from_le_bytes(*b"PMem") {
                let pid = arg.data[0] as u8;
                for range in arg.data[1..].chunks(2) {
                    self.process_mut(PID::new(pid).unwrap())
                        .unwrap_or_else(|e| panic!("Could not find PID={pid} in arg {arg}: {e:?}"))
                        .add_memory_permission((range[0] as usize)..(range[1] as usize))
                        .unwrap_or_else(|e| panic!("Could not add memory permission to {pid}: {e:?}"));
                }
            } else if arg.name == u32::from_le_bytes(*b"PSys") {
                let pid = arg.data[0] as u8;
                let mask = (arg.data[1] as u64) | ((arg.data[2] as u64) << 32);
                self.process_mut(PID::new(pid).unwrap())
                    .unwrap_or_else(|e| panic!("Could not find PID={pid} in arg {arg}: {e:?}"))
                    .set_syscall_permissions(mask);
            }
        }
    }

    /// Add a new entry to the process table. This results in a new address space
    /// and a new PID, though the process is in the state `Setup()`.
    pub fn create_process(&mut self, init_process: ProcessInit) -> Result<ProcessStartup, Error> {
        let ppid = crate::arch::process::current_pid();
        let slot_index = self.processes.iter_mut().position(|p| p.is_none()).ok_or_else(|| {
            println!("[!] No free PIDs left to allocate a new process");
            Error::OutOfMemory
        })?;
        let pid = pid_from_usize(slot_index + 1)?;
        let mut mapping = MemoryMapping::default();
        unsafe { mapping.allocate(pid)? };
        let mut process = Process::new(mapping, pid, ppid, init_process.app_id);
        #[cfg(beetos)]
        {
            let name_str = unsafe {
                core::slice::from_raw_parts(init_process.name_addr.get() as *const u8, MAX_PROCESS_NAME_LEN)
            };
            process.set_name(name_str)?;
        }
        process.set_thread_state(INITIAL_TID, ThreadState::Ready);
        self.processes[slot_index] = Some(process);
        let startup = crate::arch::process::Process::create(pid, init_process, self).unwrap();
        klog!("created new process for PID {} with PPID {}", pid, ppid);
        Ok(startup)
    }

    pub fn process_index(pid: PID) -> usize {
        // PID0 doesn't exist -- process IDs are offset by 1.
        pid.get() as usize - 1
    }

    pub fn process(&self, pid: PID) -> Result<&Process, Error> {
        self.processes[Self::process_index(pid)].as_ref().ok_or(Error::ProcessNotFound)
    }

    pub fn process_mut(&mut self, pid: PID) -> Result<&mut Process, Error> {
        self.processes[Self::process_index(pid)].as_mut().ok_or(Error::ProcessNotFound)
    }

    /// Insert a pre-built process into the process table at the given slot index.
    /// Used during early boot to register processes created outside of create_process().
    #[cfg(beetos)]
    pub fn insert_process(&mut self, slot_index: usize, process: Process) {
        self.processes[slot_index] = Some(process);
    }

    pub fn free_process(&mut self, pid: PID) { self.processes[Self::process_index(pid)] = None; }

    pub fn current_process(&self) -> &Process { self.process(current_pid()).unwrap() }

    pub fn current_process_mut(&mut self) -> &mut Process { self.process_mut(current_pid()).unwrap() }

    pub fn for_all_processes(&mut self, mut f: impl FnMut(&mut Process)) {
        for process in self.processes.iter_mut().flatten() {
            f(process);
        }
    }

    pub fn retry_syscall(&mut self, tid: TID, state: ThreadState) -> Result<xous::Result, Error> {
        ArchProcess::current().retry_swi_instruction(tid)?;
        self.current_process_mut().set_thread_state(tid, state);
        Scheduler::with_mut(|s| s.activate_current(self))
    }

    pub fn set_thread_result(&mut self, pid: PID, tid: TID, result: xous::Result) -> Result<(), Error> {
        // Temporarily switch into the target process memory space
        // in order to pass the return value.
        let current_pid = current_pid();
        if current_pid == pid {
            ArchProcess::current().set_thread_result(tid, result);
            return Ok(());
        }

        self.process(pid)?.activate();
        ArchProcess::current().set_thread_result(tid, result);

        // Return to the original memory space.
        self.process(current_pid).expect("couldn't switch back after setting context result").activate();
        Ok(())
    }

    /// Move memory from one process to another.
    ///
    /// During this process, memory is deallocated from the first process, then
    /// we switch contexts and look for a free slot in the second process. After
    /// that, we switch back to the first process and return.
    ///
    /// If no free slot can be found, memory is re-attached to the first
    /// process.  By following this break-then-make approach, we avoid getting
    /// into a situation where memory may appear in two different processes at
    /// once.
    ///
    /// The given memory range is guaranteed to be unavailable in the src process
    /// after this function returns.
    ///
    /// # Returns
    ///
    /// Returns the virtual address of the memory region in the target process.
    ///
    /// # Errors
    ///
    /// * **ShareViolation**: Tried to mutably share a region that was already shared
    /// * **BadAddress**: The provided address was not valid
    /// * **BadAlignment**: The provided address or length was not page-aligned
    ///
    /// # Panics
    ///
    /// If the memory should have been able to go into the destination process
    /// but failed, then the system panics.
    #[cfg(beetos)]
    pub fn send_memory(
        &mut self,
        src_virt: *mut usize,
        dest_pid: PID,
        dest_virt: *mut usize,
        len: usize,
    ) -> Result<*mut usize, Error> {
        if len == 0 {
            return Err(Error::BadAddress);
        }
        if len & (beetos::PAGE_SIZE - 1) != 0 {
            return Err(Error::BadAddress);
        }
        if src_virt as usize & (beetos::PAGE_SIZE - 1) != 0 {
            return Err(Error::BadAddress);
        }
        if dest_virt as usize & (beetos::PAGE_SIZE - 1) != 0 {
            return Err(Error::BadAddress);
        }
        if (dest_virt as usize) + len > beetos::USER_AREA_END {
            return Err(Error::BadAddress);
        }

        let current_pid = current_pid();

        // Iterators and `ptr.wrapping_add()` operate on `usize` types,
        // which effectively lowers the `len`.
        let usize_len = len / core::mem::size_of::<usize>();
        let usize_page = crate::mem::PAGE_SIZE / core::mem::size_of::<usize>();

        // If the dest and src PID is the same, do nothing.
        if current_pid == dest_pid {
            crate::mem::MemoryManager::with_mut(|mm| {
                for offset in (0..usize_len).step_by(usize_page) {
                    mm.ensure_page_exists(src_virt.wrapping_add(offset))?;
                }
                Ok(())
            })?;
            return Ok(src_virt);
        }

        let src_mapping = &mut self.process_mut(current_pid)?.mapping;
        // Opt out of the borrow checker, because we know these are two different mappings.
        let src_mapping = unsafe { &mut *(src_mapping as *mut _) };
        let dest_mapping = &mut self.process_mut(dest_pid)?.mapping;
        crate::mem::MemoryManager::with_mut(|mm| {
            let dest_virt = mm.find_virtual_address(dest_mapping, dest_virt, len)?;

            let mut error = None;

            // Move each subsequent page.
            for offset in (0..usize_len).step_by(usize_page) {
                assert_eq!(((src_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)), 0);
                assert_eq!(((dest_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)), 0);
                mm.ensure_page_exists(src_virt.wrapping_add(offset))?;
                mm.move_page(
                    src_mapping,
                    src_virt.wrapping_add(offset),
                    dest_mapping,
                    dest_virt.wrapping_add(offset),
                )
                .unwrap_or_else(|e| error = Some(e));
            }
            error.map_or_else(|| Ok(dest_virt), |e| panic!("unable to send: {:?}", e))
        })
    }

    #[cfg(not(beetos))]
    pub fn send_memory(
        &mut self,
        src_virt: *mut usize,
        _dest_pid: PID,
        _dest_virt: *mut usize,
        _len: usize,
    ) -> Result<*mut usize, Error> {
        Ok(src_virt)
    }

    /// Lend memory from one process to another.
    ///
    /// During this process, memory is marked as `Shared` in the source process.
    /// If the share is Mutable, then this memory is unmapped from the source
    /// process.  If the share is immutable, then memory is marked as
    /// not-writable in the source process.
    ///
    /// If no free slot can be found, memory is re-attached to the first
    /// process.  By following this break-then-make approach, we avoid getting
    /// into a situation where memory may appear in two different processes at
    /// once.
    ///
    /// If the share is mutable and the memory is already shared, then an error
    /// is returned.
    ///
    /// # Returns
    ///
    /// Returns the virtual address of the memory region in the target process.
    ///
    /// # Errors
    ///
    /// * **ShareViolation**: Tried to mutably share a region that was already shared
    /// * **BadAddress**: The provided address was not valid
    /// * **BadAlignment**: The provided address or length was not page-aligned
    #[cfg(beetos)]
    pub fn lend_memory(
        &mut self,
        src_virt: *mut usize,
        dest_pid: PID,
        dest_virt: *mut usize,
        len: usize,
        mutable: bool,
    ) -> Result<*mut usize, Error> {
        if len == 0 {
            return Err(Error::BadAddress);
        }
        if len & (beetos::PAGE_SIZE - 1) != 0 {
            return Err(Error::BadAlignment);
        }
        if src_virt as usize & (beetos::PAGE_SIZE - 1) != 0 {
            return Err(Error::BadAlignment);
        }
        if dest_virt as usize & (beetos::PAGE_SIZE - 1) != 0 {
            return Err(Error::BadAlignment);
        }
        // Iterators and `ptr.wrapping_add()` operate on `usize` types,
        // which effectively lowers the `len`.
        let usize_len = len / core::mem::size_of::<usize>();
        let usize_page = crate::mem::PAGE_SIZE / core::mem::size_of::<usize>();

        let current_pid = current_pid();
        // If it's within the same process, ignore the move operation and
        // just ensure the pages actually exist.
        if current_pid == dest_pid {
            MemoryManager::with_mut(|mm| {
                for offset in (0..usize_len).step_by(usize_page) {
                    assert!(((src_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)) == 0);
                    mm.ensure_page_exists(src_virt.wrapping_add(offset))?;
                }
                Ok(())
            })?;
            return Ok(src_virt);
        }
        let src_mapping = &mut self.process_mut(current_pid)?.mapping;
        // Opt out of the borrow checker, because we know these are two different mappings.
        let src_mapping = unsafe { &mut *(src_mapping as *mut _) };
        let dest_mapping = &mut self.process_mut(dest_pid)?.mapping;
        use crate::mem::MemoryManager;
        MemoryManager::with_mut(|mm| {
            let dest_virt = mm.find_virtual_address(dest_mapping, dest_virt, len)?;

            let mut error = None;

            // Lend each subsequent page.
            for offset in (0..usize_len).step_by(usize_page) {
                assert!(((src_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)) == 0);
                assert!(((dest_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)) == 0);
                mm.ensure_page_exists(src_virt.wrapping_add(offset))?;
                mm.lend_page(
                    src_mapping,
                    src_virt.wrapping_add(offset),
                    dest_mapping,
                    dest_virt.wrapping_add(offset),
                    mutable,
                )
                .unwrap_or_else(|e| {
                    error = Some(e);
                });
            }
            error.map_or_else(
                || Ok(dest_virt),
                |e| {
                    panic!(
                        "unable to lend {:08x} in pid {} to {:08x} in pid {}: {:?}",
                        src_virt as usize, current_pid, dest_virt as usize, dest_pid, e
                    )
                },
            )
        })
    }

    #[cfg(not(beetos))]
    pub fn lend_memory(
        &mut self,
        src_virt: *mut usize,
        _dest_pid: PID,
        _dest_virt: *mut usize,
        _len: usize,
        _mutable: bool,
    ) -> Result<*mut usize, Error> {
        Ok(src_virt)
    }

    /// Return memory from one process back to another
    ///
    /// During this process, memory is unmapped from the source process.
    ///
    /// # Returns
    ///
    /// Returns the virtual address of the memory region in the target process.
    ///
    /// # Errors
    ///
    /// * **ShareViolation**: Tried to mutably share a region that was already shared
    #[cfg(beetos)]
    pub fn return_memory(
        &mut self,
        src_virt: *mut usize,
        dest_pid: PID,
        _dest_tid: TID,
        dest_virt: *mut usize,
        len: usize,
    ) -> Result<*mut usize, Error> {
        if len == 0 {
            // klog!("No len");
            return Err(Error::BadAddress);
        }
        if len & (beetos::PAGE_SIZE - 1) != 0 {
            // klog!("len not aligned");
            return Err(Error::BadAddress);
        }
        if src_virt as usize & (beetos::PAGE_SIZE - 1) != 0 {
            // klog!("Src virt not aligned");
            return Err(Error::BadAddress);
        }
        if dest_virt as usize & (beetos::PAGE_SIZE - 1) != 0 {
            // klog!("dest virt not aligned");
            return Err(Error::BadAddress);
        }

        // Iterators and `ptr.wrapping_add()` operate on `usize` types,
        // which effectively lowers the `len`.
        let usize_len = len / core::mem::size_of::<usize>();
        let usize_page = crate::mem::PAGE_SIZE / core::mem::size_of::<usize>();

        let current_pid = current_pid();
        // If it's within the same process, ignore the operation.
        if current_pid == dest_pid {
            return Ok(src_virt);
        }
        let src_mapping = &mut self.process_mut(current_pid)?.mapping;
        // Opt out of the borrow checker, because we know these are two different mappings.
        let src_mapping = unsafe { &mut *(src_mapping as *mut _) };
        let dest_mapping = &mut self.process_mut(dest_pid)?.mapping;
        use crate::mem::MemoryManager;
        MemoryManager::with_mut(|mm| {
            let mut error = None;

            // Lend each subsequent page.
            for offset in (0..usize_len).step_by(usize_page) {
                assert!(((src_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)) == 0);
                assert!(((dest_virt.wrapping_add(offset) as usize) & (beetos::PAGE_SIZE - 1)) == 0);
                mm.unlend_page(
                    src_mapping,
                    src_virt.wrapping_add(offset),
                    dest_mapping,
                    dest_virt.wrapping_add(offset),
                )
                .unwrap_or_else(|e| {
                    error = Some(e);
                });
            }
            error.map_or_else(|| Ok(dest_virt), Err)
        })
    }

    #[cfg(not(beetos))]
    pub fn return_memory(
        &mut self,
        src_virt: *mut usize,
        dest_pid: PID,
        dest_tid: TID,
        _dest_virt: *mut usize,
        len: usize,
        // buf: MemoryRange,
    ) -> Result<*mut usize, Error> {
        let buf = unsafe { MemoryRange::new(src_virt as usize, len) }?;
        let buf = buf.as_slice();
        let current_pid = current_pid();
        {
            let target_process = self.process(dest_pid)?;
            target_process.activate();
            let mut arch_process = ArchProcess::current();
            arch_process.return_memory(dest_tid, buf);
        }
        let target_process = self.process(current_pid)?;
        target_process.activate();

        Ok(src_virt as *mut usize)
    }

    /// Create a new thread in the current process.  Execution begins at
    /// `entrypoint`, with the stack pointer set to `stack_pointer`.  A single
    /// argument will be passed to the new function.
    ///
    /// The return address of this thread will be `EXIT_THREAD`, which the
    /// kernel can trap on to indicate a thread exited.
    ///
    /// # Errors
    ///
    /// * **ThreadNotAvailable**: The process has used all of its context slots.
    pub fn create_thread(&mut self, parent: TID, thread_init: ThreadInit) -> Result<TID, Error> {
        let mut arch_process = ArchProcess::current();
        let tid = arch_process.find_free_thread().ok_or(Error::ThreadNotAvailable)?;

        arch_process.setup_thread(tid, thread_init)?;
        let process = self.current_process_mut();
        process.set_thread_state(tid, ThreadState::Ready);
        process.set_thread_priority(tid, process.thread_priority(parent));

        Ok(tid)
    }

    /// Destroy the given thread. Returns `true` if the PID has been updated.
    /// # Errors
    ///
    /// * **ThreadNotAvailable**: The thread does not exist in this process
    #[cfg(beetos)]
    pub fn thread_exited(&mut self, tid: TID) -> Result<xous::Result, Error> {
        self.current_process_mut().set_thread_state(tid, ThreadState::Free);

        if tid != crate::process::IRQ_TID {
            let mut arch_process = ArchProcess::current();

            let (return_value, stack) = arch_process.destroy_thread(tid).unwrap_or_default();
            if let Some(stack) = stack {
                crate::mem::MemoryManager::with_mut(|mm| mm.unmap_range(stack.as_ptr(), stack.len())).ok();
            }

            for waiting_tid in 0..MAX_THREAD_COUNT {
                if (self.current_process().thread_state(waiting_tid) == ThreadState::WaitJoin { tid }) {
                    crate::syscall::wake_thread_with_result(
                        self, current_pid(), waiting_tid,
                        xous::Result::Scalar1(return_value),
                    );
                }
            }
        }
        Scheduler::with_mut(|s| s.activate_current(self))
    }

    /// Park this thread if the target thread is currently running. Otherwise,
    /// return the value of the given thread.
    pub fn join_thread(&mut self, tid: TID, join_tid: TID) -> Result<xous::Result, Error> {
        let process = self.current_process_mut();

        // We cannot wait on ourselves.
        if tid == join_tid {
            return Err(Error::ThreadNotAvailable);
        }

        if process.thread_state(join_tid) != ThreadState::Free {
            #[cfg(beetos)]
            {
                crate::syscall::suspend_with_future(
                    self, tid,
                    crate::kfuture::KernelFuture::WaitJoin,
                    crate::kfuture::EVENT_KERNEL,
                );
                // Keep WaitJoin state for scan in thread_exited.
                self.current_process_mut()
                    .set_thread_state(tid, ThreadState::WaitJoin { tid: join_tid });
            }
            #[cfg(not(beetos))]
            process.set_thread_state(tid, ThreadState::WaitJoin { tid: join_tid });
            Scheduler::with_mut(|s| s.activate_current(self))
        } else {
            // The thread does not exist -- continue execution
            // Err(xous::Error::ThreadNotAvailable)
            Ok(xous::Result::Scalar1(0))
        }
    }

    pub fn wake_threads_with_state(&mut self, state: ThreadState, n: usize) {
        self.for_all_processes(|p| p.wake_threads_with_state(state, n));
    }

    /// Allocate a new server ID for this process and return the address. If the
    /// server table is full, or if there is not enough memory to map the server queue,
    /// return an error.
    ///
    /// # Errors
    ///
    /// * **OutOfMemory**: A new page could not be assigned to store the server queue.
    /// * **ServerNotFound**: The server queue was full and a free slot could not be found.
    pub fn create_server_with_address(
        &mut self,
        sid: SID,
        initial_permissions: core::ops::Range<xous::MessageId>,
    ) -> Result<SID, Error> {
        let pid = current_pid();
        for entry in self.servers.iter_mut() {
            if entry.is_none() {
                #[cfg(beetos)]
                // Allocate a single page for the server queue
                let backing = crate::mem::MemoryManager::with_mut(|mm| {
                    mm.map_range(
                        0,
                        core::ptr::null_mut(),
                        crate::mem::PAGE_SIZE,
                        xous::MemoryFlags::W | xous::MemoryFlags::POPULATE,
                        false,
                    )
                })?;

                #[cfg(not(beetos))]
                let backing = unsafe { MemoryRange::new(beetos::PAGE_SIZE, beetos::PAGE_SIZE).unwrap() };

                Server::init(entry, pid, sid, backing, initial_permissions).unwrap();

                self.wake_threads_with_state(
                    ThreadState::RetryConnect { sid_hash: sid.quick_hash() },
                    usize::MAX,
                );
                return Ok(sid);
            }
        }
        Err(Error::ServerNotFound)
    }

    /// Generate a random server ID and return it to the caller. Doesn't create
    /// any processes.
    pub fn create_server_id(&mut self) -> Result<SID, Error> {
        let sid = SID::from_u32(
            platform::rand::get_u32(),
            platform::rand::get_u32(),
            platform::rand::get_u32(),
            platform::rand::get_u32(),
        );
        Ok(sid)
    }

    /// Destroy the provided server ID and disconnect any processes that are
    /// connected.
    pub fn destroy_server(&mut self, pid: PID, sid: SID) -> Result<(), Error> {
        let sidx = self.sidx_from_sid(sid, pid).ok_or(Error::ServerNotFound)?;
        self.destroy_sidx(sidx);
        Ok(())
    }

    /// Connect the specified PID to the specified server
    pub fn connect_to_server(&mut self, pid: PID, sid: SID) -> Result<CID, Error> {
        let sidx = self
            .servers
            .iter()
            .position(|s| s.as_ref().is_some_and(|s| s.sid == sid))
            .ok_or(Error::ServerNotFound)?;
        let permissions = self.server_from_sidx(sidx).unwrap().default_permissions.clone();
        self.process_mut(pid)?.add_connection(sidx, permissions)
    }

    /// Invalidate the provided connection ID.
    pub fn disconnect_from_server(&mut self, cid: CID) -> Result<(), Error> {
        // Check to see if we've already connected to this server.
        // While doing this, find a free slot in case we haven't
        // yet connected.
        let connection_slot = self.current_process_mut().connection_mut(cid)?;
        match connection_slot {
            ConnectionSlot::Free => return Err(Error::ServerNotFound),
            ConnectionSlot::Tombstone { refcount } | ConnectionSlot::Connected { refcount, .. }
                if *refcount > 1 =>
            {
                *refcount -= 1
            }
            ConnectionSlot::Tombstone { .. } | ConnectionSlot::Connected { .. } => {
                *connection_slot = ConnectionSlot::Free;
                klog!("Removing server from connection map");
            }
        };
        Ok(())
    }

    /// Retrieve the server ID index from the specified SID and PID
    pub fn sidx_from_sid(&self, sid: SID, pid: PID) -> Option<usize> {
        self.servers.iter().position(|s| s.as_ref().is_some_and(|s| s.sid == sid && s.pid == pid))
    }

    /// Return a server based on the connection id and the current process
    pub fn server_from_sidx(&self, sidx: usize) -> Option<&Server> {
        if sidx > self.servers.len() {
            None
        } else {
            self.servers[sidx].as_ref()
        }
    }

    /// Return a server based on the connection id and the current process
    pub fn server_from_sidx_mut(&mut self, sidx: usize) -> Option<&mut Server> {
        if sidx > self.servers.len() {
            None
        } else {
            self.servers[sidx].as_mut()
        }
    }

    /// Switch to the server's memory space and add the message to its server
    /// queue
    pub fn queue_server_message(
        &mut self,
        sidx: usize,
        pid: PID,
        thread: TID,
        message: Message,
        original_address: Option<MemoryAddress>,
    ) -> Result<usize, Error> {
        let current_pid = current_pid();
        let result = {
            let server_pid = self.server_from_sidx(sidx).ok_or(Error::ServerNotFound)?.pid;
            {
                let server_process = self.process(server_pid)?;
                server_process.mapping.activate();
            }
            let server = self.server_from_sidx_mut(sidx).expect("couldn't re-discover server index");
            server.queue_message(pid, thread, message, original_address)
        };
        let current_process = self.process(current_pid).expect("couldn't restore previous process");
        current_process.mapping.activate();
        result
    }

    /// Switch to the server's address space and add a "remember this address"
    /// entry to its server queue, then switch back to the original address space.
    pub fn remember_server_message(
        &mut self,
        sidx: usize,
        current_pid: PID,
        current_thread: TID,
        message: &Message,
        client_address: Option<MemoryAddress>,
    ) -> Result<usize, Error> {
        let server_pid = self.server_from_sidx(sidx).ok_or(Error::ServerNotFound)?.pid;
        self.process(server_pid)?.mapping.activate();
        let server = self.server_from_sidx_mut(sidx).expect("couldn't re-discover server index");
        let result = server.queue_response(current_pid, current_thread, message, client_address);
        self.process(current_pid).expect("couldn't find old process").mapping.activate();
        result
    }

    /// Terminate the given process. Returns the process' parent PID.
    pub fn terminate_current_process(&mut self, ret: u32) -> Result<xous::Result, Error> {
        let pid = current_pid();

        // Notify the parent process that this process is terminating
        // Crash the OS if the terminated process was a system process
        if ret != 0 && self.current_process().ppid.map(|p| p.get() == 1).unwrap_or(false) {
            #[cfg(beetos)]
            {
                #[cfg(not(feature = "production"))]
                crate::debug::serial::with_output(|stream| self.print_current_process(stream, true).unwrap());
                let process_name = self.current_process().name().unwrap_or("N/A");
                panic!("System process PID={} (`{}`) terminated with code {}", pid, process_name, ret);
            }

            #[cfg(not(beetos))]
            {
                let panic_message = self.get_panic_message(pid).cloned();
                if let Some(panic_msg) = panic_message {
                    panic!("System process PID={} terminated with code {}\n{}", pid, ret, panic_msg);
                } else {
                    panic!("System process PID={} terminated with code {}\n= <NO PANIC> =", pid, ret);
                }
            }
        }

        for sidx in 0..self.servers.len() {
            let Some(server_pid) = self.servers[sidx].as_ref().map(|s| s.pid) else { continue };
            if server_pid == pid {
                // This is our server, just destroy it.
                self.destroy_sidx(sidx);
            } else {
                self.process(server_pid).unwrap().activate();
                // Look through this server's memory space to determine if this process
                // is mentioned there as having some memory lent out.
                self.servers[sidx].as_mut().unwrap().discard_messages_for_pid(pid);
                self.process(pid)?.activate();
            }
        }

        if let Some(ppid) = self.current_process().ppid {
            self.send_event(ppid, SystemEvent::ChildTerminated, [ret as _, 0, 0, 0]).ok();
        }

        // Wake all threads waiting on this process via WaitProcess syscall.
        // Collect waiters first to avoid borrow conflicts.
        {
            let dying_pid = pid;
            let exit_code = ret as usize;
            // Stack-allocated buffer for waiters (pid, tid pairs).
            // 64 entries should be more than enough — typical case is 1 waiter.
            let mut waiters = [(xous::PID::new(1).unwrap(), 0usize); 64];
            let mut waiter_count = 0;

            for pidx in 0..self.processes.len() {
                let Some(process) = &self.processes[pidx] else { continue };
                let waiter_pid = process.pid;
                for tid in 1..crate::arch::process::MAX_THREAD_COUNT {
                    if process.thread_state(tid)
                        == (ThreadState::WaitProcess { pid: dying_pid })
                    {
                        if waiter_count < waiters.len() {
                            waiters[waiter_count] = (waiter_pid, tid);
                            waiter_count += 1;
                        }
                    }
                }
            }

            // Now wake all collected waiters
            for i in 0..waiter_count {
                let (waiter_pid, tid) = waiters[i];

                // On hardware: if the thread has a kernel future, deposit
                // the result in the mailbox (the future will poll it).
                // Otherwise (hosted mode): set the result directly.
                #[cfg(beetos)]
                {
                    if let Ok(process) = self.process_mut(waiter_pid) {
                        if process.has_kernel_future(tid) {
                            process.set_mailbox(tid, xous::Result::Scalar1(exit_code));
                            process.set_thread_state(tid, ThreadState::Ready);
                            continue;
                        }
                    }
                }
                self.set_thread_result(waiter_pid, tid, xous::Result::Scalar1(exit_code))
                    .ok();
                self.process_mut(waiter_pid)
                    .map(|p| p.set_thread_state(tid, ThreadState::Ready))
                    .ok();
            }
        }

        #[cfg(beetos)]
        if ret != 0 {
            #[cfg(not(feature = "production"))]
            crate::debug::serial::with_output(|stream| self.print_current_process(stream, true).unwrap());
        }

        self.process_mut(pid)?.terminate(ret)?;
        self.free_process(pid);

        // Reparent all children to PID1
        self.for_all_processes(|p| {
            if p.ppid == Some(pid) {
                p.ppid = None
            }
        });
        // In case the process terminated itself
        Scheduler::with_mut(|s| s.activate_current(self))
    }

    fn destroy_sidx(&mut self, sidx: usize) {
        // Return and dequeue any remaining messages
        self.servers[sidx].take().unwrap().destroy(self);

        // Tombstone connections, so send_message throws an error when trying to use this CID, and tell the
        // processes the server no longer exists.
        for pidx in 0..self.processes.len() {
            // Manual indexing because send_event borrow-checks the whole object as mut
            let Some(process) = self.processes[pidx].as_mut() else { continue };
            if let Some(cid) = process.tombstone_connection_by_sidx(sidx) {
                let pid = process.pid;
                self.send_event(pid, SystemEvent::Disconnected, [cid as usize, 0, 0, 0]).ok();
            }
        }
    }

    fn send_event(&mut self, dst_pid: PID, event: SystemEvent, args: [usize; 4]) -> Result<(), Error> {
        if let Some((sid, id)) = self.process(dst_pid)?.get_event_handler(event) {
            if let Some(sidx) = self.sidx_from_sid(sid, dst_pid) {
                let msg = Message::new_scalar(id, args[0], args[1], args[2], args[3]);
                crate::syscall::send_message_inner(self, 0, sidx, msg)?;
            }
        }
        Ok(())
    }

    #[cfg(beetos)]
    pub fn broadcast_event(&mut self, event: SystemEvent, args: [usize; 4]) -> Result<(), Error> {
        for pid in 1..=MAX_PROCESS_COUNT as u8 {
            let pid = PID::new(pid).unwrap();
            if self.process(pid).is_ok() {
                self.send_event(pid, event, args)?;
            }
        }
        Ok(())
    }

    /// Terminates the process with the given PID and return code.
    pub fn terminate_process(&mut self, caller_tid: TID, pid: PID, ret: u32) -> Result<xous::Result, Error> {
        let caller_pid = current_pid();
        klog!("Terminating process with PID {pid} from PID {caller_pid}");
        // Disallow termination of processes spawned by the kernel
        if self.process(pid)?.ppid.map(|pid| pid.get() == 1).unwrap_or(false) {
            println!("[!] PID {caller_pid} attempted to terminate a system process with PID {pid}");
            return Err(Error::AccessDenied);
        }

        self.set_thread_result(caller_pid, caller_tid, xous::Result::Ok)?;
        self.process_mut(pid).unwrap().activate();
        self.terminate_current_process(ret)
    }

    /// Calls the provided function with the current inner process state.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        #[cfg(beetos)]
        crate::platform::shutdown(); // diverges (-> !)

        // Destroy all processes. This will cause them to immediately terminate.
        #[cfg(not(beetos))]
        {
            for process in &mut self.processes {
                if let Some(process) = process {
                    process.activate();
                    process.terminate(0).unwrap_or_default();
                }
            }
            Ok(())
        }
    }

    #[cfg(all(beetos, any(not(feature = "production"), feature = "log-serial")))]
    pub fn print_current_process(
        &self,
        mut output: impl core::fmt::Write,
        with_backtrace: bool,
    ) -> Result<(), Error> {
        if with_backtrace {
            crate::arch::backtrace::print_current_process_backtrace();
        }
        let process = self.current_process();
        writeln!(output, "{:x?} [{}]", process, process.name().unwrap_or("")).ok();
        crate::arch::process::Process::with_current(|arch_process| {
            for tid in 0..MAX_THREAD_COUNT {
                let thread = process.thread_state(tid);
                if thread == ThreadState::Free {
                    continue;
                }
                write!(output, "Thread {tid} (priority={:?}): ", process.thread_priority(tid)).ok();
                if tid == arch_process.current_tid() {
                    write!(output, "[Last active] ").ok();
                }
                match thread {
                    ThreadState::Free => unreachable!(),
                    ThreadState::Ready => writeln!(output,).ok(),
                    ThreadState::WaitJoin { tid: _tid } => writeln!(output, "WaitingJoin({_tid})").ok(),
                    ThreadState::RetryConnect { sid_hash: _sid_hash } => {
                        writeln!(output, "RetryConnect({_sid_hash:08x})").ok()
                    }
                    ThreadState::RetryQueueFull { sidx } => {
                        if let Some(_server) = self.server_from_sidx(sidx) {
                            writeln!(output, "RetryQueueFull({:08x?}, pid={})", _server.sid, _server.pid).ok()
                        } else {
                            writeln!(output, "RetryQueueFull(NONEXISTENT)").ok()
                        }
                    }
                    ThreadState::WaitBlocking { sidx } => {
                        if let Some(_server) = self.server_from_sidx(sidx) {
                            writeln!(output, "WaitBlocking({:08x?}, pid={})", _server.sid, _server.pid).ok()
                        } else {
                            writeln!(output, "WaitBlocking(NONEXISTENT)").ok()
                        }
                    }
                    ThreadState::WaitReceive { sidx } => {
                        if let Some(_server) = self.server_from_sidx(sidx) {
                            writeln!(output, "WaitRecv({:08x?}, pid={})", _server.sid, _server.pid).ok()
                        } else {
                            writeln!(output, "WaitRecv(NONEXISTENT)").ok()
                        }
                    }
                    ThreadState::WaitFutex { addr: _addr } => writeln!(output, "WaitFutex({_addr:08x})").ok(),
                    ThreadState::WaitProcess { pid: _pid } => writeln!(output, "WaitProcess({})", _pid).ok(),
                    ThreadState::WaitEvent { mask: _mask } => writeln!(output, "WaitEvent({_mask:#x})").ok(),
                };
                write!(output, "{:?}", arch_process.thread(tid)).ok();
            }
        });
        writeln!(output,).ok();
        Ok(())
    }

    /// Spawn a new process by name from the embedded binary table.
    /// Creates the process with full syscall permissions and UART mapping.
    /// Returns the PID of the new process.
    #[cfg(beetos)]
    pub fn spawn_by_name(&mut self, name: &str) -> Result<xous::PID, Error> {
        use xous::MemoryAddress;

        let elf_bytes = crate::arch::boot::lookup_binary(name).ok_or_else(|| {
            println!("[!] spawn_by_name: binary '{}' not found", name);
            Error::ProcessNotFound
        })?;

        let elf_range = unsafe {
            MemoryRange::new(elf_bytes.as_ptr() as usize, elf_bytes.len())
        }.map_err(|_| Error::InternalError)?;

        // Use a stack-allocated name buffer
        let mut name_buf = [0u8; xous::arch::MAX_PROCESS_NAME_LEN];
        let copy_len = name.len().min(name_buf.len());
        name_buf[..copy_len].copy_from_slice(&name.as_bytes()[..copy_len]);

        let init = ProcessInit {
            elf: elf_range,
            name_addr: MemoryAddress::new(name_buf.as_ptr() as usize).ok_or(Error::InternalError)?,
            app_id: AppId::from([0u32; 4]),
        };

        let startup = self.create_process(init)?;
        let pid = startup.pid();

        // Debug: check if the shell's stack was corrupted during process creation
        #[cfg(feature = "platform-qemu-virt")]
        {
            // The shell (PID 4) has a value at user VA SP+0x1b8 that gets zeroed.
            // Check by translating the shell's stack VA to PA and reading through phys_to_virt.
            if let Ok(shell_proc) = self.process(xous::PID::new(4).unwrap()) {
                // Check the top stack page (VA near USER_STACK_BOTTOM)
                let stack_va = beetos::USER_STACK_BOTTOM - beetos::PAGE_SIZE; // top page
                if let Ok(pa) = shell_proc.mapping.virt_to_phys(stack_va as *mut usize) {
                    let kern_va = beetos::phys_to_virt(pa) as *const u64;
                    // Read a few values from the page to check for zeroing
                    let sample = unsafe { core::ptr::read_volatile(kern_va.add(16)) }; // random offset
                    println!("[*] After spawn '{}' as PID {}: shell stack page PA={:#x} sample={:#x}",
                        name, pid, pa, sample);
                }
            }
        }

        // Grant all syscall permissions
        self.process_mut(pid)?.set_syscall_permissions(u64::MAX);

        // Map UART MMIO into the new process
        #[cfg(feature = "platform-qemu-virt")]
        {
            const UART_PHYS: usize = 0x0900_0000;
            crate::mem::MemoryManager::with_mut(|mm| {
                let process = self.process_mut(pid).expect("spawned process");
                process.mapping.map_page(
                    mm,
                    UART_PHYS,
                    crate::arch::boot::SHELL_UART_VA as *mut usize,
                    xous::MemoryFlags::W | xous::MemoryFlags::DEV,
                    true,
                ).ok(); // May fail if already mapped, that's fine
            });
        }

        // Pass UART VA via x0
        {
            let idx = pid.get() as usize - 1;
            unsafe { crate::arch::process::set_thread_arg0(idx, crate::arch::boot::SHELL_UART_VA); }
        }

        println!("[*] spawn_by_name: created '{}' as PID {}", name, pid);
        Ok(pid)
    }

    /// Spawn a new process by name with argv data.
    ///
    /// Like `spawn_by_name`, but also allocates a page for argv data,
    /// copies the provided args into it, maps it read-only into the new
    /// process at `ARGV_PAGE_VA`, and sets x1/x2 so the process can read
    /// its arguments.
    ///
    /// `argv_ptr` and `argv_len` point to a buffer in the calling process's
    /// address space containing null-separated argument strings.
    #[cfg(beetos)]
    pub fn spawn_by_name_with_args(
        &mut self,
        name: &str,
        argv_ptr: usize,
        argv_len: usize,
    ) -> Result<xous::PID, Error> {
        use xous::MemoryAddress;

        let elf_bytes = crate::arch::boot::lookup_binary(name).ok_or_else(|| {
            println!("[!] spawn_by_name_with_args: binary '{}' not found", name);
            Error::ProcessNotFound
        })?;

        let elf_range = unsafe {
            MemoryRange::new(elf_bytes.as_ptr() as usize, elf_bytes.len())
        }.map_err(|_| Error::InternalError)?;

        let mut name_buf = [0u8; xous::arch::MAX_PROCESS_NAME_LEN];
        let copy_len = name.len().min(name_buf.len());
        name_buf[..copy_len].copy_from_slice(&name.as_bytes()[..copy_len]);

        let init = ProcessInit {
            elf: elf_range,
            name_addr: MemoryAddress::new(name_buf.as_ptr() as usize).ok_or(Error::InternalError)?,
            app_id: AppId::from([0u32; 4]),
        };

        let startup = self.create_process(init)?;
        let pid = startup.pid();

        // Grant all syscall permissions
        self.process_mut(pid)?.set_syscall_permissions(u64::MAX);

        // Map UART MMIO into the new process
        #[cfg(feature = "platform-qemu-virt")]
        {
            const UART_PHYS: usize = 0x0900_0000;
            crate::mem::MemoryManager::with_mut(|mm| {
                let process = self.process_mut(pid).expect("spawned process");
                process.mapping.map_page(
                    mm,
                    UART_PHYS,
                    crate::arch::boot::SHELL_UART_VA as *mut usize,
                    xous::MemoryFlags::W | xous::MemoryFlags::DEV,
                    true,
                ).ok();
            });
        }

        // Copy argv data and map into new process
        let actual_argv_len = if argv_ptr != 0 && argv_len > 0 {
            let clamped_len = argv_len.min(beetos::ARGV_MAX_LEN);

            // Copy argv from caller's address space into a kernel buffer.
            // The caller's TTBR0 is still active, so we can read from
            // their VA through the physical mapping.
            let mut argv_buf = [0u8; 256]; // stack buffer for small args
            let use_len = clamped_len.min(argv_buf.len());

            // Read through the caller's page tables: translate VA → PA → kernel VA
            let caller_pid = crate::arch::process::current_pid();
            let caller_proc = self.process(caller_pid).map_err(|_| Error::InternalError)?;
            let caller_pa = caller_proc.mapping.virt_to_phys(argv_ptr as *mut usize)
                .map_err(|_| {
                    println!("[!] spawn_by_name_with_args: cannot translate argv_ptr {:#x}", argv_ptr);
                    Error::BadAddress
                })?;
            let page_offset = argv_ptr & (beetos::PAGE_SIZE - 1);
            let kern_va = beetos::phys_to_virt(caller_pa) as *const u8;
            // Safety: kern_va points to the physical page through TTBR1 linear map.
            // We only read up to use_len bytes within the page.
            let bytes_in_page = beetos::PAGE_SIZE - page_offset;
            let safe_len = use_len.min(bytes_in_page);
            unsafe {
                core::ptr::copy_nonoverlapping(kern_va, argv_buf.as_mut_ptr(), safe_len);
            }

            // Allocate a physical page for the argv data
            crate::mem::MemoryManager::with_mut(|mm| {
                let (argv_phys, zeroed) = mm.alloc_range(1, pid)?;
                if !zeroed {
                    let p = beetos::phys_to_virt(argv_phys) as *mut u8;
                    unsafe { core::ptr::write_bytes(p, 0, beetos::PAGE_SIZE); }
                }

                // Write argv data into the page
                let argv_page_kern_va = beetos::phys_to_virt(argv_phys) as *mut u8;
                unsafe {
                    core::ptr::copy_nonoverlapping(argv_buf.as_ptr(), argv_page_kern_va, safe_len);
                }

                // Map the argv page read-only into the new process
                // No W or X flags = read-only (all pages are readable by default)
                let process = self.process_mut(pid).expect("spawned process");
                process.mapping.map_page(
                    mm,
                    argv_phys,
                    beetos::ARGV_PAGE_VA as *mut usize,
                    xous::MemoryFlags::empty(),
                    true,
                ).map_err(|_| {
                    println!("[!] spawn_by_name_with_args: failed to map argv page");
                    Error::InternalError
                })?;

                Ok(safe_len)
            })?
        } else {
            0
        };

        // Set x0 = UART VA, x1 = ARGV_PAGE_VA (or 0), x2 = argv_len
        {
            let idx = pid.get() as usize - 1;
            let argv_va = if actual_argv_len > 0 { beetos::ARGV_PAGE_VA } else { 0 };
            unsafe {
                crate::arch::process::set_thread_args(
                    idx,
                    crate::arch::boot::SHELL_UART_VA,
                    argv_va,
                    actual_argv_len,
                );
            }
        }

        println!("[*] spawn_by_name_with_args: created '{}' as PID {} (argv_len={})",
            name, pid, actual_argv_len);
        Ok(pid)
    }

    pub fn pid_from_app_id(&self, app_id: AppId) -> Option<PID> {
        for process in self.processes.iter().flatten() {
            if process.app_id() == app_id {
                return Some(process.pid);
            }
        }
        None
    }
}
