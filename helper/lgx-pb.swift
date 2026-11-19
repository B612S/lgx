// lgx-pb — bridge the macOS pasteboard to the lgx clipboard protocol.
//
// The lgx protocol carries clipboard updates as CLIP_SET / CLIP_DATA (plain
// text) and CLIP_SET_HTML / CLIP_DATA_HTML (HTML + plain fallback). This
// helper owns NSPasteboard and speaks a length-framed binary protocol with
// the Rust server on stdin/stdout:
//
//   u32 big-endian payload length, u8 kind (1 = text, 2 = html), payload
//
// The html payload is the protocol's rich payload: u32 html length, html,
// then the plain-text fallback.
//
// Changes made here (from the client) do not bounce back: the pasteboard's
// changeCount is recorded after every write. The initial pasteboard contents
// are not sent either, so the client's clipboard wins on connect.

import AppKit
import Foundation

let kindText: UInt8 = 1
let kindHtml: UInt8 = 2

func log(_ message: String) {
    FileHandle.standardError.write(("lgx-pb: " + message + "\n").data(using: .utf8)!)
}

let pasteboard = NSPasteboard.general
let lock = NSLock()
var lastChange = pasteboard.changeCount

// availableData may return more than one record; keep the remainder.
var stdinBuffer = Data()

func take(_ count: Int) -> Data? {
    while stdinBuffer.count < count {
        let chunk = FileHandle.standardInput.availableData
        if chunk.isEmpty {
            return nil
        }
        stdinBuffer.append(chunk)
    }
    let out = stdinBuffer.prefix(count)
    stdinBuffer.removeFirst(count)
    return Data(out)
}

// The frame (u32 length + kind) is big-endian, matching the Rust server's
// write_clip; the rich payload's inner html length is little-endian, matching
// lgx_proto::encode_rich/decode_rich.
func readBe32(_ data: Data, _ offset: Int) -> UInt32? {
    guard data.count >= offset + 4 else { return nil }
    let b = [UInt8](data[offset..<offset + 4])
    return (UInt32(b[0]) << 24) | (UInt32(b[1]) << 16) | (UInt32(b[2]) << 8) | UInt32(b[3])
}

func readLe32(_ data: Data, _ offset: Int) -> UInt32? {
    guard data.count >= offset + 4 else { return nil }
    let b = [UInt8](data[offset..<offset + 4])
    return UInt32(b[0]) | (UInt32(b[1]) << 8) | (UInt32(b[2]) << 16) | (UInt32(b[3]) << 24)
}

func appendLe32(_ out: inout Data, _ value: UInt32) {
    out.append(UInt8(truncatingIfNeeded: value))
    out.append(UInt8(truncatingIfNeeded: value >> 8))
    out.append(UInt8(truncatingIfNeeded: value >> 16))
    out.append(UInt8(truncatingIfNeeded: value >> 24))
}

func writeRecord(_ kind: UInt8, _ payload: Data) {
    var frame = Data(capacity: 5 + payload.count)
    let length = UInt32(payload.count)
    frame.append(UInt8(truncatingIfNeeded: length >> 24))
    frame.append(UInt8(truncatingIfNeeded: length >> 16))
    frame.append(UInt8(truncatingIfNeeded: length >> 8))
    frame.append(UInt8(truncatingIfNeeded: length))
    frame.append(kind)
    frame.append(payload)
    FileHandle.standardOutput.write(frame)
}

func apply(_ kind: UInt8, _ payload: Data) {
    lock.lock()
    defer { lock.unlock() }
    // Validate before touching the pasteboard: a malformed record used to clear
    // the clipboard and leave it empty, which made every Paste do nothing.
    if kind == kindHtml {
        guard let htmlLength = readLe32(payload, 0), 4 + Int(htmlLength) <= payload.count else {
            log("ignoring a malformed rich clipboard record")
            return
        }
        let html = payload.subdata(in: 4..<(4 + Int(htmlLength)))
        let text = payload.subdata(in: (4 + Int(htmlLength))..<payload.count)
        pasteboard.clearContents()
        pasteboard.setData(html, forType: .html)
        if let string = String(data: text, encoding: .utf8), !string.isEmpty {
            pasteboard.setString(string, forType: .string)
        }
    } else if let string = String(data: payload, encoding: .utf8), !string.isEmpty {
        pasteboard.clearContents()
        pasteboard.setString(string, forType: .string)
    } else {
        return
    }
    lastChange = pasteboard.changeCount
}

func publish() {
    lock.lock()
    defer { lock.unlock() }
    let change = pasteboard.changeCount
    if change == lastChange {
        return
    }
    lastChange = change
    if let html = pasteboard.data(forType: .html), !html.isEmpty {
        let plain = pasteboard.string(forType: .string) ?? ""
        var payload = Data(capacity: 4 + html.count + plain.utf8.count)
        appendLe32(&payload, UInt32(html.count))
        payload.append(html)
        payload.append(plain.data(using: .utf8) ?? Data())
        writeRecord(kindHtml, payload)
    } else if let text = pasteboard.string(forType: .string), !text.isEmpty {
        writeRecord(kindText, Data(text.utf8))
    }
}

// Poll changeCount: NSPasteboard has no change notification.
let pollQueue = DispatchQueue(label: "lgx.pb.poll")
let timer = DispatchSource.makeTimerSource(queue: pollQueue)
timer.schedule(deadline: .now() + 0.2, repeating: 0.2)
timer.setEventHandler { publish() }
timer.resume()

log("watching the pasteboard")

// Commands from the server, one framed record after another.
while true {
    guard let lengthData = take(4), let kindData = take(1) else { break }
    guard let length = readBe32(lengthData, 0) else { break }
    guard let payload = take(Int(length)) else { break }
    guard let kind = kindData.first else { break }
    apply(kind, payload)
}
