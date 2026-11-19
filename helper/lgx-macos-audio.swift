// lgx-macos-audio — capture the macOS system audio mix for lgx.
//
// A Core Audio process tap (macOS 14.4+) captures every process's output.
// The tap uses CATapMutedWhenTapped, so while this process reads it the HAL
// stops sending that audio to the speakers: the client hears the stream and
// the Mac itself stays quiet. No virtual audio driver is installed and the
// system's default output device is not changed.
//
// stdout carries raw little-endian signed 16-bit stereo frames at 48 kHz,
// which is exactly what the lgx audio port transports. stderr carries status
// for the server log.
//
// The host app must declare NSAudioCaptureUsageDescription; without the
// "System Audio Recording" grant the tap is created successfully but delivers
// pure silence, so a one-time warning is printed if that happens.

import AudioToolbox
import CoreAudio
import Darwin
import Foundation

let sampleRate: Double = 48_000
let channelCount = 2
let silenceThreshold: Float = 0.0005

var tapID = AudioObjectID(kAudioObjectUnknown)
var aggregateID = AudioObjectID(kAudioObjectUnknown)
var procID: AudioDeviceIOProcID?
var stopping = false
var writeFailed = false
var callbacks: UInt64 = 0
var framesSeen: UInt64 = 0
var framesLoud: UInt64 = 0
var warnedSilence = false
var warnedNoCallbacks = false
var rng: UInt64 = 0x9E37_79B9_7F4A_7C15
var signalSources: [DispatchSourceSignal] = []
var ticks = 0
// The tap only calls back while some process is actually producing audio.
// A 10 ms pacer writes silence the rest of the time, so the client receives a
// continuous 48 kHz stream instead of starving and re-prebuffering on every
// sound.
let emitLock = NSLock()
var lastCallbackAt = DispatchTime.now().uptimeNanoseconds
var clockStartNs = DispatchTime.now().uptimeNanoseconds
var emittedFrames: UInt64 = 0
var tapIdle = true

// Far above any tap callback (the default IO buffer is 1024 stereo frames);
// a larger callback would drop the extra frames instead of overflowing.
let capacityFrames = 1 << 14
let outBuffer = UnsafeMutablePointer<Int16>.allocate(capacity: capacityFrames * 2)

func log(_ message: String) {
    FileHandle.standardError.write(("lgx-audio: " + message + "\n").data(using: .utf8)!)
}

func fail(_ message: String) -> Never {
    log(message)
    exit(1)
}

func propertyAddress(
    _ selector: AudioObjectPropertySelector,
    _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
    _ element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: scope, mElement: element)
}

func propertyValue<T>(_ object: AudioObjectID, _ address: AudioObjectPropertyAddress, _ fallback: T) -> T {
    var addr = address
    var value = fallback
    var size = UInt32(MemoryLayout<T>.size)
    let status = withUnsafeMutablePointer(to: &value) {
        AudioObjectGetPropertyData(object, &addr, 0, nil, &size, $0)
    }
    return status == noErr ? value : fallback
}

func streamFormat(_ stream: AudioStreamID) -> AudioStreamBasicDescription {
    propertyValue(stream, propertyAddress(kAudioStreamPropertyVirtualFormat), AudioStreamBasicDescription())
}

/// The tap stream of the private aggregate. The aggregate is built with no
/// output sub-device, so its only input stream is the tap.
func firstInputStream(_ device: AudioObjectID) -> AudioStreamID? {
    var address = propertyAddress(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(device, &address, 0, nil, &size) == noErr,
          size >= UInt32(MemoryLayout<AudioStreamID>.size)
    else {
        return nil
    }
    var streams = [AudioStreamID](repeating: 0, count: Int(size) / MemoryLayout<AudioStreamID>.size)
    var outSize = size
    guard AudioObjectGetPropertyData(device, &address, 0, nil, &outSize, &streams) == noErr else {
        return nil
    }
    return streams.first(where: { $0 != 0 })
}

/// Total-pdf dither, mirroring the Linux server's float32 -> s16le conversion.
func nextUniform() -> Float {
    rng ^= rng >> 12
    rng ^= rng << 25
    rng ^= rng >> 27
    let value = rng &* 0x2545_F491_4F6C_DD1D
    return Float(UInt32(truncatingIfNeeded: value >> 40)) / Float(1 << 24)
}

func quantize(_ sample: Float) -> Int16 {
    let dither = nextUniform() - nextUniform()
    let scaled = (sample * 32768.0 + dither).rounded()
    return Int16(max(-32768.0, min(32767.0, scaled)))
}

func writeAll(_ bytes: Int) {
    var offset = 0
    let base = UnsafeMutableRawPointer(outBuffer)
    while offset < bytes {
        let written = write(STDOUT_FILENO, base + offset, bytes - offset)
        if written > 0 {
            offset += written
        } else if written < 0 && errno == EINTR {
            continue
        } else {
            // The server is gone; the tap must be destroyed so the system
            // gets its audio back. The main queue notices this flag.
            writeFailed = true
            return
        }
    }
}

func handleInput(_ input: UnsafePointer<AudioBufferList>) {
    callbacks += 1
    lastCallbackAt = DispatchTime.now().uptimeNanoseconds
    tapIdle = false
    emitLock.lock()
    defer { emitLock.unlock() }
    let buffers = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: input))
    guard buffers.count > 0 else { return }
    var written = 0
    if buffers.count == 1 {
        // Interleaved: one buffer with every channel of the frame.
        let buffer = buffers[0]
        guard let data = buffer.mData, buffer.mNumberChannels >= UInt32(channelCount) else { return }
        let channels = Int(buffer.mNumberChannels)
        let samples = Int(buffer.mDataByteSize) / MemoryLayout<Float>.size
        let floats = data.bindMemory(to: Float.self, capacity: samples)
        let frames = min(samples / channels, capacityFrames)
        for frame in 0..<frames {
            let left = floats[frame * channels]
            let right = floats[frame * channels + 1]
            outBuffer[frame * 2] = quantize(left)
            outBuffer[frame * 2 + 1] = quantize(right)
            if abs(left) > silenceThreshold || abs(right) > silenceThreshold {
                framesLoud += 1
            }
            framesSeen += 1
        }
        written = frames
    } else {
        // Planar: one buffer per channel.
        guard buffers.count >= channelCount,
              let leftData = buffers[0].mData,
              let rightData = buffers[1].mData
        else { return }
        let leftSamples = Int(buffers[0].mDataByteSize) / MemoryLayout<Float>.size
        let rightSamples = Int(buffers[1].mDataByteSize) / MemoryLayout<Float>.size
        let left = leftData.bindMemory(to: Float.self, capacity: leftSamples)
        let right = rightData.bindMemory(to: Float.self, capacity: rightSamples)
        let frames = min(min(leftSamples, rightSamples), capacityFrames)
        for frame in 0..<frames {
            outBuffer[frame * 2] = quantize(left[frame])
            outBuffer[frame * 2 + 1] = quantize(right[frame])
            if abs(left[frame]) > silenceThreshold || abs(right[frame]) > silenceThreshold {
                framesLoud += 1
            }
            framesSeen += 1
        }
        written = frames
    }
    if written > 0 {
        writeAll(written * 4)
        emittedFrames += UInt64(written)
    }
}

/// Frames that should have been written by now at 48 kHz. The tap only calls
/// back while audio is playing, so this clock is what keeps the stream
/// continuous for the client.
func targetFrames(_ now: UInt64) -> UInt64 {
    UInt64(Double(now &- clockStartNs) * (sampleRate / 1_000_000_000.0))
}

/// Writes `frames` frames of dithered silence. Caller holds `emitLock`.
func writeSilence(_ frames: Int) {
    for index in 0..<(frames * 2) {
        outBuffer[index] = quantize(0)
    }
    writeAll(frames * 4)
}

func shutdown() {
    if stopping { return }
    stopping = true
    if let proc = procID {
        AudioDeviceStop(aggregateID, proc)
        AudioDeviceDestroyIOProcID(aggregateID, proc)
        procID = nil
    }
    if aggregateID != AudioObjectID(kAudioObjectUnknown) {
        AudioHardwareDestroyAggregateDevice(aggregateID)
        aggregateID = AudioObjectID(kAudioObjectUnknown)
    }
    if tapID != AudioObjectID(kAudioObjectUnknown) {
        AudioHardwareDestroyProcessTap(tapID)
        tapID = AudioObjectID(kAudioObjectUnknown)
    }
    exit(0)
}

func start() {
    let tap = CATapDescription(stereoGlobalTapButExcludeProcesses: [])
    tap.name = "lgx audio"
    tap.uuid = UUID()
    tap.muteBehavior = .mutedWhenTapped
    tap.isPrivate = true

    var createdTap = AudioObjectID(kAudioObjectUnknown)
    let tapStatus = AudioHardwareCreateProcessTap(tap, &createdTap)
    if tapStatus != noErr {
        fail("cannot create the process tap: \(tapStatus)")
    }
    tapID = createdTap

    // A private aggregate that contains only the tap. Adding the output device
    // would also pull in its input streams (for example a headset microphone)
    // and duplicate audio into the capture.
    let aggregate: [String: Any] = [
        kAudioAggregateDeviceNameKey as String: "lgx audio tap",
        kAudioAggregateDeviceUIDKey as String: "dev.b612s.lgx.audio." + UUID().uuidString,
        kAudioAggregateDeviceIsPrivateKey as String: true,
        kAudioAggregateDeviceIsStackedKey as String: false,
        kAudioAggregateDeviceTapListKey as String: [[
            kAudioSubTapUIDKey as String: tap.uuid.uuidString,
            kAudioSubTapDriftCompensationKey as String: true,
        ]],
        kAudioAggregateDeviceTapAutoStartKey as String: true,
    ]
    var createdAggregate = AudioObjectID(kAudioObjectUnknown)
    let aggregateStatus = AudioHardwareCreateAggregateDevice(aggregate as CFDictionary, &createdAggregate)
    if aggregateStatus != noErr {
        AudioHardwareDestroyProcessTap(tapID)
        fail("cannot create the aggregate device: \(aggregateStatus)")
    }
    aggregateID = createdAggregate

    // Do not touch kAudioDevicePropertyBufferFrameSize here: on a tap-only
    // aggregate setting it makes AudioDeviceStart() succeed while no IOProc
    // callback is ever delivered. The default (1024 frames) is fine.
    guard let stream = firstInputStream(aggregateID) else {
        shutdown()
        fail("the tap did not expose an input stream")
    }
    let actual = streamFormat(stream)
    let usable = actual.mFormatID == kAudioFormatLinearPCM
        && (actual.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        && actual.mChannelsPerFrame >= UInt32(channelCount)
    if !usable || actual.mSampleRate != sampleRate {
        shutdown()
        fail(
            "tap format is \(actual.mSampleRate) Hz, \(actual.mChannelsPerFrame) ch, "
                + "flags \(actual.mFormatFlags); need 48000 Hz float stereo"
        )
    }

    var createdProc: AudioDeviceIOProcID?
    let procStatus = AudioDeviceCreateIOProcIDWithBlock(&createdProc, aggregateID, nil) { _, input, _, _, _ in
        handleInput(input)
    }
    if procStatus != noErr || createdProc == nil {
        shutdown()
        fail("cannot create the IO proc: \(procStatus)")
    }
    procID = createdProc
    let startStatus = AudioDeviceStart(aggregateID, createdProc)
    if startStatus != noErr {
        shutdown()
        fail("cannot start the tap: \(startStatus)")
    }
    log("capturing the system mix at 48 kHz stereo; local playback is muted while streaming")
}

signal(SIGPIPE, SIG_IGN)
for signalNumber in [SIGTERM, SIGINT] {
    signal(signalNumber, SIG_IGN)
    let source = DispatchSource.makeSignalSource(signal: signalNumber, queue: .main)
    source.setEventHandler { shutdown() }
    source.resume()
    signalSources.append(source)
}

// A failed write means the server vanished; tear the tap down so the system
// is not left muted. The same timer watches for a tap that never delivers,
// which is what a denied System Audio Recording permission looks like.
let watchdog = DispatchSource.makeTimerSource(queue: .main)
watchdog.schedule(deadline: .now() + 1, repeating: 1)
watchdog.setEventHandler {
    ticks += 1
    if writeFailed {
        shutdown()
    }
    if !warnedNoCallbacks, ticks >= 5, callbacks == 0 {
        warnedNoCallbacks = true
        log("the tap has not delivered a single callback; check System Audio Recording permission")
    }
    if !warnedSilence, framesSeen > UInt64(sampleRate * 15), framesLoud == 0 {
        warnedSilence = true
        log("no non-silent audio after 15 s; if the client is silent, allow System Audio Recording for the app")
    }
}
watchdog.resume()

// Paces silence while the tap is idle (see lastCallbackAt above).
let pacer = DispatchSource.makeTimerSource(queue: .main)
pacer.schedule(deadline: .now() + 0.01, repeating: 0.01, leeway: .milliseconds(2))
pacer.setEventHandler {
    let now = DispatchTime.now().uptimeNanoseconds
    // One callback interval of grace; the tap stops calling back as soon as
    // the system goes quiet, and an audible sound must not be interleaved.
    if now &- lastCallbackAt < 30_000_000 {
        return
    }
    emitLock.lock()
    defer { emitLock.unlock() }
    let target = targetFrames(now)
    if !tapIdle {
        // Resume after a sound: do not burst all the skipped silence at once,
        // just realign the clock and continue from here.
        tapIdle = true
        emittedFrames = target
    }
    guard target > emittedFrames else { return }
    let gap = min(target - emittedFrames, 4_800)
    if gap >= 240 {
        writeSilence(Int(gap))
        emittedFrames += gap
    }
}
pacer.resume()

start()
dispatchMain()
