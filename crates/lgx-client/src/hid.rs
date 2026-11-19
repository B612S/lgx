//! HID usage -> evdev keycode, taken from the kernel table
//! (drivers/hid/hid-input.c). Index is the SDL scancode (a HID usage).

pub const HID_TO_EVDEV: [u16; 256] = [
    0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
    50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3,
    4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26,
    27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
    65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106,
    105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
    72, 73, 82, 83, 86, 127, 116, 117, 183, 184, 185, 186, 187, 188, 189, 190,
    191, 192, 193, 194, 134, 138, 130, 132, 128, 129, 131, 137, 133, 135, 136, 113,
    115, 114, 240, 240, 240, 121, 240, 89, 93, 124, 92, 94, 95, 240, 240, 240,
    122, 123, 90, 91, 85, 240, 240, 240, 240, 240, 240, 240, 111, 240, 240, 240,
    240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240,
    240, 240, 240, 240, 240, 240, 179, 180, 240, 240, 240, 240, 240, 240, 240, 240,
    240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240, 240,
    240, 240, 240, 240, 240, 240, 240, 240, 111, 240, 240, 240, 240, 240, 240, 240,
    29, 42, 56, 125, 97, 54, 100, 126, 164, 166, 165, 163, 161, 115, 114, 113,
    150, 158, 159, 128, 136, 177, 178, 176, 142, 152, 173, 140, 240, 240, 240, 240,
];

pub fn evdev_from_scancode(sc: i32) -> Option<u16> {
    // SDL scancodes 262..=272 are the consumer/media page, above the 8-bit
    // HID keyboard page covered by HID_TO_EVDEV. They are passed through
    // unchanged by the caller's `& 0x1ff` mask.
    let code = match sc {
        262 => 207, // play
        263 => 164, // pause (play/pause on the wire)
        264 => 167, // record
        265 => 208, // fast forward
        266 => 168, // rewind
        267 => 163, // next track
        268 => 165, // previous track
        269 => 166, // stop
        270 => 161, // eject
        271 => 164, // play/pause
        272 => 226, // media select
        _ => {
            if !(1..256).contains(&sc) {
                return None;
            }
            HID_TO_EVDEV[sc as usize]
        }
    };
    if code == 0 || code == 240 {
        None
    } else {
        Some(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot checks against the kernel HID->evdev table; these are the keys a
    /// remote session uses constantly, so a shifted table is very visible.
    #[test]
    fn common_keys_map_to_the_kernel_table() {
        assert_eq!(evdev_from_scancode(4), Some(30)); // a
        assert_eq!(evdev_from_scancode(29), Some(44)); // z
        assert_eq!(evdev_from_scancode(40), Some(28)); // enter
        assert_eq!(evdev_from_scancode(42), Some(14)); // backspace
        assert_eq!(evdev_from_scancode(44), Some(57)); // space
        assert_eq!(evdev_from_scancode(225), Some(42)); // left shift
        assert_eq!(evdev_from_scancode(224), Some(29)); // left ctrl
        assert_eq!(evdev_from_scancode(226), Some(56)); // left alt
        assert_eq!(evdev_from_scancode(227), Some(125)); // left meta
        assert_eq!(evdev_from_scancode(41), Some(1)); // escape
        assert_eq!(evdev_from_scancode(58), Some(59)); // F1
        assert_eq!(evdev_from_scancode(68), Some(87)); // F11
        assert_eq!(evdev_from_scancode(69), Some(88)); // F12
        assert_eq!(evdev_from_scancode(127), Some(113)); // mute
        assert_eq!(evdev_from_scancode(128), Some(115)); // volume up
        assert_eq!(evdev_from_scancode(129), Some(114)); // volume down
        assert_eq!(evdev_from_scancode(267), Some(163)); // next track
        assert_eq!(evdev_from_scancode(271), Some(164)); // play/pause
    }

    #[test]
    fn reserved_and_out_of_range_scancodes_are_dropped() {
        assert_eq!(evdev_from_scancode(0), None);
        assert_eq!(evdev_from_scancode(256), None);
        assert_eq!(evdev_from_scancode(-1), None);
        assert_eq!(evdev_from_scancode(1), None); // no key
        assert_eq!(evdev_from_scancode(130), None); // 0xF0 placeholder
    }

    #[test]
    fn every_supported_scancode_is_a_known_evdev_code() {
        for sc in 1..256i32 {
            if let Some(code) = evdev_from_scancode(sc) {
                assert!(code < 256, "scancode {sc} maps to {code}");
            }
        }
    }
}
