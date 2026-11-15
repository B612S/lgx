//! SDL3 gamepads forwarded as compact state snapshots.
//!
//! Snapshots (not individual events) keep the wire format small and let the
//! guest uinput helper stay stateless: every packet contains the full state.

use sdl3::gamepad::{Axis, Button, Gamepad};
use sdl3::GamepadSubsystem;

use lgx_proto::{msg, Msg};

use crate::net::Conn;

pub const MAX_PADS: usize = 4;

#[derive(Clone, Default)]
pub struct State {
    pub buttons: u32,
    pub axes: [i16; 6],
    pub hat: [i8; 2],
}

pub struct Pad {
    pub id: u32,
    pub slot: u8,
    pub path: Option<String>,
    pub name: String,
    pub _gamepad: Gamepad,
    pub state: State,
}

fn button_bit(b: Button) -> u32 {
    match b {
        Button::South => 0,
        Button::East => 1,
        Button::North => 2,
        Button::West => 3,
        Button::Back => 4,
        Button::Guide => 5,
        Button::Start => 6,
        Button::LeftStick => 7,
        Button::RightStick => 8,
        Button::LeftShoulder => 9,
        Button::RightShoulder => 10,
        Button::DPadUp => 11,
        Button::DPadDown => 12,
        Button::DPadLeft => 13,
        Button::DPadRight => 14,
        Button::Misc1 => 15,
        Button::Misc2 => 16,
        Button::Misc3 => 17,
        Button::Misc4 => 18,
        Button::Misc5 => 19,
        Button::RightPaddle1 => 20,
        Button::LeftPaddle1 => 21,
        Button::RightPaddle2 => 22,
        Button::LeftPaddle2 => 23,
        Button::Touchpad => 24,
    }
}

impl State {
    pub fn set_axis(&mut self, axis: Axis, value: i16) {
        let idx = match axis {
            Axis::LeftX => 0,
            Axis::LeftY => 1,
            Axis::RightX => 2,
            Axis::RightY => 3,
            Axis::TriggerLeft => 4,
            Axis::TriggerRight => 5,
        };
        self.axes[idx] = value;
    }

    pub fn set_button(&mut self, button: Button, down: bool) {
        let bit = button_bit(button);
        if down {
            self.buttons |= 1 << bit;
        } else {
            self.buttons &= !(1 << bit);
        }
        self.hat = [
            if self.buttons & (1 << 11) != 0 {
                -1
            } else if self.buttons & (1 << 12) != 0 {
                1
            } else {
                0
            },
            if self.buttons & (1 << 13) != 0 {
                -1
            } else if self.buttons & (1 << 14) != 0 {
                1
            } else {
                0
            },
        ];
    }
}

fn encode(slot: u8, connected: bool, state: &State, out: &mut [u8; 24]) {
    out.fill(0);
    out[0] = slot;
    out[1] = connected as u8;
    out[4..8].copy_from_slice(&state.buttons.to_le_bytes());
    for (i, a) in state.axes.iter().enumerate() {
        out[8 + i * 2..10 + i * 2].copy_from_slice(&a.to_le_bytes());
    }
    out[20] = state.hat[0] as u8;
    out[21] = state.hat[1] as u8;
}

pub fn send(conn: &Conn, pad: &Pad, connected: bool) {
    let mut p = [0u8; 24];
    encode(pad.slot, connected, &pad.state, &mut p);
    if std::env::var_os("LGX_DEBUG_PAD").is_some() {
        eprintln!(
            "lgx-client: pad {} connected={} buttons={:#x} axes={:?} hat={:?}",
            pad.slot, connected, pad.state.buttons, pad.state.axes, pad.state.hat
        );
    }
    let _ = conn.send(Msg::new(msg::GAMEPAD, 0, 24, 0), &p);
}

pub fn send_disconnected(conn: &Conn, slot: u8) {
    let state = State::default();
    let mut p = [0u8; 24];
    encode(slot, false, &state, &mut p);
    let _ = conn.send(Msg::new(msg::GAMEPAD, 0, 24, 0), &p);
}

fn make(subsystem: &GamepadSubsystem, id: u32, slot: u8) -> Option<Pad> {
    let gamepad = match subsystem.open(id) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("lgx-client: gamepad {id} unavailable: {e}");
            return None;
        }
    };
    let name = gamepad.name().unwrap_or_else(|| "gamepad".into());
    let path = gamepad.path();
    Some(Pad {
        id: gamepad.id().unwrap_or(id),
        slot,
        path,
        name,
        _gamepad: gamepad,
        state: State::default(),
    })
}

pub fn open_all(subsystem: &GamepadSubsystem) -> Vec<Pad> {
    let ids = subsystem.gamepads().unwrap_or_default();
    let mut out = Vec::new();
    for (slot, id) in ids.into_iter().take(MAX_PADS).enumerate() {
        if let Some(p) = make(subsystem, id, slot as u8) {
            eprintln!("lgx-client: gamepad {}: {}", p.slot, p.name);
            out.push(p);
        }
    }
    out
}

pub fn open_one(subsystem: &GamepadSubsystem, id: u32, slot: u8) -> Option<Pad> {
    make(subsystem, id, slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit order is shared with `helper/lgx-uinput.c` (`gamepad_buttons[]`);
    /// reordering either side silently swaps buttons inside the guest.
    #[test]
    fn button_bits_match_the_helper_table() {
        let expected = [
            (Button::South, 0),
            (Button::East, 1),
            (Button::North, 2),
            (Button::West, 3),
            (Button::Back, 4),
            (Button::Guide, 5),
            (Button::Start, 6),
            (Button::LeftStick, 7),
            (Button::RightStick, 8),
            (Button::LeftShoulder, 9),
            (Button::RightShoulder, 10),
            (Button::DPadUp, 11),
            (Button::DPadDown, 12),
            (Button::DPadLeft, 13),
            (Button::DPadRight, 14),
            (Button::Misc1, 15),
            (Button::Misc2, 16),
            (Button::Misc3, 17),
            (Button::Misc4, 18),
            (Button::Misc5, 19),
            (Button::RightPaddle1, 20),
            (Button::LeftPaddle1, 21),
            (Button::RightPaddle2, 22),
            (Button::LeftPaddle2, 23),
            (Button::Touchpad, 24),
        ];
        for (button, bit) in expected {
            assert_eq!(button_bit(button), bit, "{button:?}");
        }
    }

    #[test]
    fn axes_follow_the_wire_order() {
        for (axis, idx) in [
            (Axis::LeftX, 0),
            (Axis::LeftY, 1),
            (Axis::RightX, 2),
            (Axis::RightY, 3),
            (Axis::TriggerLeft, 4),
            (Axis::TriggerRight, 5),
        ] {
            let mut state = State::default();
            state.set_axis(axis, 1234);
            let mut expected = [0i16; 6];
            expected[idx] = 1234;
            assert_eq!(state.axes, expected, "{axis:?}");
        }
    }
}
