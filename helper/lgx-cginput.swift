//! lgx-cginput: evdev codes from the lgx client -> CGEvent, no kernel driver.
//!
//! Reads one command per line on stdin:
//!   key <evdev> <0|1>        key down/up
//!   btn <evdev> <0|1>        mouse button  (0x110 left, 0x111 right, 0x112 middle)
//!   motion <x> <y>           absolute pointer, both 0..65535
//!   wheel <axis> <steps>     axis 0 = vertical, 1 = horizontal
//!
//! Requires the Accessibility TCC grant for the containing app bundle.
import Foundation
import CoreGraphics
import ApplicationServices

// evdev keycode -> macOS virtual keycode (US layout).
let keymap: [UInt16: CGKeyCode] = [
    // letters
    30: 0x00, 48: 0x0B, 46: 0x08, 32: 0x02, 18: 0x0E, 33: 0x03, 34: 0x05, 35: 0x04,
    23: 0x22, 36: 0x26, 37: 0x28, 38: 0x25, 50: 0x2E, 49: 0x2D, 24: 0x1F, 25: 0x23,
    16: 0x0C, 19: 0x0F, 31: 0x01, 20: 0x11, 22: 0x20, 47: 0x31, 17: 0x0D, 45: 0x2C,
    21: 0x06, 44: 0x07, 2: 0x12, 3: 0x13, 4: 0x14, 5: 0x15, 6: 0x17, 7: 0x16,
    8: 0x1A, 9: 0x1C, 10: 0x19, 11: 0x1D, 12: 0x18, 13: 0x1B,
    // digits row
    41: 0x32, 1: 0x12, 2: 0x13, 3: 0x14, 4: 0x15, 5: 0x17, 6: 0x16, 7: 0x1A,
    8: 0x1C, 9: 0x19, 10: 0x1D,
    // punctuation
    12: 0x1B, 13: 0x18, 26: 0x0A, 27: 0x1B, 39: 0x29, 40: 0x27, 43: 0x2A,
    51: 0x2B, 52: 0x2F, 53: 0x2C, 57: 0x3D, 86: 0x0A, 96: 0x52, 98: 0x5E,
    99: 0x5B, 100: 0x5C, 102: 0x57, 103: 0x58, 104: 0x5C, 105: 0x5D,
    106: 0x5E, 107: 0x5C, 108: 0x5F, 109: 0x60, 110: 0x61, 111: 0x62,
    // whitespace / editing
    14: 0x33, 15: 0x30, 28: 0x24, 58: 0x36, 69: 0x4B, 70: 0x47, 87: 0x4F,
    88: 0x50, 89: 0x51, 90: 0x52, 91: 0x53, 92: 0x56, 93: 0x4E, 94: 0x5B,
    95: 0x55, 97: 0x3F, 54: 0x5F, 55: 0x67, 56: 0x6F, 71: 0x63, 72: 0x6E,
    73: 0x62, 74: 0x66, 75: 0x68, 76: 0x69, 77: 0x65, 78: 0x6B, 79: 0x6A,
    80: 0x71, 81: 0x6D, 82: 0x70, 83: 0x6C,
    // modifiers
    29: 0x3B, 125: 0x2E, 42: 0x38, 54: 0x31, 56: 0x3A, 97: 0x3E, 100: 0x3B,
    126: 0x3D, 127: 0x47,
    // navigation
    102: 0x72, 103: 0x73, 104: 0x74, 105: 0x76, 106: 0x77, 107: 0x78,
    108: 0x7A, 109: 0x7B, 110: 0x7C, 111: 0x7D, 112: 0x7E, 113: 0x7F,
    114: 0x70, 115: 0x71,
    // escape / misc
    1: 0x35, 28: 0x24, 57: 0x31, 59: 0x7A, 60: 0x78, 61: 0x63, 62: 0x76,
    63: 0x60, 64: 0x61, 65: 0x62, 66: 0x64, 67: 0x65, 68: 0x68, 87: 0x4F,
]

func postKey(_ evdev: UInt16, _ down: Bool) {
    guard let code = keymap[evdev] else { return }
    let src = CGEventSource(stateID: .combinedSessionState)
    if let ev = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: down) {
        ev.post(tap: .cghidEventTap)
    }
}

func postButton(_ evdev: UInt16, _ down: Bool) {
    let button: CGMouseButton
    switch evdev {
    case 0x110: button = .left
    case 0x111: button = .right
    case 0x112: button = .center
    default: return
    }
    let loc = CGEvent(source: nil)?.location ?? .zero
    let type: CGEventType = down
        ? (button == .left ? .leftMouseDown : button == .right ? .rightMouseDown : .otherMouseDown)
        : (button == .left ? .leftMouseUp : button == .right ? .rightMouseUp : .otherMouseUp)
    let src = CGEventSource(stateID: .combinedSessionState)
    if let ev = CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: loc, mouseButton: button) {
        ev.post(tap: .cghidEventTap)
    }
}

func postMotion(_ nx: Int, _ ny: Int) {
    guard let screen = CGDisplayBounds(CGMainDisplayID()) as CGRect? else { return }
    let x = screen.origin.x + CGFloat(nx) / 65535.0 * screen.width
    let y = screen.origin.y + CGFloat(ny) / 65535.0 * screen.height
    let src = CGEventSource(stateID: .combinedSessionState)
    if let ev = CGEvent(mouseEventSource: src, mouseType: .mouseMoved,
                        mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: .left) {
        ev.post(tap: .cghidEventTap)
    }
}

func postWheel(_ axis: Int, _ steps: Int) {
    let src = CGEventSource(stateID: .combinedSessionState)
    let dy = axis == 0 ? Int32(-steps) : 0
    let dx = axis == 1 ? Int32(steps) : 0
    if let ev = CGEvent(scrollWheelEvent2Source: src, units: .line, wheelCount: axis == 0 ? 1 : 2,
                        wheel1: dy, wheel2: dx, wheel3: 0) {
        ev.post(tap: .cghidEventTap)
    }
}

let axTrusted = AXIsProcessTrusted()
FileHandle.standardError.write("lgx-cginput: accessibility=\(axTrusted ? "granted" : "MISSING") pid=\(getpid())\n".data(using: .utf8)!)

while let line = readLine(strippingNewline: true) {
    let p = line.split(separator: " ").map(String.init)
    guard p.count >= 2 else { continue }
    switch p[0] {
    case "key", "btn":
        guard p.count >= 3, let code = UInt16(p[1]), let down = Int(p[2]) else { continue }
        if p[0] == "key" { postKey(code, down != 0) } else { postButton(code, down != 0) }
    case "motion":
        guard p.count >= 3, let x = Int(p[1]), let y = Int(p[2]) else { continue }
        postMotion(x, y)
    case "wheel":
        guard p.count >= 3, let axis = Int(p[1]), let steps = Int(p[2]) else { continue }
        postWheel(axis, steps)
    default:
        break
    }
}
