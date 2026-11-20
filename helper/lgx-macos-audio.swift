// lgx-macos-audio — capture the macOS system audio mix for lgx.
//
// A Core Audio process tap (macOS 14.4+) captures every process's output.
// The tap uses CATapMutedWhenTapped, so while this process reads it the HAL
// stops sending that audio to the speakers: the client hears the stream and
// the Mac itself stays quiet. No virtual audio driver is installed and the
// system's default output device is not changed.
//
// stdout carries framed PCM: u64 LE timestamp, u32 LE byte count, then
// little-endian signed 16-bit stereo frames at 48 kHz. The timestamp is the
// server timeline used for A/V sync. stderr carries status for the server log.
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
/// Linear gain from the default output device's volume. The process tap
/// carries the app's audio *before* the device volume, so without this the
/// client always hears the stream at 100% no matter how the Mac is set.
let gainLock = NSLock()
var outputGain: Float = 1.0
/// Gain used by the previous callback; a volume/mute change ramps over one
/// buffer instead of stepping (which clicks).
var lastGain: Float = 1.0

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

/// The output device's volume scalar, 0...1. The main element works for most
/// devices (including the built-in speakers); fall back to channel 1 for the
/// few that only expose per-channel controls.
func outputVolume(_ device: AudioObjectID) -> Float {
    for element in [kAudioObjectPropertyElementMain, 1] {
        var address = propertyAddress(kAudioDevicePropertyVolumeScalar, kAudioObjectPropertyScopeOutput, element)
        var value: Float32 = 1
        var size = UInt32(MemoryLayout<Float32>.size)
        if AudioObjectGetPropertyData(device, &address, 0, nil, &size, &value) == noErr {
            return max(0, min(1, value))
        }
    }
    return 1
}

func outputMuted(_ device: AudioObjectID) -> Bool {
    for element in [kAudioObjectPropertyElementMain, 1] {
        var address = propertyAddress(kAudioDevicePropertyMute, kAudioObjectPropertyScopeOutput, element)
        var value: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        if AudioObjectGetPropertyData(device, &address, 0, nil, &size, &value) == noErr {
            return value != 0
        }
    }
    return false
}

/// Re-reads the default output device's volume/mute. Called from a timer:
/// Core Audio property reads are not real-time safe.
func refreshOutputGain() {
    let device = propertyValue(
        AudioObjectID(kAudioObjectSystemObject),
        propertyAddress(kAudioHardwarePropertyDefaultOutputDevice),
        AudioObjectID(kAudioObjectUnknown)
    )
    guard device != AudioObjectID(kAudioObjectUnknown) else { return }
    let gain: Float = outputMuted(device) ? 0 : outputVolume(device)
    gainLock.lock()
    outputGain = gain
    gainLock.unlock()
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

func quantize(_ sample: Float, _ gain: Float) -> Int16 {
    let dither = nextUniform() - nextUniform()
    let scaled = (sample * gain * 32768.0 + dither).rounded()
    return Int16(max(-32768.0, min(32767.0, scaled)))
}

func writeRaw(_ bytes: UnsafeRawPointer, _ count: Int) {
    var offset = 0
    while offset < count {
        let written = write(STDOUT_FILENO, bytes + offset, count - offset)
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

/// Writes one PCM chunk with the server-side timeline timestamp of its first
/// sample. Header: u64 LE nanoseconds, u32 LE byte count, then s16le stereo.
/// The timestamp is taken at the write: the client uses it for A/V sync and
/// the tap's fixed buffer latency is compensated on the client.
func writeFrame(_ frames: Int) {
    let bytes = frames * 4
    var header = [UInt8](repeating: 0, count: 12)
    var timestamp = DispatchTime.now().uptimeNanoseconds.littleEndian
    var count = UInt32(bytes).littleEndian
    withUnsafeBytes(of: &timestamp) { raw in
        for i in 0..<8 { header[i] = raw[i] }
    }
    withUnsafeBytes(of: &count) { raw in
        for i in 0..<4 { header[8 + i] = raw[i] }
    }
    header.withUnsafeBytes { writeRaw($0.baseAddress!, 12) }
    if !writeFailed {
        writeRaw(UnsafeRawPointer(outBuffer), bytes)
    }
}

func handleInput(_ input: UnsafePointer<AudioBufferList>) {
    callbacks += 1
    lastCallbackAt = DispatchTime.now().uptimeNanoseconds
    tapIdle = false
    emitLock.lock()
    defer { emitLock.unlock() }
    gainLock.lock()
    let targetGain = outputGain
    gainLock.unlock()
    let startGain = lastGain
    lastGain = targetGain
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
        let step = (targetGain - startGain) / Float(max(frames, 1))
        var gain = startGain
        for frame in 0..<frames {
            let left = floats[frame * channels]
            let right = floats[frame * channels + 1]
            outBuffer[frame * 2] = quantize(left, gain)
            outBuffer[frame * 2 + 1] = quantize(right, gain)
            gain += step
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
        let step = (targetGain - startGain) / Float(max(frames, 1))
        var gain = startGain
        for frame in 0..<frames {
            outBuffer[frame * 2] = quantize(left[frame], gain)
            outBuffer[frame * 2 + 1] = quantize(right[frame], gain)
            gain += step
            if abs(left[frame]) > silenceThreshold || abs(right[frame]) > silenceThreshold {
                framesLoud += 1
            }
            framesSeen += 1
        }
        written = frames
    }
    if written > 0 {
        writeFrame(written)
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
        outBuffer[index] = quantize(0, 1)
    }
    writeFrame(frames)
}

/// Apps that re-render other apps' audio through a Core Audio tap of their
/// own (FineTune). While one of them runs, a plain system tap sees both the
/// raw app stream and the processed copy, which the client hears as doubling.
/// Capturing the mixer's output and muting everything else keeps one copy and
/// keeps per-app volume/EQ working.
let mixerBundleIDs: [String] = {
    let value = ProcessInfo.processInfo.environment["LGX_AUDIO_MIXERS"]
        ?? "com.finetuneapp.FineTune"
    return value.split(separator: ",")
        .map { $0.trimmingCharacters(in: .whitespaces) }
        .filter { !$0.isEmpty }
}()
let tapMode = ProcessInfo.processInfo.environment["LGX_AUDIO_TAP"] ?? "auto"

/// One process tap plus its private tap-only aggregate and running IOProc.
final class TapSource {
    var tapID = AudioObjectID(kAudioObjectUnknown)
    var aggregateID = AudioObjectID(kAudioObjectUnknown)
    var procID: AudioDeviceIOProcID?
    var stream = AudioStreamID(0)
}

var tapSources: [TapSource] = []

func destroyTapSource(_ source: TapSource) {
    if let proc = source.procID, source.aggregateID != AudioObjectID(kAudioObjectUnknown) {
        AudioDeviceStop(source.aggregateID, proc)
        AudioDeviceDestroyIOProcID(source.aggregateID, proc)
    }
    source.procID = nil
    if source.aggregateID != AudioObjectID(kAudioObjectUnknown) {
        AudioHardwareDestroyAggregateDevice(source.aggregateID)
        source.aggregateID = AudioObjectID(kAudioObjectUnknown)
    }
    if source.tapID != AudioObjectID(kAudioObjectUnknown) {
        AudioHardwareDestroyProcessTap(source.tapID)
        source.tapID = AudioObjectID(kAudioObjectUnknown)
    }
}

func shutdown() {
    if stopping { return }
    stopping = true
    for source in tapSources {
        destroyTapSource(source)
    }
    tapSources.removeAll()
    exit(0)
}

/// Creates the tap and a private aggregate that contains only that tap.
/// Adding the output device would also pull in its input streams (for example
/// a headset microphone) and duplicate audio into the capture.
func createTapSource(_ description: CATapDescription, _ name: String) -> TapSource? {
    description.name = name
    description.uuid = UUID()
    description.muteBehavior = .mutedWhenTapped
    description.isPrivate = true
    var tap = AudioObjectID(kAudioObjectUnknown)
    let tapStatus = AudioHardwareCreateProcessTap(description, &tap)
    if tapStatus != noErr {
        log("cannot create the \(name) tap: \(tapStatus)")
        return nil
    }
    let source = TapSource()
    source.tapID = tap
    let aggregate: [String: Any] = [
        kAudioAggregateDeviceNameKey as String: name,
        kAudioAggregateDeviceUIDKey as String: "dev.b612s.lgx.audio." + UUID().uuidString,
        kAudioAggregateDeviceIsPrivateKey as String: true,
        kAudioAggregateDeviceIsStackedKey as String: false,
        kAudioAggregateDeviceTapListKey as String: [[
            kAudioSubTapUIDKey as String: description.uuid.uuidString,
            kAudioSubTapDriftCompensationKey as String: true,
        ]],
        kAudioAggregateDeviceTapAutoStartKey as String: true,
    ]
    var aggregateID = AudioObjectID(kAudioObjectUnknown)
    let aggregateStatus = AudioHardwareCreateAggregateDevice(aggregate as CFDictionary, &aggregateID)
    if aggregateStatus != noErr {
        log("cannot create the \(name) aggregate: \(aggregateStatus)")
        destroyTapSource(source)
        return nil
    }
    source.aggregateID = aggregateID
    // Do not touch kAudioDevicePropertyBufferFrameSize here: on a tap-only
    // aggregate setting it makes AudioDeviceStart() succeed while no IOProc
    // callback is ever delivered. The default (1024 frames) is fine.
    guard let stream = firstInputStream(aggregateID) else {
        log("the \(name) tap did not expose an input stream")
        destroyTapSource(source)
        return nil
    }
    source.stream = stream
    return source
}

func checkTapFormat(_ source: TapSource, _ name: String) -> Bool {
    let actual = streamFormat(source.stream)
    let usable = actual.mFormatID == kAudioFormatLinearPCM
        && (actual.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        && actual.mChannelsPerFrame >= UInt32(channelCount)
    if !usable || actual.mSampleRate != sampleRate {
        log(
            "\(name) tap format is \(actual.mSampleRate) Hz, \(actual.mChannelsPerFrame) ch, "
                + "flags \(actual.mFormatFlags); need 48000 Hz float stereo"
        )
        return false
    }
    return true
}

func startTapSource(
    _ source: TapSource,
    _ name: String,
    _ handler: @escaping (UnsafePointer<AudioBufferList>) -> Void
) -> Bool {
    var proc: AudioDeviceIOProcID?
    let status = AudioDeviceCreateIOProcIDWithBlock(&proc, source.aggregateID, nil) { _, input, _, _, _ in
        handler(input)
    }
    guard status == noErr, let proc else {
        log("cannot create the \(name) IO proc: \(status)")
        return false
    }
    source.procID = proc
    let startStatus = AudioDeviceStart(source.aggregateID, proc)
    if startStatus != noErr {
        log("cannot start the \(name) tap: \(startStatus)")
        return false
    }
    return true
}

func processBundleID(_ process: AudioObjectID) -> String? {
    var address = propertyAddress(kAudioProcessPropertyBundleID)
    var value: Unmanaged<CFString>?
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    let status = withUnsafeMutablePointer(to: &value) {
        AudioObjectGetPropertyData(process, &address, 0, nil, &size, $0)
    }
    guard status == noErr, let value else { return nil }
    return value.takeRetainedValue() as String
}

/// Process objects of the known audio mixers. The object exists as soon as the
/// app is an audio client, even before it renders anything, so the topology can
/// be chosen at startup.
func mixerProcesses() -> [AudioObjectID] {
    var list = [AudioObjectID](repeating: 0, count: 256)
    var size = UInt32(list.count * MemoryLayout<AudioObjectID>.size)
    var address = propertyAddress(kAudioHardwarePropertyProcessObjectList)
    let status = AudioObjectGetPropertyData(
        AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &list
    )
    guard status == noErr else { return [] }
    let count = Int(size) / MemoryLayout<AudioObjectID>.size
    return list.prefix(count).filter { process in
        guard let bundle = processBundleID(process) else { return false }
        return mixerBundleIDs.contains(bundle)
    }
}

/// Process object ids of the mixers the current tap topology was built for.
var activeMixers: [AudioObjectID] = []

func setupTaps() -> Bool {
    let mixers = tapMode == "global" ? [] : mixerProcesses()
    activeMixers = mixers
    if !mixers.isEmpty {
        let muteDescription = CATapDescription(stereoGlobalTapButExcludeProcesses: mixers)
        let captureDescription = CATapDescription(stereoMixdownOfProcesses: mixers)
        let muteSource = createTapSource(muteDescription, "lgx audio mute")
        let captureSource = createTapSource(captureDescription, "lgx audio mixer")
        if let muteSource, let captureSource,
           checkTapFormat(captureSource, "mixer"),
           startTapSource(muteSource, "mute", { _ in }),
           startTapSource(captureSource, "mixer", { handleInput($0) }) {
            tapSources = [muteSource, captureSource]
            log(
                "capturing the mixer output (\(mixerBundleIDs.joined(separator: ","))); "
                    + "other apps are muted locally"
            )
        } else {
            // Do not leave a half-built pair behind; fall back to the whole mix.
            if let muteSource { destroyTapSource(muteSource) }
            if let captureSource { destroyTapSource(captureSource) }
            log("mixer capture unavailable; falling back to the full system mix")
        }
    }
    if tapSources.isEmpty {
        let description = CATapDescription(stereoGlobalTapButExcludeProcesses: [])
        guard let source = createTapSource(description, "lgx audio"),
              checkTapFormat(source, "system"),
              startTapSource(source, "system", { handleInput($0) })
        else {
            shutdown()
            fail("cannot start the system audio tap")
        }
        tapSources = [source]
        log("capturing the system mix at 48 kHz stereo; local playback is muted while streaming")
    }
    return !tapSources.isEmpty
}

/// Restarts the tap topology from scratch and realigns the silence clock:
/// without that, the skipped time is emitted as one startup burst of silence.
func rebuildTaps() {
    for source in tapSources {
        destroyTapSource(source)
    }
    tapSources.removeAll()
    let now = DispatchTime.now().uptimeNanoseconds
    emitLock.lock()
    clockStartNs = now
    emittedFrames = 0
    lastCallbackAt = now
    tapIdle = true
    emitLock.unlock()
    if !setupTaps() {
        log("audio tap rebuild failed; stopping")
        shutdown()
    }
}

func start() {
    guard setupTaps() else {
        shutdown()
        fail("cannot start the audio tap")
    }
    // The pacer's clock starts when the tap actually starts. Otherwise the
    // hundreds of milliseconds spent creating the process tap and aggregate
    // are emitted as one startup burst of silence, which the client prebuffers
    // and plays before the first real sample.
    emitLock.lock()
    let now = DispatchTime.now().uptimeNanoseconds
    lastCallbackAt = now
    clockStartNs = now
    emittedFrames = 0
    tapIdle = true
    emitLock.unlock()
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
    // FineTune may start after the helper or quit mid-session; the topology
    // has to follow, otherwise the client either doubles or goes silent.
    if ticks % 2 == 0, tapMode != "global" {
        let mixers = mixerProcesses()
        if Set(mixers) != Set(activeMixers) {
            rebuildTaps()
        }
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

// Follow the Mac's output volume and mute. The tap signal is upstream of the
// device volume, so this is the only place where a volume-key press can reach
// the client.
refreshOutputGain()
let volumeWatch = DispatchSource.makeTimerSource(queue: .main)
volumeWatch.schedule(deadline: .now() + 0.1, repeating: 0.1, leeway: .milliseconds(20))
volumeWatch.setEventHandler { refreshOutputGain() }
volumeWatch.resume()

start()
dispatchMain()
