//! USB HID keyboard usages (page 0x07) <-> Linux evdev key codes (an X keycode is the
//! evdev code + 8). The table is the kernel's own (drivers/hid/hid-input.c, hid_keyboard).

const NONE: u8 = 0;

/// evdev codes for HID usages 0x00..=0x94; modifiers 0xE0..=0xE7 below.
const KEYBOARD: [u8; 0x95] = [
    NONE, NONE, NONE, NONE, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, // 0x00
    50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3, // 0x10
    4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, // 0x20
    27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64, // 0x30
    65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106, // 0x40
    105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, // 0x50
    72, 73, 82, 83, 86, 127, 116, 117, 183, 184, 185, 186, 187, 188, 189, 190, // 0x60
    191, 192, 193, 194, 134, 138, 130, 132, 128, 129, 131, 137, 133, 135, 136, 113, // 0x70
    115, 114, NONE, NONE, NONE, 121, NONE, 89, 93, 124, 92, 94, 95, NONE, NONE, NONE, // 0x80
    122, 123, 90, 91, 85, // 0x90
];

const MODIFIERS: [u8; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

pub fn evdev(hid: u16) -> Option<u8> {
    let code = match hid {
        0..=0x94 => KEYBOARD[hid as usize],
        0xE0..=0xE7 => MODIFIERS[(hid - 0xE0) as usize],
        _ => NONE,
    };
    (code != NONE).then_some(code)
}

pub fn hid(evdev: u8) -> Option<u16> {
    if evdev == NONE {
        return None;
    }
    if let Some(i) = MODIFIERS.iter().position(|&c| c == evdev) {
        return Some(0xE0 + i as u16);
    }
    KEYBOARD.iter().position(|&c| c == evdev).map(|i| i as u16) // 0x31 before 0x32 (both 43)
}

/// Media keys: consumer usages <-> evdev codes.
const MEDIA: &[(u16, u8)] = &[
    (0xE9, 115), // volume up
    (0xEA, 114), // volume down
    (0xE2, 113), // mute
    (0xCD, 164), // play/pause
    (0xB5, 163), // next song
    (0xB6, 165), // previous song
    (0xB7, 166), // stop
];

pub fn media_evdev(usage: u16) -> Option<u8> {
    MEDIA.iter().find(|m| m.0 == usage).map(|m| m.1)
}

pub fn media_usage(evdev: u8) -> Option<u16> {
    MEDIA.iter().find(|m| m.1 == evdev).map(|m| m.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for hid_usage in (0..=0x94).chain(0xE0..=0xE7) {
            if hid_usage == 0x32 {
                continue; // non-US # shares its code with backslash
            }
            if let Some(code) = evdev(hid_usage) {
                assert_eq!(hid(code), Some(hid_usage), "{hid_usage:#x}");
            }
        }
        assert_eq!(evdev(0x0E), Some(37)); // K
        assert_eq!(evdev(0xE3), Some(125)); // left Meta
    }
}
