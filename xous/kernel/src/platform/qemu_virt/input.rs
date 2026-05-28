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
/// EV_ABS event type (absolute axis — tablet x/y).
const EV_ABS: u16 = 3;
/// Key-down event value.
const VAL_DOWN: u32 = 1;

/// Left/right shift keycodes (Linux evdev).
const KEY_LEFTSHIFT:  u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTCTRL:   u16 = 29;
const KEY_RIGHTCTRL:  u16 = 97;

/// BTN_LEFT — primary mouse / tablet button (Linux evdev).
const BTN_LEFT: u16 = 0x110;

/// ABS_X / ABS_Y axes for the tablet's absolute coordinates.
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;

/// Number of event slots per eventq.
const QUEUE_SIZE: u16 = 64;

/// Size of one virtio_input_event in bytes (type + code + value).
const EVENT_SIZE: usize = 8;

/// We support up to two virtio-input devices on the same platform: a
/// keyboard for character input and a tablet for absolute pointer
/// motion + clicks. The slots are statically sized so the whole
/// driver remains no_alloc.
const MAX_INPUT_DEVS: usize = 2;

// ─────────────────────────────────────────────────────────────────────────────
// Device state
// ─────────────────────────────────────────────────────────────────────────────

/// What kind of input device occupies a slot. Used to dispatch events
/// to the right consumer (keyboard → keycode_to_ascii; tablet → cursor).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DevKind {
    Keyboard,
    Tablet,
}

struct InputDev {
    base_va: usize,
    irq:     u32,
    eventq:  Virtqueue,
    kind:    DevKind,
    shift:   bool,
    ctrl:    bool,
    /// Tablet only: last absolute X / Y reported by the device. The
    /// virtio-tablet sends EV_ABS events between EV_SYN frames; we
    /// commit motion on each frame end.
    abs_x:   u32,
    abs_y:   u32,
    abs_max: u32,
}

static mut INPUT_DEVS: [Option<InputDev>; MAX_INPUT_DEVS] = [None, None];

// ─────────────────────────────────────────────────────────────────────────────
// Static DMA buffers (kernel BSS — physically contiguous)
// ─────────────────────────────────────────────────────────────────────────────

#[repr(C, align(16384))]
struct VqBuf([u8; Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]);

/// Per-device virtqueue scratch buffer.
static mut EVENTQ_BUFS: [VqBuf; MAX_INPUT_DEVS] = [
    VqBuf([0u8; Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]),
    VqBuf([0u8; Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]),
];

/// Per-device event-buffer pool — one 8-byte event slot per queue
/// descriptor, two devices.
static mut EVENT_BUFS_DEV: [[[u8; EVENT_SIZE]; QUEUE_SIZE as usize]; MAX_INPUT_DEVS] =
    [[[0u8; EVENT_SIZE]; QUEUE_SIZE as usize]; MAX_INPUT_DEVS];

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Probe all virtio MMIO transports and initialize the first keyboard device found.
/// Called during platform init.
pub fn probe_and_init(virtio_base_va: usize) {
    unsafe {
        let mut slot = 0usize;
        for i in 0..NUM_TRANSPORTS {
            if slot >= MAX_INPUT_DEVS { break; }
            let base_va = virtio_base_va + i * TRANSPORT_SIZE;
            if virtio::probe_transport(base_va) == Some(VIRTIO_INPUT_DEVICE_ID) {
                let irq = VIRTIO_IRQ_BASE + i as u32;
                // QEMU lists devices in the order they appear on the
                // command line. We launch virtio-keyboard-device first
                // and virtio-tablet-device second; slot 0 = keyboard,
                // slot 1 = tablet. Tablet's absolute axis maximum is
                // typically 32767 (Q15.0) — QEMU clamps to that.
                let kind = if slot == 0 { DevKind::Keyboard } else { DevKind::Tablet };
                init(slot, base_va, irq, kind);
                slot += 1;
            }
        }
    }
}

/// Return the GIC IRQ number of any registered input device.
///
/// Today's `handle_irq` dispatches by checking `irq_number()` for every
/// match, so we return the FIRST registered IRQ here — the keyboard.
/// `tablet_irq_number()` returns the tablet's.
pub fn irq_number() -> Option<u32> {
    unsafe {
        (*(&raw const INPUT_DEVS)).iter()
            .filter_map(|d| d.as_ref())
            .find(|d| d.kind == DevKind::Keyboard)
            .map(|d| d.irq)
    }
}

/// GIC IRQ for the tablet device, or `None` when no tablet attached.
pub fn tablet_irq_number() -> Option<u32> {
    unsafe {
        (*(&raw const INPUT_DEVS)).iter()
            .filter_map(|d| d.as_ref())
            .find(|d| d.kind == DevKind::Tablet)
            .map(|d| d.irq)
    }
}

fn dev_by_irq(irq: u32) -> Option<&'static mut InputDev> {
    unsafe {
        (*(&raw mut INPUT_DEVS)).iter_mut()
            .filter_map(|d| d.as_mut())
            .find(|d| d.irq == irq)
    }
}

/// Acknowledge the virtio-input interrupt for a specific IRQ.
pub fn ack_irq_for(irq: u32) {
    if let Some(dev) = dev_by_irq(irq) {
        virtio::ack_interrupt(dev.base_va);
    }
}

/// Back-compat shim — keyboard ack used by the existing irq.rs dispatch.
pub fn ack_irq() {
    if let Some(kbd_irq) = irq_number() {
        ack_irq_for(kbd_irq);
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
        // The keyboard always lives in slot 0 (see probe_and_init).
        let slot = 0;
        let dev = (*(&raw mut INPUT_DEVS))[slot].as_mut()?;
        if dev.kind != DevKind::Keyboard { return None; }

        loop {
            let (desc_idx, _len) = dev.eventq.pop_used()?;
            let buf = &EVENT_BUFS_DEV[slot][desc_idx as usize];
            let ev_type  = u16::from_le_bytes([buf[0], buf[1]]);
            let ev_code  = u16::from_le_bytes([buf[2], buf[3]]);
            let ev_value = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

            if ev_code == KEY_LEFTSHIFT || ev_code == KEY_RIGHTSHIFT {
                dev.shift = ev_value == VAL_DOWN;
            }
            if ev_code == KEY_LEFTCTRL || ev_code == KEY_RIGHTCTRL {
                dev.ctrl = ev_value == VAL_DOWN;
            }

            dev.eventq.push_avail(desc_idx);
            virtio::notify(dev.base_va, 0);

            if ev_type == EV_KEY && ev_value == VAL_DOWN {
                if let Some(c) = keycode_to_ascii(ev_code, dev.shift, dev.ctrl) {
                    return Some(c);
                }
            }
        }
    }
}

/// Drain pending tablet events.  Each call processes everything queued
/// up since the last call, applies absolute-axis updates to the cursor
/// via `set_cursor`, and invokes `on_click` for every BTN_LEFT down
/// event seen. Returns `true` if anything changed (motion or click) so
/// the caller can trigger a recompose.
///
/// QEMU's virtio-tablet reports ABS_X/Y in 0..32767 — we scale into the
/// FB's pixel range using the slot's `abs_max`.
pub fn drain_tablet_events(
    set_cursor: impl Fn(i32, i32),
    on_click:   impl Fn(),
) -> bool {
    unsafe {
        let slot = 1; // tablet sits in slot 1 (see probe_and_init).
        let dev = match (*(&raw mut INPUT_DEVS))[slot].as_mut() {
            Some(d) if d.kind == DevKind::Tablet => d,
            _ => return false,
        };
        let mut changed = false;
        let abs_max = dev.abs_max.max(1) as f32;
        while let Some((desc_idx, _len)) = dev.eventq.pop_used() {
            let buf = &EVENT_BUFS_DEV[slot][desc_idx as usize];
            let ev_type  = u16::from_le_bytes([buf[0], buf[1]]);
            let ev_code  = u16::from_le_bytes([buf[2], buf[3]]);
            let ev_value = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

            match (ev_type, ev_code) {
                (EV_ABS, ABS_X) => {
                    dev.abs_x = ev_value;
                    let x = (ev_value as f32 / abs_max
                        * crate::platform::qemu_virt::fb::FB_WIDTH as f32) as i32;
                    let y = (dev.abs_y as f32 / abs_max
                        * crate::platform::qemu_virt::fb::FB_HEIGHT as f32) as i32;
                    set_cursor(x, y);
                    changed = true;
                }
                (EV_ABS, ABS_Y) => {
                    dev.abs_y = ev_value;
                    let x = (dev.abs_x as f32 / abs_max
                        * crate::platform::qemu_virt::fb::FB_WIDTH as f32) as i32;
                    let y = (ev_value as f32 / abs_max
                        * crate::platform::qemu_virt::fb::FB_HEIGHT as f32) as i32;
                    set_cursor(x, y);
                    changed = true;
                }
                (EV_KEY, BTN_LEFT) if ev_value == VAL_DOWN => {
                    on_click();
                    changed = true;
                }
                _ => {}
            }

            dev.eventq.push_avail(desc_idx);
            virtio::notify(dev.base_va, 0);
        }
        changed
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Initialization
// ─────────────────────────────────────────────────────────────────────────────

unsafe fn init(slot: usize, base_va: usize, irq: u32, kind: DevKind) {
    if slot >= MAX_INPUT_DEVS { return; }
    if virtio::init_device(base_va, 0).is_none() {
        return;
    }

    // Set up eventq (queue 0). Use addr_of_mut! to avoid forming an
    // intermediate &mut to the mutable static (lint: static_mut_refs).
    let buf_va = core::ptr::addr_of_mut!(EVENTQ_BUFS[slot].0) as *mut u8 as usize;
    let buf_pa = beetos::virt_to_phys(buf_va);
    let eventq = Virtqueue::init(buf_va, buf_pa, QUEUE_SIZE, beetos::PAGE_SIZE);
    virtio::setup_queue(base_va, 0, &eventq, beetos::PAGE_SIZE);

    let mut dev = InputDev {
        base_va, irq, eventq, kind,
        shift: false, ctrl: false,
        abs_x: 0, abs_y: 0,
        abs_max: 32767, // QEMU tablet default Q15
    };

    // Pre-populate descriptors: each points to its own slot in the
    // per-device event buffer pool.
    for i in 0..(QUEUE_SIZE as usize) {
        let desc_idx = dev.eventq.alloc_desc().expect("eventq descriptor");
        let buf_pa   = beetos::virt_to_phys(EVENT_BUFS_DEV[slot][i].as_ptr() as usize) as u64;

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

    INPUT_DEVS[slot] = Some(dev);

    super::uart::puts("virtio-input: keyboard ready\n");
}

// ─────────────────────────────────────────────────────────────────────────────
// Keycode → ASCII (US QWERTY)
// ─────────────────────────────────────────────────────────────────────────────

fn keycode_to_ascii(code: u16, shift: bool, ctrl: bool) -> Option<u8> {
    let c: u8 = match code {
        1       => 0x1b,  // ESC
        14      => 0x7f,  // Backspace → DEL
        15      => b'\t', // Tab
        28 | 96 => if shift { 0xD5 } else { b'\n' }, // Enter / KP Enter (Shift = click)
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

        // Cursor keys — encoded as four single-byte sentinels that
        // beetos::gui treats as steering events (Snake game today,
        // window navigation tomorrow). Avoids parsing ANSI escape
        // sequences in the input layer.
        //
        // Shift switches the role: plain cursor keys steer the focused
        // interactive window (Snake), shifted cursor keys move the
        // mouse cursor across the desktop (range 0xD1-0xD4); Shift+Enter
        // is a click at the current cursor position (0xD5). Ctrl-arrow
        // (range 0xE1-0xE4) drags the focused window by 16 px so any
        // overlap is correctable from the keyboard alone.
        103 => if ctrl { 0xE1 } else if shift { 0xD1 } else { 0xC1 }, // KEY_UP
        108 => if ctrl { 0xE2 } else if shift { 0xD2 } else { 0xC2 }, // KEY_DOWN
        106 => if ctrl { 0xE3 } else if shift { 0xD3 } else { 0xC3 }, // KEY_RIGHT
        105 => if ctrl { 0xE4 } else if shift { 0xD4 } else { 0xC4 }, // KEY_LEFT

        // Numeric keypad — useful for driving the GUI calculator directly
        // without modifier juggling. Codes per Linux evdev (KEY_KP*).
        71 => b'7',
        72 => b'8',
        73 => b'9',
        75 => b'4',
        76 => b'5',
        77 => b'6',
        79 => b'1',
        80 => b'2',
        81 => b'3',
        82 => b'0',
        83 => b'.',
        55 => b'*',
        74 => b'-',
        78 => b'+',
        98 => b'/',

        _ => return None,
    };
    Some(c)
}
