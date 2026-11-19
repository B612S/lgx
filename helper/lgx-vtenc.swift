// lgx-vtenc: ScreenCaptureKit -> VideoToolbox HEVC -> Annex-B access units on stdout.
//
// Wire format per access unit: 4-byte big-endian length + Annex-B bytes.
// Mirrors helper/lgx-vaenc.c on Linux, so the Rust server side stays unchanged.
//
// usage: lgx-vtenc [--width W] [--height H] [--fps N] [--bitrate BPS]
//                  [--display N] [--seconds S] [--out FILE] [--list]
//
// Each frame is u32 BE length, u64 BE capture timestamp (microseconds) and
// the Annex-B access unit. --fps defaults to the display's refresh rate, so a 120 Hz panel is not
// silently capped at 60. The Rust server may write `idr` on stdin to force the
// next frame to be an IDR (used after a dropped frame broke the reference
// chain). The pointer is never captured: a composited cursor lags by the whole
// capture/encode pipeline, so the client draws its own.
import Foundation
import ScreenCaptureKit
import VideoToolbox
import CoreMedia
import CoreVideo
import CoreGraphics

func die(_ msg: String) -> Never {
    FileHandle.standardError.write(("lgx-vtenc: " + msg + "\n").data(using: .utf8)!)
    exit(1)
}

/// Requested frame rate; 0 means "follow the display refresh rate".
var fpsArg = 0
var width = 0, height = 0, bitrate = 20_000_000, seconds = 0, displayIndex = 0
var outPath: String? = nil
var listOnly = false
var selfTest = false

let cursorProbePath = ProcessInfo.processInfo.environment["LGX_CURSOR_PROBE"]

var args = Array(CommandLine.arguments.dropFirst())
while let a = args.first {
    args.removeFirst()
    switch a {
    case "--width": width = Int(args.removeFirst()) ?? 0
    case "--height": height = Int(args.removeFirst()) ?? 0
    case "--fps": fpsArg = Int(args.removeFirst()) ?? 0
    case "--bitrate": bitrate = Int(args.removeFirst()) ?? 20_000_000
    case "--seconds": seconds = Int(args.removeFirst()) ?? 0
    case "--display": displayIndex = Int(args.removeFirst()) ?? 0
    case "--out": outPath = args.first
    case "--list": listOnly = true
    case "--selftest": selfTest = true
    default: break
    }
}

func ansi(_ s: String) -> Data { s.data(using: .utf8)! }

func envInt(_ name: String, _ fallback: Int) -> Int {
    guard let value = ProcessInfo.processInfo.environment[name], let n = Int(value) else {
        return fallback
    }
    return n
}


func runSelfTest(_ w: Int, _ h: Int, _ frames: Int, _ fps: Int, _ handle: FileHandle) {
    let enc = Encoder(out: handle)
    gEnc = enc
    enc.makeSession(w, h, fps)
    var pb: CVPixelBuffer?
    let attrs: [String: Any] = [
        kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelBufferWidthKey as String: w,
        kCVPixelBufferHeightKey as String: h,
        kCVPixelBufferIOSurfacePropertiesKey as String: [:],
    ]
    guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
                              attrs as CFDictionary, &pb) == kCVReturnSuccess, let buffer = pb
    else { die("could not create pixel buffer") }

    for i in 0..<frames {
        CVPixelBufferLockBaseAddress(buffer, [])
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        if let yBase = CVPixelBufferGetBaseAddressOfPlane(buffer, 0) {
            let y = yBase.assumingMemoryBound(to: UInt8.self)
            for row in 0..<h {
                for col in 0..<w {
                    y[row * yStride + col] = UInt8((col / 8 + i * 3) % 200 + 20)
                }
            }
        }
        if let cBase = CVPixelBufferGetBaseAddressOfPlane(buffer, 1) {
            let c = cBase.assumingMemoryBound(to: UInt8.self)
            let cStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)
            for row in 0..<(h / 2) { for col in 0..<cStride { c[row * cStride + col] = 128 } }
        }
        CVPixelBufferUnlockBaseAddress(buffer, [])

        let pts = CMTime(value: Int64(i), timescale: CMTimeScale(fps))
        let props: CFDictionary? = i == 0
            ? ([kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary) : nil
        var flags: VTEncodeInfoFlags = []
        VTCompressionSessionEncodeFrame(enc.session!, imageBuffer: buffer,
                                        presentationTimeStamp: pts, duration: .invalid,
                                        frameProperties: props, sourceFrameRefcon: nil,
                                        infoFlagsOut: &flags)
    }
    VTCompressionSessionCompleteFrames(enc.session!, untilPresentationTimeStamp: .invalid)
    FileHandle.standardError.write(ansi("lgx-vtenc: selftest encoded \(enc.frames) frames\n"))
    exit(enc.frames > 0 ? 0 : 3)
}



/// Counts what ScreenCaptureKit delivered versus what VideoToolbox emitted.
/// A low HEVC fps in the server log can mean either the display/content did
/// not change or the encoder could not keep up; this line separates them.
final class EncoderStats {
    private let lock = NSLock()
    private var sck: UInt64 = 0
    private var encoded: UInt64 = 0
    private var calls: UInt64 = 0
    private var errors: UInt64 = 0
    private var inFlight = 0
    private var maxInFlight = 0
    private var submitNs: UInt64 = 0
    private var emitNs: UInt64 = 0
    private var writeNs: UInt64 = 0
    private var lastSck: UInt64 = 0
    private var lastEncoded: UInt64 = 0
    private var lastErrors: UInt64 = 0
    private var lastCalls: UInt64 = 0
    private var lastSubmitNs: UInt64 = 0
    private var lastEmitNs: UInt64 = 0
    private var lastWriteNs: UInt64 = 0
    private var lastNs: UInt64 = DispatchTime.now().uptimeNanoseconds

    func sckFrame() {
        lock.lock(); sck &+= 1; lock.unlock()
    }
    func submitted(_ ns: UInt64) {
        lock.lock()
        calls &+= 1
        inFlight += 1
        if inFlight > maxInFlight { maxInFlight = inFlight }
        submitNs &+= ns
        lock.unlock()
    }
    func outputArrived() {
        lock.lock(); if inFlight > 0 { inFlight -= 1 }; lock.unlock()
    }
    func encodedFrame(_ emit: UInt64, _ write: UInt64) {
        lock.lock()
        encoded &+= 1
        emitNs &+= emit
        writeNs &+= write
        lock.unlock()
    }
    func encodeError() {
        lock.lock(); errors &+= 1; lock.unlock()
    }

    func report() -> String {
        lock.lock()
        let now = DispatchTime.now().uptimeNanoseconds
        let dt = max(Double(now &- lastNs) / 1_000_000_000.0, 0.001)
        let ds = sck &- lastSck
        let de = encoded &- lastEncoded
        let derr = errors &- lastErrors
        let dcalls = calls &- lastCalls
        let dsubmit = submitNs &- lastSubmitNs
        let demit = emitNs &- lastEmitNs
        let dwrite = writeNs &- lastWriteNs
        lastSck = sck; lastEncoded = encoded; lastErrors = errors
        lastCalls = calls; lastSubmitNs = submitNs; lastEmitNs = emitNs; lastWriteNs = writeNs
        lastNs = now
        let flights = inFlight
        let worst = maxInFlight
        // Timings are nanoseconds; the log prints milliseconds.
        let avgSubmit = dcalls > 0 ? Double(dsubmit) / Double(dcalls) / 1_000_000.0 : 0
        let avgEmit = de > 0 ? Double(demit) / Double(de) / 1_000_000.0 : 0
        let avgWrite = de > 0 ? Double(dwrite) / Double(de) / 1_000_000.0 : 0
        lock.unlock()
        return String(
            format: "SCK %.1f fps, encoded %.1f fps, errors %llu, in-flight %d/%d, submit %.2f ms, emit %.2f ms, write %.2f ms",
            Double(ds) / dt, Double(de) / dt, derr, flights, worst,
            avgSubmit, avgEmit, avgWrite)
    }
}

/// Capture timestamp carried through VideoToolbox as the frame refcon. It is
/// mach absolute time, the same clock the audio tap timestamps use, so the
/// client can align the two streams.
final class FrameStamp {
    let ns: UInt64
    init(_ ns: UInt64) { self.ns = ns }
}

/// One retained frame, replaced atomically. The cursor probe reads it without
/// touching the encoder queue.
final class FrameMailbox {
    private let lock = NSLock()
    private var buffer: CVPixelBuffer?

    func store(_ pb: CVPixelBuffer) {
        lock.lock()
        buffer = pb
        lock.unlock()
    }

    func take() -> CVPixelBuffer? {
        lock.lock()
        let current = buffer
        lock.unlock()
        return current
    }
}

/// Exact cursor visibility: a periodic ScreenCaptureKit screenshot includes
/// the pointer while the main stream never does. Comparing a patch around the
/// pointer shows whether macOS is currently drawing it. Screenshots are
/// on-demand, so a static desktop cannot starve the probe the way a
/// change-driven second stream can.
final class CursorProbe {
    private let mailbox: FrameMailbox
    private let displayID: CGDirectDisplayID
    private let path: String?
    private let probeW = 640
    private let probeH: Int
    private let queue = DispatchQueue(label: "lgx.cursorprobe", qos: .utility)
    private var timer: DispatchSourceTimer?
    private var filter: SCContentFilter?
    private var config: SCStreamConfiguration?
    private var busy = false
    private var lastHidden: Bool?
    private var visibleFrames = 0
    private var hiddenFrames = 0
    private var probes = 0

    init(mailbox: FrameMailbox, mainW: Int, mainH: Int, displayID: CGDirectDisplayID, path: String?) {
        self.mailbox = mailbox
        self.displayID = displayID
        self.path = path
        let aspect = mainW > 0 ? Double(mainH) / Double(mainW) : 0.75
        self.probeH = max(1, Int((Double(640) * aspect).rounded()))
    }

    func start(filter: SCContentFilter) {
        self.filter = filter
        let cfg = SCStreamConfiguration()
        cfg.width = probeW
        cfg.height = probeH
        cfg.showsCursor = true
        cfg.scalesToFit = true
        cfg.pixelFormat = kCVPixelFormatType_32BGRA
        cfg.capturesAudio = false
        self.config = cfg
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 0.2, repeating: 0.2)
        timer.setEventHandler { [weak self] in self?.tick() }
        timer.resume()
        self.timer = timer
        FileHandle.standardError.write(
            "lgx-vtenc: cursor probe \(probeW)x\(probeH)@5\n".data(using: .utf8)!)
    }

    private func tick() {
        if busy { return }
        guard let filter, let config else { return }
        busy = true
        SCScreenshotManager.captureImage(contentFilter: filter, configuration: config) {
            [weak self] image, error in
            guard let self else { return }
            self.queue.async {
                self.busy = false
                if let image {
                    self.probes += 1
                    self.evaluate(image)
                } else if let error {
                    FileHandle.standardError.write(
                        "lgx-vtenc: cursor probe capture: \(error.localizedDescription)\n"
                            .data(using: .utf8)!)
                }
            }
        }
    }

    private func setHidden(_ hidden: Bool) {
        if lastHidden == hidden { return }
        lastHidden = hidden
        guard let path else { return }
        let value = Data([hidden ? 0x31 : 0x30])
        try? value.write(to: URL(fileURLWithPath: path), options: .atomic)
        FileHandle.standardError.write(
            "lgx-vtenc: cursor \(hidden ? "hidden" : "visible")\n".data(using: .utf8)!)
    }

    /// Compare a 32x32 patch around the pointer. The main frame has no cursor;
    /// the screenshot does. Two absent probes in a row are required before
    /// hiding, which filters a single unlucky screenshot.
    private func evaluate(_ image: CGImage) {
        guard let main = mailbox.take() else { return }
        let mainW = CVPixelBufferGetWidth(main)
        let mainH = CVPixelBufferGetHeight(main)
        let w = image.width
        let h = image.height
        guard mainW > 8, mainH > 8, w >= 32, h >= 32 else { return }
        let bounds = CGDisplayBounds(displayID)
        guard bounds.width > 0, bounds.height > 0 else { return }
        let loc = CGEvent(source: nil)?.location ?? CGPoint.zero
        let fx = (loc.x - bounds.origin.x) / bounds.width
        let fy = (loc.y - bounds.origin.y) / bounds.height
        guard fx >= 0, fx <= 1, fy >= 0, fy <= 1 else {
            setHidden(false)
            return
        }

        var pixels = [UInt8](repeating: 0, count: w * h * 4)
        pixels.withUnsafeMutableBytes { raw in
            guard let ctx = CGContext(
                data: raw.baseAddress, width: w, height: h, bitsPerComponent: 8,
                bytesPerRow: w * 4, space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
            // Put row 0 at the top, matching the ScreenCaptureKit frame and
            // CGEvent coordinates.
            ctx.translateBy(x: 0, y: CGFloat(h))
            ctx.scaleBy(x: 1, y: -1)
            ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
        }

        let cx = Int(fx * Double(w))
        let cy = Int(fy * Double(h))
        let half = 16
        let patch = half * 2
        let px0 = min(max(0, cx - half), w - patch)
        let py0 = min(max(0, cy - half), h - patch)

        CVPixelBufferLockBaseAddress(main, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(main, .readOnly) }
        guard let mBase = CVPixelBufferGetBaseAddressOfPlane(main, 0) else { return }
        let mStride = CVPixelBufferGetBytesPerRowOfPlane(main, 0)
        let m = mBase.assumingMemoryBound(to: UInt8.self)

        var different = 0
        for y in 0..<patch {
            let my0 = (py0 + y) * mainH / h
            let my1 = max(my0 + 1, (py0 + y + 1) * mainH / h)
            for x in 0..<patch {
                let mx0 = (px0 + x) * mainW / w
                let mx1 = max(mx0 + 1, (px0 + x + 1) * mainW / w)
                var sum = 0
                var count = 0
                for sy in my0..<my1 {
                    let row = sy * mStride
                    for sx in mx0..<mx1 {
                        sum += Int(m[row + sx])
                        count += 1
                    }
                }
                if count == 0 { continue }
                let avg = sum / count
                let p = (py0 + y) * w * 4 + (px0 + x) * 4
                let luma = (Int(pixels[p]) * 299 + Int(pixels[p + 1]) * 587
                            + Int(pixels[p + 2]) * 114) / 1000
                if abs(luma - avg) > 18 { different += 1 }
            }
        }

        if probes <= 10 || probes % 25 == 0 {
            FileHandle.standardError.write(
                "lgx-vtenc: cursor probe frames \(probes) diff \(different) loc \(Int(cx)),\(Int(cy))\n"
                    .data(using: .utf8)!)
        }
        let present = different >= 8
        if present {
            visibleFrames += 1
            hiddenFrames = 0
            setHidden(false)
        } else {
            hiddenFrames += 1
            visibleFrames = 0
            if hiddenFrames >= 2 {
                setHidden(true)
            }
        }
    }
}

final class Encoder: NSObject, SCStreamOutput, SCStreamDelegate {
    var session: VTCompressionSession?
    var out: FileHandle
    var frames = 0
    var expected = 0
    var started = Date()
    let stats = EncoderStats()
    let mailbox = FrameMailbox()
    private let idrLock = NSLock()
    private var idrPending = false

    init(out: FileHandle) { self.out = out }

    func makeSession(_ w: Int, _ h: Int, _ fps: Int) {
        // Optional low-latency rate control (macOS 13+): no frame reordering,
        // no lookahead, temporal layers. It is opt-in because it also changes
        // how the bitrate is applied; the session falls back to plain realtime
        // mode when the encoder refuses the specification.
        let lowLatency = ProcessInfo.processInfo.environment["LGX_VT_LOWLATENCY"] == "1"
        let requireHW = ProcessInfo.processInfo.environment["LGX_VT_REQUIRE_HW"] == "1"
        let maxDelay = envInt("LGX_VT_MAX_DELAY", 1)
        var spec: [String: Any] = [
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder as String: true,
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder as String: requireHW,
        ]
        if lowLatency {
            spec[kVTVideoEncoderSpecification_EnableLowLatencyRateControl as String] = true
        }

        func create() -> OSStatus {
            var out: VTCompressionSession?
            let status = VTCompressionSessionCreate(
                allocator: kCFAllocatorDefault, width: Int32(w), height: Int32(h),
                codecType: kCMVideoCodecType_HEVC,
                encoderSpecification: spec as CFDictionary,
                imageBufferAttributes: nil,
                compressedDataAllocator: nil,
                outputCallback: { (refcon, stampRefcon, _, _, sampleBuffer) in
                    guard let refcon else {
                        if let stampRefcon {
                            Unmanaged<FrameStamp>.fromOpaque(stampRefcon).release()
                        }
                        return
                    }
                    let encoder = Unmanaged<Encoder>.fromOpaque(refcon).takeUnretainedValue()
                    guard let sampleBuffer else {
                        encoder.stats.outputArrived()
                        if let stampRefcon {
                            Unmanaged<FrameStamp>.fromOpaque(stampRefcon).release()
                        }
                        return
                    }
                    var tsNs: UInt64 = 0
                    if let stampRefcon {
                        tsNs = Unmanaged<FrameStamp>.fromOpaque(stampRefcon)
                            .takeRetainedValue().ns
                    }
                    encoder.emit(sampleBuffer, tsNs: tsNs)
                },
                refcon: Unmanaged.passUnretained(self).toOpaque(),
                compressionSessionOut: &out)
            self.session = out
            return status
        }

        var status = create()
        if status != noErr && lowLatency {
            spec.removeValue(forKey: kVTVideoEncoderSpecification_EnableLowLatencyRateControl as String)
            status = create()
        }
        guard status == noErr, let session = self.session else { die("VTCompressionSessionCreate failed: \(status)") }

        var hwValue: CFTypeRef?
        let hwStatus = VTSessionCopyProperty(
            session,
            key: kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder as CFString,
            allocator: kCFAllocatorDefault,
            valueOut: &hwValue)
        var hwText = "unknown"
        if hwStatus == noErr, let hwValue, CFGetTypeID(hwValue) == CFBooleanGetTypeID() {
            hwText = CFBooleanGetValue((hwValue as! CFBoolean)) ? "yes" : "no"
        }
        FileHandle.standardError.write(ansi("lgx-vtenc: hardware encoder \(hwText) (status \(hwStatus)), low-latency \(lowLatency), max delay \(maxDelay), require HW \(requireHW)\n"))

        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ProfileLevel,
                             value: kVTProfileLevel_HEVC_Main_AutoLevel)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AllowFrameReordering,
                             value: kCFBooleanFalse)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate,
                             value: bitrate as CFNumber)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ExpectedFrameRate,
                             value: fps as CFNumber)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxKeyFrameInterval,
                             value: (fps * 2) as CFNumber)
        // Never hold more than one frame inside the encoder and prefer speed
        // over rate-control quality on this interactive stream. Low-latency
        // rate control already forbids lookahead, so the delay cap is only
        // needed in the plain realtime mode.
        if !lowLatency && maxDelay > 0 {
            VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxFrameDelayCount,
                                 value: maxDelay as CFNumber)
        }
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                             value: kCFBooleanTrue)
        VTCompressionSessionPrepareToEncodeFrames(session)
    }

    /// Ask for the next encoded frame to be an IDR. Called from the stdin
    /// thread after the server dropped a frame, so the reference chain heals.
    func requestIDR() {
        idrLock.lock()
        idrPending = true
        idrLock.unlock()
    }

    private func takePendingIDR() -> Bool {
        idrLock.lock()
        defer { idrLock.unlock() }
        let pending = idrPending
        idrPending = false
        return pending
    }

    func emit(_ sb: CMSampleBuffer, tsNs: UInt64) {
        stats.outputArrived()
        let emitStarted = DispatchTime.now().uptimeNanoseconds
        guard let bb = CMSampleBufferGetDataBuffer(sb) else { return }
        var total = 0
        var ptr: UnsafeMutablePointer<Int8>?
        guard CMBlockBufferGetDataPointer(bb, atOffset: 0, lengthAtOffsetOut: nil,
                                          totalLengthOut: &total, dataPointerOut: &ptr) == noErr,
              let p = ptr else { return }

        var au = Data()
        au.reserveCapacity(total + 64)
        // Parameter sets are needed on every IDR, not on every 120 Hz frame.
        // Sending them only with keyframes removes a per-frame copy and the
        // redundant bytes without breaking decoder setup.
        let notSync = CMGetAttachment(
            sb, key: kCMSampleAttachmentKey_NotSync as CFString,
            attachmentModeOut: nil) as? Bool ?? false
        let keyframe = !notSync
        if keyframe, let fmt = CMSampleBufferGetFormatDescription(sb),
           let hevc = fmt as CMFormatDescription? {
            var count = 0
            if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                hevc, parameterSetIndex: 0, parameterSetPointerOut: nil,
                parameterSetSizeOut: nil, parameterSetCountOut: &count,
                nalUnitHeaderLengthOut: nil) == noErr {
                for i in 0..<count {
                    var pset: UnsafePointer<UInt8>?
                    var size = 0
                    if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                        hevc, parameterSetIndex: i, parameterSetPointerOut: &pset,
                        parameterSetSizeOut: &size, parameterSetCountOut: nil,
                        nalUnitHeaderLengthOut: nil) == noErr, let ps = pset {
                        au.append(contentsOf: [0, 0, 0, 1])
                        au.append(ps, count: size)
                    }
                }
            }
        }
        // AVCC -> Annex-B.
        var offset = 0
        let bytes = UnsafeRawPointer(p).assumingMemoryBound(to: UInt8.self)
        while offset + 4 <= total {
            let len = (Int(bytes[offset]) << 24) | (Int(bytes[offset + 1]) << 16)
                    | (Int(bytes[offset + 2]) << 8) | Int(bytes[offset + 3])
            offset += 4
            if len <= 0 || offset + len > total { break }
            au.append(contentsOf: [0, 0, 0, 1])
            au.append(bytes + offset, count: len)
            offset += len
        }
        guard !au.isEmpty else { return }

        // Frame format: u32 BE access-unit length, u64 BE capture timestamp in
        // microseconds (0 when unknown), then the Annex-B access unit.
        let writeStarted = DispatchTime.now().uptimeNanoseconds
        var packet = Data(capacity: 12 + au.count)
        var be = UInt32(au.count).bigEndian
        packet.append(Data(bytes: &be, count: 4))
        var tsUs = (tsNs / 1000).bigEndian
        packet.append(Data(bytes: &tsUs, count: 8))
        packet.append(au)
        out.write(packet)
        let now = DispatchTime.now().uptimeNanoseconds
        stats.encodedFrame(now &- emitStarted, now &- writeStarted)
        frames += 1

        if let _ = CMSampleBufferGetFormatDescription(sb), frames == 1 {
            let attrs = CMGetAttachment(sb, key: kCMSampleAttachmentKey_NotSync as CFString, attachmentModeOut: nil)
            _ = attrs
        }
        if expected > 0 && frames >= expected {
            FileHandle.standardError.write(ansi("lgx-vtenc: \(frames) frames\n"))
            exit(0)
        }
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, CMSampleBufferIsValid(sb) else { return }
        stats.sckFrame()
        guard let pb = CMSampleBufferGetImageBuffer(sb), let session else { return }
        if cursorProbePath != nil {
            mailbox.store(pb)
        }
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        var props: CFDictionary?
        if takePendingIDR() {
            props = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary
        }
        // Timestamp when ScreenCaptureKit hands us the frame, before the
        // encoder: this is the capture time, not the encode completion time.
        let stamp = FrameStamp(DispatchTime.now().uptimeNanoseconds)
        let refcon = Unmanaged.passRetained(stamp).toOpaque()
        let submittedAt = DispatchTime.now().uptimeNanoseconds
        let status = VTCompressionSessionEncodeFrame(
            session, imageBuffer: pb, presentationTimeStamp: pts,
            duration: .invalid, frameProperties: props,
            sourceFrameRefcon: refcon, infoFlagsOut: nil)
        let elapsed = DispatchTime.now().uptimeNanoseconds &- submittedAt
        if status != noErr {
            stats.encodeError()
            Unmanaged<FrameStamp>.fromOpaque(refcon).release()
        } else {
            stats.submitted(elapsed)
        }
    }

    func logStats() {
        let line = "lgx-vtenc: " + stats.report() + "\n"
        FileHandle.standardError.write(line.data(using: .utf8)!)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        die("stream stopped: \(error.localizedDescription)")
    }
}

// Keep the capture and the encoder alive for the whole run.
var gStream: SCStream?
var gProbe: CursorProbe?
var gEnc: Encoder?
var gStatsTimer: DispatchSourceTimer?

if selfTest {
    let w = width > 0 ? width : 1920
    let h = height > 0 ? height : 1200
    let f = fpsArg > 0 ? fpsArg : 60
    let handle: FileHandle
    if let path = outPath {
        FileManager.default.createFile(atPath: path, contents: nil)
        handle = FileHandle(forWritingAtPath: path) ?? FileHandle.standardOutput
    } else { handle = FileHandle.standardOutput }
    runSelfTest(w, h, seconds > 0 ? seconds * f : 60, f, handle)
}

Task {
    do {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        if listOnly {
            for d in content.displays {
                print("display \(d.displayID): \(d.width)x\(d.height)")
            }
            exit(0)
        }
        guard !content.displays.isEmpty else { die("no displays available (lid closed?)") }
        // --display picks a monitor by index into SCShareableContent's list;
        // the first one is the main display.
        let display = content.displays[min(max(displayIndex, 0), content.displays.count - 1)]

        let w = width > 0 ? width : display.width
        let h = height > 0 ? height : display.height

        // Follow the panel unless the caller asked for a fixed rate: a
        // 120 Hz display must not be captured at 60 because of a default.
        var displayHZ = 60.0
        if let mode = CGDisplayCopyDisplayMode(display.displayID), mode.refreshRate > 1 {
            displayHZ = mode.refreshRate
        }
        let fps = fpsArg > 0 ? fpsArg : Int(displayHZ.rounded())
        FileHandle.standardError.write(ansi("lgx-vtenc: capture \(w)x\(h)@\(fps), display \(displayIndex) id=\(display.displayID) \(display.width)x\(display.height) \(displayHZ) Hz\n"))

        let handle: FileHandle
        if let path = outPath {
            FileManager.default.createFile(atPath: path, contents: nil)
            handle = FileHandle(forWritingAtPath: path) ?? FileHandle.standardOutput
        } else {
            handle = FileHandle.standardOutput
        }

        let enc = Encoder(out: handle)
        gEnc = enc
        enc.makeSession(w, h, fps)
        // Every helper process serves a new client session; make the first
        // access unit an IDR so the decoder never starts on a P-frame.
        enc.requestIDR()
        if seconds > 0 { enc.expected = seconds * fps }

        let cfg = SCStreamConfiguration()
        cfg.width = w
        cfg.height = h
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        // 3 frames of capture buffering keeps motion smooth without adding the
        // ~0.2 s that a deeper queue costs at 60 fps.
        let queueDepth = envInt("LGX_VT_QUEUE_DEPTH", 3)
        cfg.queueDepth = queueDepth
        // The pointer is drawn by the client, not captured into the video:
        // ScreenCaptureKit only produces frames when content changes, so a
        // composited cursor moves in video-frame steps while a local cursor
        // follows the host pointer at the display rate.
        cfg.showsCursor = false

        let filter = SCContentFilter(display: display, excludingWindows: [])
        let stream = SCStream(filter: filter, configuration: cfg, delegate: enc)
        gStream = stream
        try stream.addStreamOutput(enc, type: .screen, sampleHandlerQueue: DispatchQueue(label: "lgx.video"))
        try await stream.startCapture()

        // Optional exact cursor-visibility probe. It only runs while a client
        // session owns this helper; 640x400 at 5 fps is a few MB/s and never
        // enters the video encoder or the network stream.
        if let cursorProbePath {
            let probe = CursorProbe(mailbox: enc.mailbox, mainW: w, mainH: h,
                                    displayID: display.displayID, path: cursorProbePath)
            gProbe = probe
            probe.start(filter: filter)
        }

        let statsTimer = DispatchSource.makeTimerSource(queue: DispatchQueue(label: "lgx.vtenc.stats"))
        statsTimer.schedule(deadline: .now() + 5, repeating: 5)
        statsTimer.setEventHandler { gEnc?.logStats() }
        statsTimer.resume()
        gStatsTimer = statsTimer

        // Commands on stdin: `idr` asks for a keyframe after a dropped frame
        // broke the reference chain, `bitrate N` retunes the adaptive rate.
        let commandQueue = DispatchQueue(label: "lgx.commands")
        commandQueue.async {
            while let line = readLine(strippingNewline: true) {
                if line == "idr" {
                    enc.requestIDR()
                } else if line.hasPrefix("bitrate ") {
                    if let value = Int(line.dropFirst("bitrate ".count)), value > 0 {
                        bitrate = value
                        if let session = enc.session {
                            VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate,
                                                 value: value as CFNumber)
                            FileHandle.standardError.write(
                                ansi("lgx-vtenc: bitrate \(value)\n"))
                        }
                    }
                }
            }
        }

        if seconds > 0 {
            DispatchQueue.main.asyncAfter(deadline: .now() + .seconds(seconds + 5)) {
                FileHandle.standardError.write(ansi("lgx-vtenc: timeout, \(enc.frames) frames\n"))
                exit(2)
            }
        }
    } catch {
        die("capture failed: \(error.localizedDescription)")
    }
}

// The main runloop is needed for the asyncAfter timeout and to keep the
// stream and encoder objects alive.
RunLoop.main.run()
