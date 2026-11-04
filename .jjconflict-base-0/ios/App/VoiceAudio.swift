import AVFoundation
import Foundation

/// The microphone and the speaker for one voice conversation.
///
/// One AVAudioEngine does both, so voice processing (echo cancellation) on
/// the input node hears what the player node is saying and keeps Rudder's
/// own voice out of the microphone. That is what lets the user talk over
/// Rudder (barge-in) on a phone speaker or in a car.
///
/// Microphone audio leaves as 24 kHz mono PCM16 (what the Realtime session is
/// configured for) in chunks of about 50 ms. Assistant audio arrives the same
/// way and is played in 40 ms buffers, each one counted when it has actually
/// played, so a barge-in can tell the model how much of its answer was heard.
///
/// All engine state lives on one serial queue shared by every instance, so a
/// conversation that is stopping has deactivated the audio session before the
/// next one activates it. Callbacks fire on that queue (or the tap thread);
/// the owner hops to the main actor itself.
final class VoiceAudio: @unchecked Sendable {
    struct Handlers: Sendable {
        /// About 50 ms of 24 kHz mono PCM16, little endian.
        var microphone: @Sendable (Data) -> Void
        /// 0...1, for the orb. Microphone and playback both report.
        var level: @Sendable (Float) -> Void
        /// True when assistant audio starts playing, false when it drains.
        var playing: @Sendable (Bool) -> Void
        /// The audio stopped on its own: nil for an interruption (a call,
        /// Siri), or a message when it failed.
        var stopped: @Sendable (String?) -> Void
    }

    static let sampleRate: Double = 24_000
    /// 50 ms of PCM16 at 24 kHz.
    private static let microphoneChunkBytes = 2_400
    /// 40 ms per scheduled playback buffer: the granularity of `playedMs`.
    private static let playbackChunkFrames = 960
    /// `.allowBluetoothHFP` by raw value (the old `.allowBluetooth`): the
    /// name changed in the iOS 26 SDK and the value did not, so this compiles
    /// on either SDK without a deprecation.
    private static let bluetoothHFP = AVAudioSession.CategoryOptions(rawValue: 0x4)

    private static let queue = DispatchQueue(label: "dev.viraat.rudder.voice.audio", qos: .userInitiated)
    private var queue: DispatchQueue { Self.queue }

    private let handlers: Handlers
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private var playFormat: AVAudioFormat?
    private var sendFormat: AVAudioFormat?
    private var converter: AVAudioConverter?
    private var observers: [NSObjectProtocol] = []
    private var running = false
    private var tapInstalled = false
    private var playerAttached = false
    private var sessionActive = false
    private var microphonePending = Data()

    // Playback bookkeeping. `generation` changes on every flush, so completion
    // handlers of flushed buffers are ignored when they arrive late.
    private var generation = 0
    private var pending: [(item: String, frames: Int)] = []
    private var played: [String: Int] = [:]
    private var droppedItem: String?
    private var isPlaying = false

    init(handlers: Handlers) {
        self.handlers = handlers
    }

    /// Activates the audio session and starts the engine. Blocks briefly; call
    /// it off the main actor. Throws with a readable message.
    func start() throws {
        try queue.sync { try startLocked() }
    }

    /// Stops the engine and gives the audio back to other apps.
    func stop() {
        queue.async { self.stopLocked() }
    }

    /// Queues one `response.output_audio.delta` for playback.
    func play(base64: String, item: String) {
        queue.async { self.playLocked(base64: base64, item: item) }
    }

    /// Barge-in: stops playback at once and drops whatever was queued. When
    /// assistant audio was playing, returns its item and how many ms of it the
    /// user heard (for `conversation.item.truncate`).
    func interrupt() -> (item: String, playedMs: Int)? {
        queue.sync { interruptLocked() }
    }

    // MARK: - Engine

    private func startLocked() throws {
        guard !running else { return }
        guard let play = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: Self.sampleRate, channels: 1, interleaved: false),
              let send = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: Self.sampleRate, channels: 1, interleaved: true) else {
            throw VoiceError("This iPhone could not set up audio.")
        }
        playFormat = play
        sendFormat = send
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(
                .playAndRecord,
                mode: .voiceChat,
                options: [Self.bluetoothHFP, .allowBluetoothA2DP, .defaultToSpeaker, .duckOthers]
            )
            // Short IO buffers keep barge-in quick; the system may round it.
            try? session.setPreferredIOBufferDuration(0.02)
            try session.setActive(true)
            sessionActive = true

            // Voice processing has to be on before the input format is read:
            // it changes the format the input node delivers.
            try engine.inputNode.setVoiceProcessingEnabled(true)
            engine.attach(player)
            playerAttached = true
            engine.connect(player, to: engine.mainMixerNode, format: play)
            try installTap()
            engine.prepare()
            try engine.start()
            player.play()
            running = true
            observe()
        } catch {
            stopLocked()
            if let error = error as? VoiceError { throw error }
            throw VoiceError("Could not start the microphone: \(error.localizedDescription)")
        }
    }

    private func installTap() throws {
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0, let send = sendFormat,
              let converter = AVAudioConverter(from: format, to: send) else {
            throw VoiceError("No microphone is available.")
        }
        // Voice processing can hand over more than one channel; mix them down.
        converter.downmix = true
        self.converter = converter
        input.installTap(onBus: 0, bufferSize: 2_048, format: format) { [weak self] buffer, _ in
            self?.captured(buffer)
        }
        tapInstalled = true
    }

    private func stopLocked() {
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        observers.removeAll()
        if tapInstalled {
            engine.inputNode.removeTap(onBus: 0)
            tapInstalled = false
        }
        generation += 1
        pending.removeAll()
        played.removeAll()
        microphonePending.removeAll()
        if playerAttached {
            player.stop()
        }
        engine.stop()
        converter = nil
        running = false
        if isPlaying {
            isPlaying = false
            handlers.playing(false)
        }
        if sessionActive {
            sessionActive = false
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
    }

    /// A route change (headphones, the car) or a format change stops the
    /// engine. Reinstall the tap for the new input format and carry on; the
    /// buffers that were queued are gone with it.
    private func restartLocked() {
        guard running else { return }
        if tapInstalled {
            engine.inputNode.removeTap(onBus: 0)
            tapInstalled = false
        }
        generation += 1
        pending.removeAll()
        if isPlaying {
            isPlaying = false
            handlers.playing(false)
        }
        do {
            try installTap()
            engine.prepare()
            try engine.start()
            player.play()
        } catch {
            stopLocked()
            handlers.stopped("The audio route changed and the microphone could not restart.")
        }
    }

    private func observe() {
        let center = NotificationCenter.default
        observers.append(center.addObserver(
            forName: AVAudioSession.interruptionNotification,
            object: AVAudioSession.sharedInstance(),
            queue: nil
        ) { [weak self] note in
            let raw = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? NSNumber)?.uintValue
            guard let raw, AVAudioSession.InterruptionType(rawValue: raw) == .began else { return }
            self?.interrupted()
        })
        observers.append(center.addObserver(
            forName: .AVAudioEngineConfigurationChange,
            object: engine,
            queue: nil
        ) { [weak self] _ in
            self?.configurationChanged()
        })
        observers.append(center.addObserver(
            forName: AVAudioSession.mediaServicesWereResetNotification,
            object: nil,
            queue: nil
        ) { [weak self] _ in
            self?.mediaServicesReset()
        })
    }

    private func interrupted() {
        queue.async {
            guard self.running else { return }
            self.stopLocked()
            self.handlers.stopped(nil)
        }
    }

    private func configurationChanged() {
        queue.async { self.restartLocked() }
    }

    private func mediaServicesReset() {
        queue.async {
            guard self.running else { return }
            self.stopLocked()
            self.handlers.stopped("Audio stopped. Try again.")
        }
    }

    // MARK: - Microphone

    /// On the tap's thread: convert to 24 kHz mono PCM16, then batch on the queue.
    private func captured(_ buffer: AVAudioPCMBuffer) {
        guard buffer.frameLength > 0, let converter, let send = sendFormat, buffer.format.sampleRate > 0 else { return }
        let ratio = send.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up)) + 64
        guard let output = AVAudioPCMBuffer(pcmFormat: send, frameCapacity: capacity) else { return }
        let feed = ConverterFeed(buffer)
        var error: NSError?
        let status = converter.convert(to: output, error: &error) { _, inputStatus in
            if let next = feed.take() {
                inputStatus.pointee = .haveData
                return next
            }
            // Not .endOfStream: the converter keeps its resampler state
            // between taps, so the stream stays continuous.
            inputStatus.pointee = .noDataNow
            return nil
        }
        guard status != .error, output.frameLength > 0, let channel = output.int16ChannelData?[0] else { return }
        let count = Int(output.frameLength)
        let data = Data(bytes: channel, count: count * MemoryLayout<Int16>.size)
        var sum: Float = 0
        for index in 0..<count {
            let sample = Float(channel[index]) / 32_768
            sum += sample * sample
        }
        let level = Self.level(rms: (sum / Float(count)).squareRoot())
        queue.async { self.appendMicrophone(data, level: level) }
    }

    private func appendMicrophone(_ data: Data, level: Float) {
        guard running else { return }
        microphonePending.append(data)
        guard microphonePending.count >= Self.microphoneChunkBytes else { return }
        let chunk = microphonePending
        microphonePending.removeAll(keepingCapacity: true)
        handlers.microphone(chunk)
        if !isPlaying {
            handlers.level(level)
        }
    }

    // MARK: - Playback

    private func playLocked(base64: String, item: String) {
        // A route change stops the engine until restartLocked runs; a
        // player started on a stopped engine raises, so skip until then.
        guard running, engine.isRunning, item != droppedItem, let format = playFormat,
              let data = Data(base64Encoded: base64), data.count >= 2 else { return }
        let total = data.count / 2
        var offset = 0
        var sum: Float = 0
        while offset < total {
            let frames = min(Self.playbackChunkFrames, total - offset)
            guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(frames)),
                  let samples = buffer.floatChannelData?[0] else { return }
            buffer.frameLength = AVAudioFrameCount(frames)
            let start = offset
            data.withUnsafeBytes { raw in
                for index in 0..<frames {
                    let value = Int16(littleEndian: raw.loadUnaligned(fromByteOffset: (start + index) * 2, as: Int16.self))
                    let sample = Float(value) / 32_768
                    samples[index] = sample
                    sum += sample * sample
                }
            }
            let current = generation
            pending.append((item: item, frames: frames))
            player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
                guard let self else { return }
                self.queue.async { self.bufferPlayed(generation: current) }
            }
            offset += frames
        }
        if !player.isPlaying {
            player.play()
        }
        if !isPlaying {
            isPlaying = true
            handlers.playing(true)
        }
        handlers.level(Self.level(rms: (sum / Float(total)).squareRoot()))
    }

    private func bufferPlayed(generation current: Int) {
        guard current == generation, !pending.isEmpty else { return }
        let done = pending.removeFirst()
        played[done.item, default: 0] += done.frames
        if pending.isEmpty, isPlaying {
            isPlaying = false
            handlers.playing(false)
            handlers.level(0)
        }
    }

    private func interruptLocked() -> (item: String, playedMs: Int)? {
        guard let current = pending.first else { return nil }
        generation += 1
        droppedItem = current.item
        pending.removeAll()
        player.stop()
        if running, engine.isRunning {
            player.play()
        }
        if isPlaying {
            isPlaying = false
            handlers.playing(false)
        }
        let frames = played[current.item, default: 0]
        return (item: current.item, playedMs: frames * 1_000 / Int(Self.sampleRate))
    }

    /// RMS to 0...1 on a 50 dB scale, which reads better than linear.
    private static func level(rms: Float) -> Float {
        guard rms.isFinite, rms > 0 else { return 0 }
        let decibels = 20 * log10(rms)
        return min(1, max(0, (decibels + 50) / 50))
    }
}

/// Hands one tap buffer to AVAudioConverter's pull block exactly once. A class
/// so the block captures something Sendable whatever the SDK says the block is.
private final class ConverterFeed: @unchecked Sendable {
    private var buffer: AVAudioPCMBuffer?

    init(_ buffer: AVAudioPCMBuffer) {
        self.buffer = buffer
    }

    func take() -> AVAudioPCMBuffer? {
        defer { buffer = nil }
        return buffer
    }
}

/// An error with a message meant for the user.
struct VoiceError: LocalizedError, Sendable {
    let message: String

    init(_ message: String) {
        self.message = message
    }

    var errorDescription: String? { message }
}
