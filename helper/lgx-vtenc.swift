// lgx-vtenc: ScreenCaptureKit -> VideoToolbox HEVC -> Annex-B access units on stdout.
//
// Wire format per access unit: 4-byte big-endian length + Annex-B bytes.
// Mirrors helper/lgx-vaenc.c on Linux, so the Rust server side stays unchanged.
//
// usage: lgx-vtenc [--width W] [--height H] [--fps N] [--bitrate BPS]
//                  [--display N] [--seconds S] [--out FILE] [--list]
//
// --fps defaults to the display's refresh rate, so a 120 Hz panel is not
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



final class Encoder: NSObject, SCStreamOutput, SCStreamDelegate {
    var session: VTCompressionSession?
    var out: FileHandle
    var frames = 0
    var expected = 0
    var started = Date()
    private let idrLock = NSLock()
    private var idrPending = false

    init(out: FileHandle) { self.out = out }

    func makeSession(_ w: Int, _ h: Int, _ fps: Int) {
        // Optional low-latency rate control (macOS 13+): no frame reordering,
        // no lookahead, temporal layers. It is opt-in because it also changes
        // how the bitrate is applied; the session falls back to plain realtime
        // mode when the encoder refuses the specification.
        let lowLatency = ProcessInfo.processInfo.environment["LGX_VT_LOWLATENCY"] == "1"
        var spec: [String: Any] = [
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder as String: true,
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder as String: false,
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
                outputCallback: { (refcon, _, _, _, sampleBuffer) in
                    guard let refcon, let sampleBuffer else { return }
                    Unmanaged<Encoder>.fromOpaque(refcon).takeUnretainedValue()
                        .emit(sampleBuffer)
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
        if !lowLatency {
            VTSessionSetProperty(session, key: kVTCompressionPropertyKey_MaxFrameDelayCount,
                                 value: 1 as CFNumber)
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

    func emit(_ sb: CMSampleBuffer) {
        guard let bb = CMSampleBufferGetDataBuffer(sb) else { return }
        var total = 0
        var ptr: UnsafeMutablePointer<Int8>?
        guard CMBlockBufferGetDataPointer(bb, atOffset: 0, lengthAtOffsetOut: nil,
                                          totalLengthOut: &total, dataPointerOut: &ptr) == noErr,
              let p = ptr else { return }

        var au = Data()
        au.reserveCapacity(total + 64)
        // Parameter sets first (AVCC keeps them out of band).
        if let fmt = CMSampleBufferGetFormatDescription(sb),
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

        var be = UInt32(au.count).bigEndian
        out.write(Data(bytes: &be, count: 4))
        out.write(au)
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
        guard let pb = CMSampleBufferGetImageBuffer(sb), let session else { return }
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        var props: CFDictionary?
        if takePendingIDR() {
            props = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary
        }
        VTCompressionSessionEncodeFrame(session, imageBuffer: pb, presentationTimeStamp: pts,
                                        duration: .invalid, frameProperties: props,
                                        sourceFrameRefcon: nil, infoFlagsOut: nil)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        die("stream stopped: \(error.localizedDescription)")
    }
}

// Keep the capture and the encoder alive for the whole run.
var gStream: SCStream?
var gEnc: Encoder?

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
        if seconds > 0 { enc.expected = seconds * fps }

        let cfg = SCStreamConfiguration()
        cfg.width = w
        cfg.height = h
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        // 3 frames of capture buffering keeps motion smooth without adding the
        // ~0.2 s that a deeper queue costs at 60 fps.
        cfg.queueDepth = 3
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
