// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! virtio-input keyboard driver for QEMU virt.
//!
//! Handles keyboard input via the virtio-input device (device ID 18).
//! Key events (EV_KEY, value=1) are converted from Linux evdev keycodes
//! to ASCII and fed into the same input path as UART RX characters.
//!
//! QEMU must be launched with `-device virtio-keyboard-device`.
//!
//! Protocol:
//! - Queue 0 (eventq): device→driver, receives `virtio_input_event` structs.
//!   Each descriptor is 8 bytes: type(u16) + code(u16) + value(u32), LE.
//! - Queue 1 (statusq): driver→device (LED updates), not used here.
//!
//! Reference: virtio spec v1.2 §5.8 (Input Device).

use super::virtio::{
    self, Virtqueue, VIRTIO_IRQ_BASE, NUM_TRANSPORTS, TRANSPORT_SIZE, VIRTQ_DESC_F_WRITE,
};

/// virtio-input device ID.
const VIRTIO_INPUT_DEVICE_ID: u32 = 18;

/// EV_KEY event type (key up/down).
const EV_KEY: u16 = 1;
/// Key-down event value.
const VAL_DOWN: u32 = 1;

/// Left/right shift keycodes (Linux evdev).
const KEY_LEFTSHIFT:  u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;

/// Number of event slots in the eventq.
const QUEUE_SIZE: u16 = 64;

/// Size of one virtio_input_event in bytes (type + code + value).
const EVENT_SIZE: usize = 8;

// ─────────────────────────────────────────────────────────────────────────────
// Device state
// ─────────────────────────────────────────────────────────────────────────────

struct InputDev {
    base_va: usize,
    irq:     u32,
    eventq:  Virtqueue,
    shift:   bool,
}

static mut INPUT_DEV: Option<InputDev> = None;

// ─────────────────────────────────────────────────────────────────────────────
// Static DMA buffers (kernel BSS — physically contiguous)
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C, align(16384))]
struct VqBuf([u8; Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]);

static mut EVENTQ_BUF: VqBuf =
    VqBuf([0u8; Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]);

/// One 8-byte event buffer per queue slot.
/// Descriptor index i maps directly to EVENT_BUFS[i].
static mut EVENT_BUFS: [[u8; EVENT_SIZE]; QUEUE_SIZE as usize] =
    [[0u8; EVENT_SIZE]; QUEUE_SIZE as usize];

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Probe all virtio MMIO transports and initialize the first keyboard device found.
/// Called during platform init.
pub fn probe_and_init(virtio_base_va: usize) {
    unsafe {
        for i in 0..NUM_TRANSPORTS {
            let base_va = virtio_base_va + i * TRANSPORT_SIZE;
            if virtio::probe_transport(base_va) == Some(VIRTIO_INPUT_DEVICE_ID) {
                let irq = VIRTIO_IRQ_BASE + i as u32;
                init(base_va, irq);
                return;
            }
        }
    }
}

/// Return the GIC IRQ number of the input device, or None if not initialized.
pub fn irq_number() -> Option<u32> {
    unsafe { (*(&raw const INPUT_DEV)).as_ref().map(|d| d.irq) }
}

/// Acknowledge the virtio-input interrupt (call once per IRQ).
pub fn ack_irq() {
    unsafe {
        if let Some(dev) = (*(&raw const INPUT_DEV)).as_ref() {
            virtio::ack_interrupt(dev.base_va);
        }
    }
}

/// Pop one pending key-press character from the event queue.
///
/// Returns `Some(c)` for a printable key-down event, `None` when the queue
/// is empty or the event produces no ASCII character. Call in a loop until
/// `None` to drain all pending characters after `ack_irq()`.
/// Pop the next printable key-press from the event queue.
///
/// Internally drains EV_SYN, key-up, and other non-character events so
/// callers receive `None` only when the queue is truly empty. Call in a
/// loop until `None` to drain all pending characters per IRQ.
pub fn get_char() -> Option<u8> {
    unsafe {
        let dev = (*(&raw mut INPUT_DEV)).as_mut()?;

        loop {
            // Return None (queue empty) when no more events.
            let (desc_idx, _len) = dev.eventq.pop_used()?;

            // Read event. Descriptor index == buffer index (identity mapping).
            let buf = &EVENT_BUFS[desc_idx as usize];
            let ev_type  = u16::from_le_bytes([buf[0], buf[1]]);
            let ev_code  = u16::from_le_bytes([buf[2], buf[3]]);
            let ev_value = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

            // Track shift state (key-down and key-up both matter).
            if ev_code == KEY_LEFTSHIFT || ev_code == KEY_RIGHTSHIFT {
                dev.shift = ev_value == VAL_DOWN;
            }

            // Re-post descriptor so QEMU can reuse it.
            dev.eventq.push_avail(desc_idx);
            virtio::notify(dev.base_va, 0);

            // EV_KEY down → convert to ASCII and return.
            // EV_SYN / key-up / repeat → continue draining.
            if ev_type == EV_KEY && ev_value == VAL_DOWN {
                if let Some(c) = keycode_to_ascii(ev_code, dev.shift) {
                    return Some(c);
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Initialization
// ─────────────────────────────────────────────────────────────────────────────

unsafe fn init(base_va: usize, irq: u32) {
    if virtio::init_device(base_va, 0).is_none() {
        return;
    }

    // Set up eventq (queue 0).
    let buf_va = EVENTQ_BUF.0.as_mut_ptr() as usize;
    let buf_pa = beetos::virt_to_phys(buf_va);
    let eventq = Virtqueue::init(buf_va, buf_pa, QUEUE_SIZE, beetos::PAGE_SIZE);
    virtio::setup_queue(base_va, 0, &eventq, beetos::PAGE_SIZE);

    let mut dev = InputDev { base_va, irq, eventq, shift: false };

    // Pre-populate all descriptors: each points to its own EVENT_BUFS slot.
    // Descriptor index i maps to EVENT_BUFS[i] (identity mapping).
    for i in 0..(QUEUE_SIZE as usize) {
        let desc_idx = dev.eventq.alloc_desc().expect("eventq descriptor");
        let buf_pa   = beetos::virt_to_phys(EVENT_BUFS[i].as_ptr() as usize) as u64;

        let d = &mut *dev.eventq.desc.add(desc_idx as usize);
        d.addr  = buf_pa;
        d.len   = EVENT_SIZE as u32;
        d.flags = VIRTQ_DESC_F_WRITE; // device writes events into the buffer
        d.next  = 0;

        dev.eventq.push_avail(desc_idx);
    }

    virtio::notify(base_va, 0);
    virtio::driver_ok(base_va);

    // Enable IRQ in GIC.
    super::gic::enable_irq(irq);

    INPUT_DEV = Some(dev);

    super::uart::puts("virtio-input: keyboard ready\n");
}

// ─────────────────────────────────────────────────────────────────────────────
// Keycode → ASCII (US QWERTY)
// ─────────────────────────────────────────────────────────────────────────────

fn keycode_to_ascii(code: u16, shift: bool) -> Option<u8> {
    let c: u8 = match code {
        1       => 0x1b,  // ESC
        14      => 0x7f,  // Backspace → DEL
        15      => b'\t', // Tab
        28 | 96 => b'\n', // Enter / KP Enter
        57      => b' ',  // Space

        // Letters (a–z / A–Z)
        16 => if shift { b'Q' } else { b'q' },
        17 => if shift { b'W' } else { b'w' },
        18 => if shift { b'E' } else { b'e' },
        19 => if shift { b'R' } else { b'r' },
        20 => if shift { b'T' } else { b't' },
        21 => if shift { b'Y' } else { b'y' },
        22 => if shift { b'U' } else { b'u' },
        23 => if shift { b'I' } else { b'i' },
        24 => if shift { b'O' } else { b'o' },
        25 => if shift { b'P' } else { b'p' },
        30 => if shift { b'A' } else { b'a' },
        31 => if shift { b'S' } else { b's' },
        32 => if shift { b'D' } else { b'd' },
        33 => if shift { b'F' } else { b'f' },
        34 => if shift { b'G' } else { b'g' },
        35 => if shift { b'H' } else { b'h' },
        36 => if shift { b'J' } else { b'j' },
        37 => if shift { b'K' } else { b'k' },
        38 => if shift { b'L' } else { b'l' },
        44 => if shift { b'Z' } else { b'z' },
        45 => if shift { b'X' } else { b'x' },
        46 => if shift { b'C' } else { b'c' },
        47 => if shift { b'V' } else { b'v' },
        48 => if shift { b'B' } else { b'b' },
        49 => if shift { b'N' } else { b'n' },
        50 => if shift { b'M' } else { b'm' },

        // Digits
        2  => if shift { b'!' } else { b'1' },
        3  => if shift { b'@' } else { b'2' },
        4  => if shift { b'#' } else { b'3' },
        5  => if shift { b'$' } else { b'4' },
        6  => if shift { b'%' } else { b'5' },
        7  => if shift { b'^' } else { b'6' },
        8  => if shift { b'&' } else { b'7' },
        9  => if shift { b'*' } else { b'8' },
        10 => if shift { b'(' } else { b'9' },
        11 => if shift { b')' } else { b'0' },

        // Punctuation
        12 => if shift { b'_'  } else { b'-'  },
        13 => if shift { b'+'  } else { b'='  },
        26 => if shift { b'{'  } else { b'['  },
        27 => if shift { b'}'  } else { b']'  },
        39 => if shift { b':'  } else { b';'  },
        40 => if shift { b'"'  } else { b'\'' },
        41 => if shift { b'~'  } else { b'`'  },
        43 => if shift { b'|'  } else { b'\\' },
        51 => if shift { b'<'  } else { b','  },
        52 => if shift { b'>'  } else { b'.'  },
        53 => if shift { b'?'  } else { b'/'  },

        _ => return None,
    };
    Some(c)
}
