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
let kindFile: UInt8 = 3
let maxFileBytes = 16 * 1024 * 1024

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

func appendLe64(_ out: inout Data, _ value: UInt64) {
    for shift in stride(from: 0, to: 64, by: 8) {
        out.append(UInt8(truncatingIfNeeded: value >> UInt64(shift)))
    }
}

func readLe64(_ data: Data, _ offset: Int) -> UInt64? {
    guard data.count >= offset + 8 else { return nil }
    var value: UInt64 = 0
    for i in 0..<8 {
        value |= UInt64(data[offset + i]) << UInt64(i * 8)
    }
    return value
}

/// File record payload: u32 LE name length, UTF-8 name, u64 LE size, data.
func decodeFileRecord(_ payload: Data) -> (String, Data)? {
    guard let nameLength = readLe32(payload, 0), nameLength > 0, nameLength <= 255,
          payload.count >= 4 + Int(nameLength) + 8 else { return nil }
    let nameData = payload.subdata(in: 4..<(4 + Int(nameLength)))
    guard let name = String(data: nameData, encoding: .utf8) else { return nil }
    guard let base = name.split(separator: "/").last.map(String.init), !base.isEmpty,
          base != ".", base != "..", !base.contains("\0") else { return nil }
    guard let size = readLe64(payload, 4 + Int(nameLength)) else { return nil }
    let start = 4 + Int(nameLength) + 8
    guard size <= UInt64(maxFileBytes), payload.count - start == Int(size) else { return nil }
    return (base, payload.subdata(in: start..<payload.count))
}

func fileRecord(name: String, data: Data) -> Data {
    let nameData = Data(name.utf8)
    var out = Data(capacity: 4 + nameData.count + 8 + data.count)
    appendLe32(&out, UInt32(nameData.count))
    out.append(nameData)
    appendLe64(&out, UInt64(data.count))
    out.append(data)
    return out
}

func cacheDirectory() -> URL {
    let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        ?? URL(fileURLWithPath: NSTemporaryDirectory())
    return base.appendingPathComponent("lgx/clipboard", isDirectory: true)
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
    if kind == kindFile {
        guard let (name, data) = decodeFileRecord(payload) else {
            log("ignoring a malformed file clipboard record")
            return
        }
        do {
            let dir = cacheDirectory().appendingPathComponent(UUID().uuidString, isDirectory: true)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let url = dir.appendingPathComponent(name)
            try data.write(to: url)
            pasteboard.clearContents()
            if !pasteboard.writeObjects([url as NSURL]) {
                log("could not publish the clipboard file")
                return
            }
            lastChange = pasteboard.changeCount
        } catch {
            log("file clipboard: \(error.localizedDescription)")
        }
        return
    }
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
    // A file offer must win over text; otherwise a copied file is sent as its
    // path and pasted back as a text file.
    if let urls = pasteboard.readObjects(
        forClasses: [NSURL.self],
        options: [.urlReadingFileURLsOnly: true]) as? [URL], !urls.isEmpty
    {
        guard urls.count == 1 else { return }
        let url = urls[0]
        guard url.isFileURL,
              let values = try? url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey]),
              values.isRegularFile == true,
              let size = values.fileSize, size > 0, size <= maxFileBytes,
              let data = try? Data(contentsOf: url)
        else { return }
        writeRecord(kindFile, fileRecord(name: url.lastPathComponent, data: data))
        return
    }
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
