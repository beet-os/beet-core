// SPDX-FileCopyrightText: 2020 Sean Cross <sean@xobs.io>
// SPDX-License-Identifier: Apache-2.0

use core::convert::TryInto;

use xous::{
    Error, MemoryAddress, MemoryFlags, MemoryMessage, MemoryRange, MemorySize, Message, MessageEnvelope,
    MessageSender, Result, SysCall, SysCallResult, ThreadPriority, CID, SID, TID,
};

use crate::irq::{interrupt_claim_user, interrupt_free};
use crate::kfuture::KernelFuture;
use crate::mem::{MemoryManager, PAGE_SIZE};
use crate::process::{current_pid, ConnectionSlot, ThreadState, IRQ_TID};
use crate::scheduler::Scheduler;
use crate::server::{SenderID, WaitingMessage};
use crate::services::SystemServices;

#[derive(PartialEq)]
#[allow(dead_code)]
enum ExecutionType {
    Blocking,
    NonBlocking,
}

/// Wake a blocked thread and deliver a result.
///
/// On hardware: if the thread has a kernel future, deposits the result
/// in its mailbox (the future polls it in `activate_current`).
/// Otherwise (or in hosted mode): sets the result directly via
/// `set_thread_result`.
///
/// In both cases, the thread state is set to `Ready`.
/// Wake a blocked thread and deliver a result.
///
/// If the thread has a kernel future:
/// - **Hardware**: deposits the result in the thread's mailbox.
///   `activate_current` will poll the future and deliver the result.
/// - **Hosted**: consumes the future and delivers the result directly
///   via `set_thread_result` (no polling needed).
///
/// If the thread has no kernel future, delivers the result directly.
#[allow(dead_code)]
pub(crate) fn wake_thread_with_result(
    ss: &mut SystemServices,
    pid: xous::PID,
    tid: TID,
    result: Result,
) {
    #[cfg(beetos)]
    {
        if let Ok(process) = ss.process_mut(pid) {
            if process.has_kernel_future(tid) {
                process.set_mailbox(tid, result);
                process.set_thread_state(tid, ThreadState::Ready);
                return;
            }
        }
    }
    // Hosted mode (or no kernel future): consume future if present,
    // deliver result directly.
    if let Ok(process) = ss.process_mut(pid) {
        process.take_kernel_future(tid);
        process.set_thread_state(tid, ThreadState::Ready);
    }
    ss.set_thread_result(pid, tid, result).ok();
}

/// Suspend the current thread with a kernel future.
///
/// Stores `future` on the thread and parks it via `WaitEvent { mask }`.
/// When the kernel posts matching notification bits, the thread is woken
/// and `activate_current` polls the future to produce the syscall result.
///
/// This is the single suspension primitive for all async syscalls.
pub(crate) fn suspend_with_future(ss: &mut SystemServices, tid: TID, future: KernelFuture, mask: usize) {
    let process = ss.current_process_mut();
    process.set_kernel_future(tid, future);
    process.set_thread_state(tid, ThreadState::WaitEvent { mask });
}

#[allow(dead_code)]
pub(crate) fn send_message(sender_tid: TID, cid: CID, message: Message) -> SysCallResult {
    SystemServices::with_mut(|ss| {
        let ConnectionSlot::Connected { sidx, permissions, .. } = ss.current_process().connection(cid)?
        else {
            return Err(Error::ServerNotFound);
        };
        let sidx = *sidx as usize;
        let server = ss.server_from_sidx(sidx).unwrap();
        if server.pid != current_pid() && !permissions.is_permitted(message.id()) {
            println!(
                "[!] Denying message send from {} to {:08x?}@{}, msgid={}",
                current_pid(),
                server.sid,
                server.pid,
                message.id(),
            );
            return Err(Error::AccessDenied);
        }
        let blocking = message.is_blocking();

        send_message_inner(ss, sender_tid, sidx, message)?;

        if blocking {
            suspend_with_future(
                ss, sender_tid,
                KernelFuture::WaitBlocking,
                crate::kfuture::EVENT_KERNEL,
            );
        } else {
            ss.set_thread_result(current_pid(), sender_tid, Result::Ok)?;
        }
        // This may or may not change to the server, depending on process priorities.
        Scheduler::with_mut(|s| s.activate_current(ss))
    })
}

pub(crate) fn send_message_inner(
    ss: &mut SystemServices,
    sender_tid: TID,
    sidx: usize,
    message: Message,
) -> core::result::Result<(), Error> {
    let server = ss.server_from_sidx(sidx).expect("server couldn't be located");
    if server.is_queue_full() {
        return Err(Error::ServerQueueFull);
    }

    let server_pid = server.pid;
    let _sid = server.sid;

    // Remember the address the message came from, in case we need to
    // return it after the borrow is through.
    let client_address = match &message {
        Message::Scalar(_) | Message::BlockingScalar(_) => None,
        Message::Move(msg) | Message::MutableBorrow(msg) | Message::Borrow(msg) => {
            MemoryAddress::new(msg.buf.as_ptr() as _)
        }
    };

    // Translate memory messages from the client process to the server
    // process. Additionally, determine whether the call is blocking. If
    // so, switch to the server context right away.
    let message = match message {
        Message::Scalar(_) | Message::BlockingScalar(_) => message,
        Message::Move(msg) => {
            let new_virt = ss.send_memory(
                msg.buf.as_mut_ptr() as *mut usize,
                server_pid,
                core::ptr::null_mut(),
                msg.buf.len(),
            )?;
            Message::Move(MemoryMessage {
                id: msg.id,
                buf: unsafe { MemoryRange::new(new_virt as usize, msg.buf.len()) }?,
                offset: msg.offset,
                valid: msg.valid,
            })
        }
        Message::MutableBorrow(msg) => {
            let new_virt = ss.lend_memory(
                msg.buf.as_mut_ptr() as *mut usize,
                server_pid,
                core::ptr::null_mut(),
                msg.buf.len(),
                true,
            )?;
            Message::MutableBorrow(MemoryMessage {
                id: msg.id,
                buf: unsafe { MemoryRange::new(new_virt as usize, msg.buf.len()) }?,
                offset: msg.offset,
                valid: msg.valid,
            })
        }
        Message::Borrow(msg) => {
            let new_virt = ss.lend_memory(
                msg.buf.as_mut_ptr() as *mut usize,
                server_pid,
                core::ptr::null_mut(),
                msg.buf.len(),
                false,
            )?;
            // println!(
            //     "Lending {} bytes from {:08x} in PID {} to {:08x} in PID {}",
            //     msg.buf.len(),
            //     msg.buf.as_mut_ptr() as usize,
            //     pid,
            //     new_virt as usize,
            //     server_pid,
            // );
            Message::Borrow(MemoryMessage {
                id: msg.id,
                buf: unsafe { MemoryRange::new(new_virt as usize, msg.buf.len()) }?,
                offset: msg.offset,
                valid: msg.valid,
            })
        }
    };

    let sender_pid = current_pid();
    // If the server has an available thread to receive the message,
    // transfer it right away.
    let server = ss.server_from_sidx_mut(sidx).expect("server couldn't be located");
    if let Some(server_tid) = server.take_available_thread() {
        // klog!(
        //     "there are threads available in PID {} to handle this message -- marking as Ready",
        //     server_pid
        // );
        let sender_idx = if message.is_blocking() {
            ss.remember_server_message(sidx, sender_pid, sender_tid, &message, client_address).inspect_err(
                |_e| {
                    klog!("error remembering server message: {:?}", _e);
                    ss.server_from_sidx_mut(sidx)
                        .expect("server couldn't be located")
                        .return_available_thread(server_tid);
                },
            )?
        } else {
            0
        };
        let sender = SenderID::new(sidx, sender_idx, Some(sender_pid));
        klog!("server connection data: sidx: {}, idx: {}, server pid: {}", sidx, sender_idx, server_pid);
        let envelope = MessageEnvelope { sender: sender.into(), body: message };

        let proc = ss.process_mut(server_pid).unwrap();
        proc.take_kernel_future(server_tid);
        proc.set_thread_state(server_tid, ThreadState::Ready);
        // This only fails if the PID does not exist, in which case we can just drop the result.
        ss.set_thread_result(server_pid, server_tid, Result::MessageEnvelope(envelope)).ok();
    } else {
        klog!("no threads available in PID {} to handle this message, so queueing", server_pid);
        // Add this message to the queue.  If the queue is full, this returns an error.
        let _queue_idx = ss.queue_server_message(sidx, sender_pid, sender_tid, message, client_address)?;
        klog!("queued into index {:x}", _queue_idx);

        // Post a notification to the server process so that any thread
        // blocked in WaitEvent (e.g. the async-rt reactor) is woken up.
        // Bit 0 = "server message available".
        let process = ss.process_mut(server_pid).expect("server process missing");
        if let Some((woken_tid, fired_bits)) = process.post_notification_bits(1) {
            ss.set_thread_result(server_pid, woken_tid, Result::Scalar1(fired_bits)).ok();
        }
    };
    Ok(())
}

#[allow(dead_code)]
fn return_memory(
    server_tid: TID,
    sender: MessageSender,
    buf: MemoryRange,
    offset: Option<MemorySize>,
    valid: Option<MemorySize>,
) -> SysCallResult {
    SystemServices::with_mut(|ss| {
        let sender = SenderID::from(sender);

        let server = ss.server_from_sidx_mut(sender.sidx).ok_or(Error::ServerNotFound)?;
        if server.pid != current_pid() {
            return Err(Error::ServerNotFound);
        }
        let queue_was_full = server.is_queue_full();
        let result = server.take_waiting_message(sender.idx, Some(&buf))?;
        if queue_was_full && !server.is_queue_full() {
            ss.wake_threads_with_state(ThreadState::RetryQueueFull { sidx: sender.sidx }, usize::MAX);
        }
        klog!("waiting message was: {:?}", result);
        match result {
            WaitingMessage::BorrowedMemory { pid, tid, client_addr } => {
                // Return the memory to the calling process
                ss.return_memory(buf.as_ptr() as _, pid, tid, client_addr.get() as _, buf.len())?;

                let return_value = Result::MemoryReturned(offset, valid);
                wake_thread_with_result(ss, pid, tid, return_value);
            }
            WaitingMessage::ForgetMemory(range) => {
                MemoryManager::with_mut(|mm| mm.unmap_range(range.as_ptr(), range.len()))?;
            }
            WaitingMessage::ScalarMessage { .. } | WaitingMessage::ScalarMessageTerminated => {
                klog!("WARNING: Tried to wait on a message that was a scalar");
                return Err(Error::DoubleFree);
            }
            WaitingMessage::None => {
                klog!("WARNING: Tried to wait on a message that didn't exist -- return memory");
                return Err(Error::DoubleFree);
            }
        };

        ss.set_thread_result(current_pid(), server_tid, Result::Ok)?;
        Scheduler::with_mut(|s| s.activate_current(ss))
    })
}

#[allow(dead_code)]
fn return_result(server_tid: TID, sender: MessageSender, return_value: Result) -> SysCallResult {
    SystemServices::with_mut(|ss| {
        let sender = SenderID::from(sender);

        let server = ss.server_from_sidx_mut(sender.sidx).ok_or(Error::ServerNotFound)?;
        if server.pid != current_pid() {
            return Err(Error::ServerNotFound);
        }
        let queue_was_full = server.is_queue_full();
        let result = server.take_waiting_message(sender.idx, None)?;
        if queue_was_full && !server.is_queue_full() {
            ss.wake_threads_with_state(ThreadState::RetryQueueFull { sidx: sender.sidx }, usize::MAX);
        }
        match result {
            WaitingMessage::ScalarMessage { pid, tid } => {
                wake_thread_with_result(ss, pid, tid, return_value);
            }
            WaitingMessage::ScalarMessageTerminated => {}
            WaitingMessage::ForgetMemory(_) => {
                klog!("WARNING: Tried to wait on a scalar message that was actually forgettingmemory");
                return Err(Error::DoubleFree);
            }
            WaitingMessage::BorrowedMemory { .. } => {
                klog!("WARNING: Tried to wait on a scalar message that was actually borrowed memory");
                return Err(Error::DoubleFree);
            }
            WaitingMessage::None => {
                klog!(
                    "WARNING ({}:{}): Tried to wait on a message that didn't exist -- return {:?}",
                    current_pid().get(),
                    server_tid,
                    result
                );
                return Err(Error::DoubleFree);
            }
        };

        ss.set_thread_result(current_pid(), server_tid, Result::Ok)?;
        Scheduler::with_mut(|s| s.activate_current(ss))
    })
}

#[allow(dead_code)]
fn receive_message(tid: TID, sid: SID, blocking: ExecutionType) -> SysCallResult {
    SystemServices::with_mut(|ss| {
        // See if there is a pending message.  If so, return immediately.
        let sidx = ss.sidx_from_sid(sid, current_pid()).ok_or(Error::ServerNotFound)?;

        {
            let server = ss.server_from_sidx_mut(sidx).ok_or(Error::ServerNotFound)?;

            // Ensure the server is for this PID
            if server.pid != current_pid() {
                return Err(Error::ServerNotFound);
            }

            let queue_was_full = server.is_queue_full();
            // If there is a pending message, return it immediately.
            if let Some(msg) = server.take_next_message(sidx) {
                klog!("waiting messages found -- returning {:x?}", msg);
                if queue_was_full && !server.is_queue_full() {
                    ss.wake_threads_with_state(ThreadState::RetryQueueFull { sidx }, usize::MAX);
                }
                return Ok(Result::MessageEnvelope(msg));
            }

            if blocking == ExecutionType::NonBlocking {
                klog!("nonblocking message -- returning None");
                return Ok(Result::None);
            }

            // Park thread on the server so send_message_inner (and
            // send_char_to_console on hardware) can find it via
            // take_available_thread for direct delivery.
            server.park_thread(tid);
        }
        // `server` borrow released here — safe to borrow `ss` again.

        klog!("did not have any waiting messages -- suspending thread {}", tid);
        suspend_with_future(
            ss, tid,
            KernelFuture::ReceiveMessage { sidx },
            crate::kfuture::EVENT_SERVER_MSG,
        );

        Scheduler::with_mut(|s| s.activate_current(ss))
    })
}

#[allow(dead_code)]
fn check_syscall_permission(call: &SysCall) -> core::result::Result<(), Error> {
    let is_permitted_by_mask = || {
        let permission_mask = SystemServices::with(|ss| ss.current_process().syscall_permissions());
        if permission_mask & (1 << call.as_args()[0]) != 0 {
            Ok(())
        } else {
            println!("[!] PID {} called {call:?} without permission", current_pid());
            Err(Error::AccessDenied)
        }
    };
    match call {
        SysCall::Yield
        | SysCall::CreateThread(..)
        | SysCall::TerminateProcess(..)
        | SysCall::GetThreadId
        | SysCall::GetProcessId
        | SysCall::JoinThread(..)
        | SysCall::RegisterEventHandler(..)
        | SysCall::AppendPanicMessage(..)
        | SysCall::GetAppId(..)
        | SysCall::AppIdToPid(..) => Ok(()),

        #[cfg(beetos)]
        SysCall::NetGetInfo => Ok(()),
        #[cfg(beetos)]
        SysCall::NetConsolePush(..) => Ok(()),
        #[cfg(beetos)]
        SysCall::NetSocketCreate
        | SysCall::NetSocketListen(..)
        | SysCall::NetSocketAccept(..)
        | SysCall::NetSocketConnect(..)
        | SysCall::NetSocketStatus(..)
        | SysCall::NetSocketSend(..)
        | SysCall::NetSocketRecv(..)
        | SysCall::NetSocketClose(..)
        | SysCall::NetPingSend(..)
        | SysCall::NetPingPoll
        | SysCall::BlockFlush(..)
        | SysCall::GetRandom => Ok(()),

        // Notification syscalls
        #[cfg(beetos)]
        SysCall::WaitEvent(..) | SysCall::PollEvent | SysCall::PostEvent(..) => Ok(()),

        #[cfg(beetos)]
        SysCall::GetBinaryName(_) => Ok(()),

        #[cfg(beetos)]
        SysCall::AcquireDisplay | SysCall::ReleaseDisplay(..) | SysCall::AcquireInputFocus(..) | SysCall::ReleaseInputFocus => Ok(()),

        // Messaging-related calls
        SysCall::CreateServer
        | SysCall::CreateServerId
        | SysCall::DestroyServer(..)
        | SysCall::Connect(..)
        | SysCall::TryConnect(..)
        | SysCall::ConnectForProcess(..)
        | SysCall::Disconnect(..)
        | SysCall::SendMessage(..)
        | SysCall::TrySendMessage(..)
        | SysCall::ReceiveMessage(..)
        | SysCall::TryReceiveMessage(..)
        | SysCall::GetRemoteProcessId(..)
        | SysCall::ReturnMemory(..)
        | SysCall::ReturnScalar1(..)
        | SysCall::ReturnScalar2(..)
        | SysCall::ReturnScalar5(..) => Ok(()),

        // XXX: Ideally this should be privileged, so that system servers with well-known SIDs can only be
        // registered by privileged processes (and the rest goes through the nameserver), but this is used in
        // various cases, like in the nameserver itself.
        SysCall::CreateServerWithAddress(..) => Ok(()),

        // Memory mapping has its own, more granular permission system
        SysCall::MapMemory(..) | SysCall::UnmapMemory(..) | SysCall::UpdateMemoryFlags(..) | SysCall::IncreaseHeap(..) => Ok(()),

        // XXX: This is somewhat sensitive, because it allows us to inject arbitrary contents (at a
        // non-controllable position) into the address space of the target PID.
        // Unfortunately this is a crucial step in the GUI framework, to allow the GUI server to see the
        // buffers of GUI apps.
        SysCall::MirrorMemoryToPid(..) => Ok(()),

        SysCall::AllowMessagesSID(sid, _messages) => {
            // If the current process owns the SID then allow the operation
            if SystemServices::with(|ss| ss.sidx_from_sid(*sid, current_pid())).is_some() {
                Ok(())
            } else {
                is_permitted_by_mask()
            }
        }
        SysCall::AllowMessagesCID(pid, cid, _messages) => {
            let server_is_current_pid = SystemServices::with(|ss| {
                let ConnectionSlot::Connected { sidx, .. } = ss.process(*pid)?.connection(*cid)? else {
                    return Err(Error::ServerNotFound);
                };
                Ok(ss.server_from_sidx(*sidx as usize).ok_or(Error::ServerNotFound)?.pid == current_pid())
            })?;
            if server_is_current_pid {
                Ok(())
            } else {
                is_permitted_by_mask()
            }
        }

        #[cfg(beetos)]
        SysCall::FlushCache(..) | SysCall::FutexWait(..) | SysCall::FutexWake(..) => Ok(()),

        SysCall::SetThreadPriority(prio) if *prio < ThreadPriority::System0 => Ok(()),

        // Privileged calls
        SysCall::SetThreadPriority(..)
        | SysCall::ClaimInterrupt(..)
        | SysCall::FreeInterrupt(..)
        | SysCall::CreateProcess(..)
        | SysCall::Shutdown(..)
        | SysCall::PowerManagement(..)
        | SysCall::TerminatePid(..)
        | SysCall::GetSystemStats(..)
        | SysCall::GetPanicMessage(..) => is_permitted_by_mask(),

        #[cfg(beetos)]
        SysCall::VirtToPhys(..) | SysCall::VirtToPhysPid(..) | SysCall::DebugCommand(..) => {
            is_permitted_by_mask()
        }

        // SpawnByName, SpawnByNameWithArgs, and WaitProcess are privileged
        #[cfg(beetos)]
        SysCall::SpawnByName(..) | SysCall::SpawnByNameWithArgs(..) | SysCall::WaitProcess(..) => is_permitted_by_mask(),

        SysCall::Invalid(..) => Err(Error::UnhandledSyscall),
    }
}

#[allow(dead_code)]
pub fn handle(tid: TID, call: SysCall) -> SysCallResult {
    klog!("KERNEL({}:{}): Syscall {:x?}", crate::arch::process::current_pid(), tid, call);
    if tid == IRQ_TID && !call.can_call_from_interrupt() {
        klog!("[!] Called {:?} that's cannot be called from the interrupt handler!", call);
        return Err(Error::InvalidSyscall);
    };
    check_syscall_permission(&call)?;
    let result = match call {
        SysCall::MapMemory(phys, virt, size, req_flags) => {
            MemoryManager::with_mut(|mm| {
                let phys_ptr = phys.map(|x| x.get()).unwrap_or_default();
                let virt_ptr = virt.map(|x| x.get() as *mut usize).unwrap_or(core::ptr::null_mut());

                // Don't let the address exceed the user area (unless it's PID 1)
                if current_pid().get() != 1 && virt.map(|x| x.get() >= beetos::USER_AREA_END).unwrap_or(false)
                {
                    klog!("Exceeded user area");
                    return Err(Error::BadAddress);

                // Don't allow mapping non-page values
                } else if size.get() & (PAGE_SIZE - 1) != 0 {
                    // println!("map: bad alignment of size {:08x}", size);
                    return Err(Error::BadAlignment);
                }

                // Don't allow RWX pages
                if req_flags.is_set(MemoryFlags::W | MemoryFlags::X) {
                    klog!("Tried to map RWX page! phys=0x{phys:08x?}, virt=0x{virt:08x?}");
                    return Err(Error::InvalidArguments);
                }

                let range = mm.map_range(phys_ptr, virt_ptr, size.get(), req_flags, true)?;

                Ok(Result::MemoryRange(range))
            })
        }
        SysCall::UnmapMemory(range) => MemoryManager::with_mut(|mm| {
            mm.check_range_accessible(range)?;
            mm.unmap_range(range.as_ptr(), range.len())?;
            Ok(Result::Ok)
        }),
        SysCall::IncreaseHeap(size) => {
            MemoryManager::with_mut(|mm| {
                if size.get() & (PAGE_SIZE - 1) != 0 {
                    return Err(Error::BadAlignment);
                }
                let range = mm.map_range(0, core::ptr::null_mut(), size.get(), MemoryFlags::POPULATE | MemoryFlags::W, true)?;
                Ok(Result::MemoryRange(range))
            })
        }
        SysCall::ClaimInterrupt(no, callback, arg) => {
            if let Ok(no) = no.try_into() {
                interrupt_claim_user(no, current_pid(), callback, arg).map(|_| Result::Ok)
            } else {
                Err(Error::InvalidArguments)
            }
        }
        SysCall::FreeInterrupt(no) => {
            if let Ok(no) = no.try_into() {
                interrupt_free(no, current_pid()).map(|_| Result::Ok)
            } else {
                Err(Error::InvalidArguments)
            }
        }
        #[cfg(beetos)]
        SysCall::FutexWait(addr, val) => MemoryManager::with(|mm| {
            use core::sync::atomic::{AtomicUsize, Ordering};

            if (addr & (core::mem::size_of::<usize>() - 1)) != 0 {
                return Err(Error::BadAlignment);
            }
            mm.check_range_accessible(unsafe { MemoryRange::new(addr, core::mem::size_of::<usize>())? })?;

            let got_val = unsafe { AtomicUsize::from_ptr(addr as *mut _).load(Ordering::SeqCst) };
            if val != got_val {
                return Err(Error::Again);
            }
            SystemServices::with_mut(|ss| {
                ss.set_thread_result(current_pid(), tid, xous::Result::Ok)?;
                suspend_with_future(
                    ss, tid,
                    KernelFuture::WaitFutex { addr },
                    crate::kfuture::EVENT_KERNEL,
                );
                Scheduler::with_mut(|s| s.activate_current(ss))
            })
        }),
        #[cfg(beetos)]
        SysCall::FutexWake(addr, n) => {
            SystemServices::with_mut(|ss| {
                let process = ss.current_process_mut();
                use crate::kfuture::KernelFuture;
                let mut remaining = n;
                for wake_tid in 1..crate::process::MAX_THREAD_COUNT {
                    if remaining == 0 {
                        break;
                    }
                    let is_futex_waiter = matches!(
                        process.kernel_future(wake_tid),
                        Some(KernelFuture::WaitFutex { addr: a }) if *a == addr
                    );
                    if is_futex_waiter {
                        process.set_mailbox(wake_tid, xous::Result::Ok);
                        process.set_thread_state(wake_tid, ThreadState::Ready);
                        remaining -= 1;
                    }
                }
            });
            Ok(Result::Ok)
        }
        SysCall::Yield => SystemServices::with_mut(|ss| {
            ss.set_thread_result(current_pid(), tid, Result::Ok)?;
            Scheduler::with_mut(|s| {
                let prio = ss.current_process().thread_priority(tid);
                s.yield_thread(current_pid(), tid, prio);
                s.activate_current(ss)
            })
        }),
        SysCall::SetThreadPriority(priority) => {
            if priority == ThreadPriority::Idle {
                Err(Error::InvalidArguments)
            } else {
                SystemServices::with_mut(|ss| {
                    ss.current_process_mut().set_thread_priority(tid, priority);
                    ss.set_thread_result(current_pid(), tid, Result::Ok)?;
                    Scheduler::with_mut(|s| s.activate_current(ss))
                })
            }
        }
        SysCall::ReceiveMessage(sid) => receive_message(tid, sid, ExecutionType::Blocking),
        SysCall::TryReceiveMessage(sid) => receive_message(tid, sid, ExecutionType::NonBlocking),
        SysCall::CreateThread(thread_init) => {
            SystemServices::with_mut(|ss| ss.create_thread(tid, thread_init).map(Result::ThreadID))
        }
        SysCall::CreateProcess(process_init) => {
            SystemServices::with_mut(|ss| ss.create_process(process_init).map(Result::NewProcess))
        }
        SysCall::CreateServerWithAddress(name, initial_range) => SystemServices::with_mut(|ss| {
            ss.create_server_with_address(name, initial_range).map(Result::NewServerID)
        }),
        SysCall::CreateServer => SystemServices::with_mut(|ss| {
            {
                let sid = ss.create_server_id()?;
                ss.create_server_with_address(sid, 0..0)
            }
            .map(Result::NewServerID)
        }),
        SysCall::CreateServerId => SystemServices::with_mut(|ss| ss.create_server_id().map(Result::ServerID)),
        SysCall::TryConnect(sid) => {
            SystemServices::with_mut(|ss| ss.connect_to_server(current_pid(), sid).map(Result::ConnectionID))
        }
        SysCall::ReturnMemory(sender, buf, offset, valid) => return_memory(tid, sender, buf, offset, valid),
        SysCall::ReturnScalar1(sender, arg) => return_result(tid, sender, Result::Scalar1(arg)),
        SysCall::ReturnScalar2(sender, arg1, arg2) => return_result(tid, sender, Result::Scalar2(arg1, arg2)),
        SysCall::ReturnScalar5(sender, arg1, arg2, arg3, arg4, arg5) => {
            return_result(tid, sender, Result::Scalar5(arg1, arg2, arg3, arg4, arg5))
        }
        SysCall::TrySendMessage(cid, message) => send_message(tid, cid, message),
        SysCall::TerminateProcess(ret) => {
            // Reclaim any TCP sockets the exiting process still owns so
            // a crash-looping app can't exhaust the socket table.
            #[cfg(all(beetos, feature = "platform-qemu-virt"))]
            crate::platform::qemu_virt::tcp::cleanup_for_pid(current_pid().get() as u32);
            SystemServices::with_mut(|ss| ss.terminate_current_process(ret))
        }
        SysCall::TerminatePid(pid, exit_code) => {
            let result = SystemServices::with_mut(|ss| {
                // The process is self-terminating, which is equivalent to TerminateProcess
                if pid == ss.current_process().pid {
                    Err(Error::InvalidArguments)
                } else {
                    ss.terminate_process(tid, pid, exit_code)
                }
            });
            // Same TCP-socket reclamation as TerminateProcess, but only
            // once the kill actually went through.
            #[cfg(all(beetos, feature = "platform-qemu-virt"))]
            if result.is_ok() {
                crate::platform::qemu_virt::tcp::cleanup_for_pid(pid.get() as u32);
            }
            result
        }
        SysCall::Shutdown(_) => SystemServices::with_mut(|ss| ss.shutdown().map(|_| Result::Ok)),
        SysCall::GetProcessId => Ok(Result::ProcessID(current_pid())),

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetGetInfo => {
            use crate::platform::qemu_virt::net_stack;
            use crate::platform::qemu_virt::net;
            let ip = net_stack::get_ip();
            let ip_u32 = u32::from_be_bytes(ip);
            let mac = net::get_mac().unwrap_or([0u8; 6]);
            let mac_hi = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]);
            let mac_lo = u32::from_be_bytes([mac[4], mac[5], 0, 0]);
            // Slot 4 carries the 100 Hz uptime tick so userspace can
            // build real (wall-clock) deadlines around non-blocking
            // socket polls — yield-spin counts are CPU-speed dependent
            // and useless as a time base.
            let ticks = crate::platform::qemu_virt::timer::tick_count() as usize;
            Ok(Result::Scalar5(ip_u32 as usize, mac_hi as usize, mac_lo as usize, ticks, 0))
        }

        #[cfg(all(beetos, not(feature = "platform-qemu-virt")))]
        SysCall::NetGetInfo => {
            // No network stack on this platform yet.
            Ok(Result::Scalar5(0, 0, 0, 0, 0))
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetConsolePush(len, w0, w1, w2, w3) => {
            // Bytes arrive packed inline in the syscall args, not as a
            // userspace pointer — so we don't have to fight PAN (which
            // AArch64v8.1 asserts on EL0→EL1 entry) or the
            // copy-from-user dance to read them. Unpack into a small
            // kernel buffer and hand to the TCP ring.
            let len = len.min(32);
            let mut stage = [0u8; 32];
            let words = [w0 as u64, w1 as u64, w2 as u64, w3 as u64];
            for (i, word) in words.iter().enumerate() {
                for j in 0..8 {
                    stage[i * 8 + j] = ((word >> (j * 8)) & 0xff) as u8;
                }
            }
            crate::platform::qemu_virt::tcp::console_push(&stage[..len]);
            Ok(Result::Ok)
        }

        #[cfg(all(beetos, not(feature = "platform-qemu-virt")))]
        SysCall::NetConsolePush(_, _, _, _, _) => {
            // No network stack on this platform yet.
            Ok(Result::Ok)
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketCreate => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            match tcp::user_create(pid) {
                Some(sock) => Ok(Result::Scalar1(sock)),
                None => Err(Error::OutOfMemory),
            }
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketListen(sock, port) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            tcp::user_listen(sock, port as u16, pid).map(|_| Result::Ok).map_err(|_| Error::InvalidArguments)
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketAccept(sock) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            // NO_PENDING (= u32::MAX) signals "no connection ready" —
            // userspace polls until it sees a real child id.
            let child = tcp::user_accept(sock, pid).unwrap_or(tcp::NO_PENDING as usize);
            Ok(Result::Scalar1(child))
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketConnect(sock, peer_ip_be, peer_port) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            let ip_bytes = (peer_ip_be as u32).to_be_bytes();
            tcp::user_connect(sock, ip_bytes, peer_port as u16, pid)
                .map(|_| Result::Ok)
                .map_err(|_| Error::InvalidArguments)
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketStatus(sock) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            let code = tcp::user_state(sock, pid).map(|s| s.code()).unwrap_or(255);
            let rx = tcp::user_rx_len(sock, pid);
            Ok(Result::Scalar2(code, rx))
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketSend(sock, len, w0, w1, w2, w3) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            // Same PAN-safe inline unpacking as NetConsolePush.
            let len = len.min(32);
            let mut stage = [0u8; 32];
            let words = [w0 as u64, w1 as u64, w2 as u64, w3 as u64];
            for (i, word) in words.iter().enumerate() {
                for j in 0..8 {
                    stage[i * 8 + j] = ((word >> (j * 8)) & 0xff) as u8;
                }
            }
            let n = tcp::user_send(sock, &stage[..len], pid).unwrap_or(0);
            Ok(Result::Scalar1(n))
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketRecv(sock, max_len) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            let max_len = max_len.min(32);
            let mut stage = [0u8; 32];
            let n = tcp::user_recv(sock, &mut stage[..max_len], pid).unwrap_or(0);
            // Pack bytes back into four u64s little-endian.
            let mut words = [0u64; 4];
            for i in 0..4 {
                let mut w = 0u64;
                for j in 0..8 {
                    w |= (stage[i * 8 + j] as u64) << (j * 8);
                }
                words[i] = w;
            }
            Ok(Result::Scalar5(n, words[0] as usize, words[1] as usize, words[2] as usize, words[3] as usize))
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetSocketClose(sock) => {
            use crate::platform::qemu_virt::tcp;
            let pid = current_pid().get() as u32;
            tcp::user_close(sock, pid).map(|_| Result::Ok).map_err(|_| Error::InvalidArguments)
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetPingSend(dst_ip_be, seq) => {
            use crate::platform::qemu_virt::net_stack;
            let dst = (dst_ip_be as u32).to_be_bytes();
            let sent = net_stack::ping_send(dst, seq as u16);
            Ok(Result::Scalar1(sent as usize))
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::NetPingPoll => {
            use crate::platform::qemu_virt::net_stack;
            match net_stack::ping_poll() {
                Some((ip, seq)) => {
                    Ok(Result::Scalar2(u32::from_be_bytes(ip) as usize, seq as usize))
                }
                None => Ok(Result::Scalar2(0, 0)),
            }
        }

        #[cfg(all(beetos, feature = "platform-qemu-virt"))]
        SysCall::BlockFlush(lba, n_blocks) => {
            use crate::platform::qemu_virt::blk;
            // Only the block service (registered at boot via
            // blk::set_flush_owner) gets to drive virtio-blk writes,
            // since the syscall reaches through the mirror by physical
            // address without any user pointer. flush_owner() is 0 when
            // nothing registered, which can never equal a live PID.
            if current_pid().get() as usize != blk::flush_owner() {
                return Err(Error::AccessDenied);
            }
            if n_blocks == 0 || n_blocks > u32::MAX as usize {
                return Err(Error::InvalidArguments);
            }
            blk::flush_mirror(lba as u64, n_blocks as u32)
                .map(|_| Result::Ok)
                .map_err(|_| Error::InvalidArguments)
        }

        #[cfg(beetos)]
        SysCall::GetRandom => {
            // Route through the platform seam, NOT arch::rand directly:
            // on QEMU the platform layer upgrades this to virtio-rng
            // (real host entropy). These bytes feed the AES-GCM nonces
            // in api/cryptblock, so they deserve the best source the
            // platform has.
            let r0 = ((crate::platform::rand::get_u32() as usize) << 32)
                | crate::platform::rand::get_u32() as usize;
            let r1 = ((crate::platform::rand::get_u32() as usize) << 32)
                | crate::platform::rand::get_u32() as usize;
            Ok(Result::Scalar2(r0, r1))
        }

        #[cfg(all(beetos, not(feature = "platform-qemu-virt")))]
        SysCall::NetSocketCreate | SysCall::NetSocketListen(_, _) | SysCall::NetSocketAccept(_)
        | SysCall::NetSocketConnect(_, _, _) | SysCall::NetSocketStatus(_)
        | SysCall::NetSocketSend(_, _, _, _, _, _) | SysCall::NetSocketRecv(_, _)
        | SysCall::NetSocketClose(_)
        | SysCall::NetPingSend(_, _) | SysCall::NetPingPoll
        | SysCall::BlockFlush(_, _) => Err(Error::InvalidSyscall),

        #[cfg(beetos)]
        SysCall::GetBinaryName(index) => {
            use crate::arch::boot;

            match boot::get_user_binary_at(index) {
                Some(name) => {
                    let bytes = name.as_bytes();
                    let mut words = [0usize; 4];
                    let ws = core::mem::size_of::<usize>();

                    for (i, chunk) in bytes.chunks(ws).enumerate() {
                        if i >= 4 { break; }
                        let mut buf = [0u8; 8];
                        buf[..chunk.len()].copy_from_slice(chunk);
                        words[i] = usize::from_le_bytes(buf);
                    }

                    Ok(Result::Scalar5(words[0], words[1], words[2], words[3], 0))
                }
                None => Err(Error::ProcessNotFound),
            }
        }

        SysCall::GetThreadId => Ok(Result::ThreadID(tid)),

        SysCall::Connect(sid) => {
            // In BeetOS, Connect behaves like TryConnect: return ServerNotFound
            // immediately instead of blocking the thread forever. The original
            // Xous behavior (retry until the server appears) requires a name
            // server and full service infrastructure that BeetOS doesn't have yet.
            // Without this, any process whose std runtime connects to a missing
            // server (e.g. xous-name-server) will hang permanently.
            SystemServices::with_mut(|ss| {
                ss.connect_to_server(current_pid(), sid).map(Result::ConnectionID)
            })
        }
        SysCall::ConnectForProcess(pid, sid) => {
            SystemServices::with_mut(|ss| ss.connect_to_server(pid, sid).map(Result::ConnectionID))
        }
        SysCall::SendMessage(cid, message) => {
            let result = send_message(tid, cid, message);
            match result {
                Ok(o) => Ok(o),
                Err(Error::ServerQueueFull) => SystemServices::with_mut(|ss| {
                    let ConnectionSlot::Connected { sidx, .. } = ss.current_process().connection(cid)? else {
                        return Err(Error::ServerNotFound);
                    };
                    let sidx = *sidx as usize;
                    ss.retry_syscall(tid, ThreadState::RetryQueueFull { sidx })
                }),
                Err(e) => Err(e),
            }
        }
        SysCall::Disconnect(cid) => {
            SystemServices::with_mut(|ss| ss.disconnect_from_server(cid).and(Ok(Result::Ok)))
        }
        SysCall::DestroyServer(sid) => {
            SystemServices::with_mut(|ss| ss.destroy_server(current_pid(), sid).and(Ok(Result::Ok)))
        }
        SysCall::JoinThread(other_tid) => SystemServices::with_mut(|ss| ss.join_thread(tid, other_tid)),
        SysCall::GetRemoteProcessId(cid) => SystemServices::with_mut(|ss| {
            let ConnectionSlot::Connected { sidx, .. } = ss.current_process().connection(cid)? else {
                return Err(Error::ServerNotFound);
            };
            let sidx = *sidx as usize;
            Ok(Result::ProcessID(ss.server_from_sidx(sidx).ok_or(Error::ServerNotFound)?.pid))
        }),
        SysCall::UpdateMemoryFlags(range, flags, pid) => {
            // We do not yet support modifying flags for other processes.
            if pid.is_some() {
                return Err(Error::ProcessNotChild);
            }

            MemoryManager::with_mut(|mm| mm.update_memory_flags(range, flags))?;
            Ok(Result::Ok)
        }
        #[cfg(beetos)]
        SysCall::VirtToPhys(vaddr) => crate::arch::mem::MemoryMapping::current()
            .virt_to_phys((vaddr & !(PAGE_SIZE - 1)) as *mut usize)
            .map(|pa| Result::Scalar1(pa | vaddr & (PAGE_SIZE - 1))),
        #[cfg(beetos)]
        SysCall::VirtToPhysPid(pid, vaddr) => {
            let pa = SystemServices::with_mut(|ss| {
                let Ok(proc) = ss.process(pid) else {
                    return Err(Error::ProcessNotFound);
                };
                proc.mapping.virt_to_phys((vaddr & !(PAGE_SIZE - 1)) as *mut usize)
            })?;
            Ok(Result::Scalar1(pa | vaddr & (PAGE_SIZE - 1)))
        }
        SysCall::GetAppId(pid) => match SystemServices::with(|ss| ss.process(pid).ok().map(|p| p.app_id())) {
            Some(app_id) => Ok(Result::Scalar5(
                u32::from_le_bytes(app_id.0[0..4].try_into().unwrap()) as usize,
                u32::from_le_bytes(app_id.0[4..8].try_into().unwrap()) as usize,
                u32::from_le_bytes(app_id.0[8..12].try_into().unwrap()) as usize,
                u32::from_le_bytes(app_id.0[12..16].try_into().unwrap()) as usize,
                1,
            )),
            None => Ok(Result::Scalar5(0, 0, 0, 0, 0)),
        },
        SysCall::AllowMessagesSID(sid, messages) => SystemServices::with_mut(|ss| {
            let Some(sidx) = ss.sidx_from_sid(sid, current_pid()) else {
                return Err(Error::ServerNotFound);
            };
            ss.server_from_sidx_mut(sidx).unwrap().default_permissions.add(messages)
        }),
        SysCall::AllowMessagesCID(pid, cid, messages) => {
            if cid < 2 {
                return Err(Error::ServerNotFound);
            }
            SystemServices::with_mut(|ss| match ss.process_mut(pid)?.connection_mut(cid) {
                Ok(ConnectionSlot::Connected { permissions, .. }) => permissions.add(messages),
                _ => Err(Error::ServerNotFound),
            })
        }
        #[cfg(beetos)]
        SysCall::FlushCache(mem, op) => MemoryManager::with(|mm| {
            mm.check_range_accessible(mem)?;
            crate::arch::mem::MemoryMapping::current().flush_cache(mem, op)?;
            Ok(Result::Ok)
        }),
        #[cfg(beetos)]
        SysCall::PowerManagement(dram) => {
            crate::platform::set_dram_idle_mode(dram);
            Ok(Result::Ok)
        }
        SysCall::AppIdToPid(app_id) => {
            let pid = SystemServices::with(|ss| ss.pid_from_app_id(app_id));
            if let Some(pid) = pid {
                return Ok(Result::ProcessID(pid));
            }

            Ok(Result::None)
        }
        #[cfg(beetos)]
        SysCall::MirrorMemoryToPid(mem, pid) => {
            let source_mapping = crate::arch::mem::MemoryMapping::current();
            let mem_phys = source_mapping.virt_to_phys(mem.as_ptr() as *const usize)?;

            // Check that the process owns the memory range both virtually and physically continuous
            for (i, page) in
                (mem.as_ptr() as usize..(mem.as_ptr() as usize + mem.len())).step_by(PAGE_SIZE).enumerate()
            {
                let page_phys = source_mapping.virt_to_phys(page as *const usize)?;
                let page_phys_expected = mem_phys + i * PAGE_SIZE;
                klog!(
                    "mirror_memory_to_pid: checking page {:08x}: got {:08x}, expected {:08x}",
                    page,
                    page_phys,
                    page_phys_expected
                );

                if page_phys != page_phys_expected {
                    klog!("mirror_memory_to_pid: physical range is not continuous");
                    return Err(Error::AccessDenied);
                }
            }

            let mirror_range = MemoryManager::with_mut(|mm| {
                SystemServices::with_mut(|ss| {
                    ss.process(pid)?.mapping.activate();
                    let res = mm.map_range_readonly_mirror(pid, mem_phys, mem.len());
                    source_mapping.activate();
                    res
                })
            })?;

            Ok(Result::MemoryRange(mirror_range))
        }
        #[cfg(beetos)]
        #[cfg(not(feature = "production"))]
        SysCall::DebugCommand(mut buffer, cmd) => MemoryManager::with_mut(|mm| {
            mm.check_range_accessible(buffer)?;
            let start = (buffer.as_ptr() as usize) & !(PAGE_SIZE - 1);
            let end = ((buffer.as_ptr() as usize) + buffer.len()).next_multiple_of(PAGE_SIZE);
            for addr in (start..end).step_by(PAGE_SIZE) {
                mm.ensure_page_exists(addr as _)?;
            }
            let mut buffer = crate::debug::BufStr::from(&mut buffer);
            crate::debug::commands::debug_command(cmd, &mut buffer);
            Ok(Result::Scalar1(buffer.as_slice().len()))
        }),
        SysCall::GetSystemStats(stat) => match stat {
            xous::SystemStat::FreeMemory => Ok(Result::Scalar1(MemoryManager::with(|mm| mm.ram_free()))),
            xous::SystemStat::IsSystemLowOnMemory => {
                Ok(Result::Scalar1(MemoryManager::with(|mm| mm.low_memory()) as usize))
            }
        },
        SysCall::RegisterEventHandler(event, sid, id) => SystemServices::with_mut(|ss| {
            ss.current_process_mut().set_event_handler(event, sid, id).and(Ok(Result::Ok))
        }),

        SysCall::AppendPanicMessage(len, a1, a2, a3, a4, a5, a6) => {
            let mut buf = [0u8; size_of::<usize>() * 6];
            let len = len.min(buf.len());

            let mut num_bytes = 0;
            for word in [a1, a2, a3, a4, a5, a6].iter() {
                for byte in word.to_le_bytes() {
                    buf[num_bytes] = byte;
                    num_bytes += 1;
                    if num_bytes >= len {
                        break;
                    }
                }
            }

            SystemServices::with_mut(|ss| ss.append_panic_message(&buf[..len]).and(Ok(Result::Ok)))
        }

        SysCall::GetPanicMessage(buf) => MemoryManager::with(|mm| {
            mm.check_range_accessible(buf)?;

            SystemServices::with_mut(|ss| {
                let (pid, msg) = ss.take_panic_message();
                let pid_val = pid.map(|p| p.get() as usize).unwrap_or(0);

                let copy_len = msg.len().min(buf.len());
                if copy_len > 0 {
                    let user_buf = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), copy_len) };
                    user_buf.copy_from_slice(&msg[..copy_len]);
                }

                Ok(Result::Scalar2(pid_val, copy_len))
            })
        }),

        #[cfg(beetos)]
        SysCall::SpawnByName(a0, a1, a2, a3) => {
            let args = [a0, a1, a2, a3];
            let name_bytes = xous::unpack_name_from_usize(&args);
            let name = core::str::from_utf8(name_bytes).map_err(|_| Error::InvalidString)?;
            // Refuse to respawn services the kernel boots itself — a
            // duplicate would loop forever on a SID it cannot
            // register, holding any caller blocked in WaitProcess.
            #[cfg(beetos)]
            if crate::arch::boot::INTERNAL_SERVICES.contains(&name) {
                return Err(Error::ProcessNotFound);
            }
            SystemServices::with_mut(|ss| {
                ss.spawn_by_name(name).map(Result::ProcessID)
            })
        }
        #[cfg(beetos)]
        SysCall::SpawnByNameWithArgs(name0, name1, argv_ptr, argv_len) => {
            // Unpack name from 2 usizes (max 16 bytes)
            let name_words = [name0, name1];
            let ws = core::mem::size_of::<usize>();
            let name_raw = unsafe { core::slice::from_raw_parts(name_words.as_ptr() as *const u8, 2 * ws) };
            let name_end = name_raw.iter().position(|&b| b == 0).unwrap_or(2 * ws);
            let name = core::str::from_utf8(&name_raw[..name_end]).map_err(|_| Error::InvalidString)?;
            if crate::arch::boot::INTERNAL_SERVICES.contains(&name) {
                return Err(Error::ProcessNotFound);
            }
            SystemServices::with_mut(|ss| {
                ss.spawn_by_name_with_args(name, argv_ptr, argv_len).map(Result::ProcessID)
            })
        }
        #[cfg(beetos)]
        SysCall::WaitProcess(target_pid) => SystemServices::with_mut(|ss| {
            // If the process already exited, return immediately.
            if ss.process(target_pid).is_err() {
                return Ok(Result::Scalar1(0));
            }

            // Suspend with a kernel future. The wake path in
            // terminate_current_process scans kernel_futures for
            // WaitProcessExit { target_pid } and deposits the exit
            // code into the mailbox.
            suspend_with_future(
                ss, tid,
                KernelFuture::WaitProcessExit { target_pid },
                crate::kfuture::EVENT_KERNEL,
            );

            Scheduler::with_mut(|s| s.activate_current(ss))
        }),

        #[cfg(beetos)]
        SysCall::WaitEvent(mask) => {
            if mask == 0 {
                return Err(Error::InvalidArguments);
            }
            SystemServices::with_mut(|ss| {
                let process = ss.current_process_mut();
                let fired = process.take_notification_bits_masked(mask);
                if fired != 0 {
                    // Bits already pending — return immediately.
                    return Ok(Result::Scalar1(fired));
                }
                // No matching bits — block the thread.
                ss.set_thread_result(current_pid(), tid, Result::Ok)?;
                ss.current_process_mut().set_thread_state(tid, ThreadState::WaitEvent { mask });
                Scheduler::with_mut(|s| s.activate_current(ss))
            })
        }
        #[cfg(beetos)]
        SysCall::PollEvent => {
            SystemServices::with_mut(|ss| {
                let bits = ss.current_process_mut().take_notification_bits();
                Ok(Result::Scalar1(bits))
            })
        }
        #[cfg(beetos)]
        SysCall::PostEvent(target_pid, bits) => {
            if bits == 0 {
                return Ok(Result::Ok);
            }
            // Access control: a process may only post notification bits to
            // itself.  The kernel posts to other processes directly via
            // post_notification_bits() without going through this syscall.
            if target_pid != current_pid() {
                return Err(Error::AccessDenied);
            }
            SystemServices::with_mut(|ss| {
                let process = ss.process_mut(target_pid)?;
                if let Some((woken_tid, fired_bits)) = process.post_notification_bits(bits) {
                    ss.set_thread_result(target_pid, woken_tid, Result::Scalar1(fired_bits)).ok();
                }
                Ok(Result::Ok)
            })
        }

        #[cfg(beetos)]
        SysCall::AcquireDisplay => SystemServices::with_mut(|ss| {
            let pid = crate::arch::process::current_pid();
            let (row, col) = ss.display_cursor();
            if ss.try_acquire_display(pid, tid) {
                // Granted immediately — return cursor position.
                Ok(xous::Result::Scalar2(row, col))
            } else {
                // Queued — suspend until release_display wakes us.
                suspend_with_future(
                    ss, tid,
                    KernelFuture::WaitDisplay,
                    crate::kfuture::EVENT_KERNEL,
                );
                Scheduler::with_mut(|s| s.activate_current(ss))
            }
        }),

        #[cfg(beetos)]
        SysCall::ReleaseDisplay(row, col) => SystemServices::with_mut(|ss| {
            ss.release_display(row, col);
            Ok(xous::Result::Ok)
        }),

        #[cfg(beetos)]
        SysCall::AcquireInputFocus(a, b, c, d) => SystemServices::with_mut(|ss| {
            let pid = crate::arch::process::current_pid();
            let sid_words = [a, b, c, d];
            let mut buf = [0u8; crate::services::INPUT_BUF_CAP];
            match ss.acquire_input_focus(pid, sid_words, &mut buf) {
                Ok(n) => {
                    // Drain buffered chars to the newly-registered SID. On
                    // platforms without a kernel input source the buffer is
                    // always empty, so this is a no-op loop.
                    for i in 0..n {
                        crate::arch::irq::deliver_char_to_sid(ss, sid_words, buf[i]);
                    }
                    Ok(xous::Result::Ok)
                }
                Err(e) => Err(e),
            }
        }),

        #[cfg(beetos)]
        SysCall::ReleaseInputFocus => SystemServices::with_mut(|ss| {
            ss.release_input_focus();
            Ok(xous::Result::Ok)
        }),

        _ => Err(Error::UnhandledSyscall),
    };
    klog!(
        " -> ({}:{}) {:x?}",
        crate::arch::process::current_pid(),
        crate::arch::process::Process::current().current_tid(),
        result
    );
    result
}
