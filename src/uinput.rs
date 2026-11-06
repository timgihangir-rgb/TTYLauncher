//! Virtual pointer injection through `/dev/uinput`.
//!
//! Terminals report the mouse as *absolute* cell coordinates only — there is
//! no relative-motion protocol — so once the real pointer reaches the edge of
//! the screen no further events arrive and the camera stops turning. The fix is
//! to recenter the pointer: when it gets to the edge of the terminal we inject
//! a relative motion that pushes it back toward the middle, and the user can
//! keep turning.
//!
//! Warping the pointer directly is not possible on Wayland (a regular client
//! may not move it), but a virtual mouse created through uinput is a real input
//! device to the compositor, so the injected motion is applied there too. The
//! device lives only for the session and is destroyed on drop (the kernel also
//! removes it when the fd closes, so even a crash leaves nothing behind).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;

// linux/uinput.h ioctls, computed with the _IOC macros.
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const INPUT_PROP_POINTER: libc::c_int = 0x00;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// `struct input_event` from linux/input.h.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// A compositor-visible relative pointer. Relative motion is injected in device
/// (screen) pixels; `move_rel` is the only operation needed to recenter.
pub struct VirtualPointer {
    dev: File,
}

impl VirtualPointer {
    /// Create the virtual mouse. Returns `None` when uinput is unavailable
    /// (device missing, no permission, sandboxed) — the caller then keeps the
    /// plain absolute mouse behaviour.
    pub fn create() -> Option<Self> {
        let dev = OpenOptions::new().write(true).open("/dev/uinput").ok()?;
        let fd = dev.as_raw_fd();

        // A mouse: relative axes plus a left button, and the POINTER property
        // so compositors treat it as a pointer and not a joystick.
        unsafe {
            if libc::ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_POINTER) < 0 {
                return None;
            }
            if libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int) < 0 {
                return None;
            }
            if libc::ioctl(fd, UI_SET_KEYBIT, BTN_LEFT as libc::c_int) < 0 {
                return None;
            }
            if libc::ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int) < 0 {
                return None;
            }
            if libc::ioctl(fd, UI_SET_RELBIT, REL_X as libc::c_int) < 0 {
                return None;
            }
            if libc::ioctl(fd, UI_SET_RELBIT, REL_Y as libc::c_int) < 0 {
                return None;
            }

            let mut name = [0u8; 80];
            let label = b"ttycraft virtual pointer";
            name[..label.len()].copy_from_slice(label);
            let setup = UinputSetup {
                id: InputId { bustype: 0x03, vendor: 0x7474, product: 0x0001, version: 1 },
                name,
                ff_effects_max: 0,
            };
            if libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) < 0 {
                return None;
            }
            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                return None;
            }
        }

        Some(Self { dev })
    }

    /// Move the pointer by `dx`/`dy` device pixels.
    pub fn move_rel(&self, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        let mut dev = &self.dev;
        let _ = dev.write_all(&raw_event(EV_REL, REL_X, dx));
        let _ = dev.write_all(&raw_event(EV_REL, REL_Y, dy));
        let _ = dev.write_all(&raw_event(EV_SYN, 0, 0));
    }
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.dev.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

fn raw_event(type_: u16, code: u16, value: i32) -> Vec<u8> {
    let ev = InputEvent {
        time: libc::timeval { tv_sec: 0, tv_usec: 0 },
        type_,
        code,
        value,
    };
    unsafe {
        std::slice::from_raw_parts(
            &ev as *const InputEvent as *const u8,
            std::mem::size_of::<InputEvent>(),
        )
    }
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_matches_kernel_layout() {
        // struct timeval (16) + type/code (4) + value (4)
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        // input_id (8) + name (80) + ff_effects_max (4)
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
    }

    #[test]
    fn ioctl_numbers_match_uapi() {
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
    }
}
