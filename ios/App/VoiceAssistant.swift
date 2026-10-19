import AVFoundation
import Foundation
import Observation

/// Where a conversation was started from. The server tailors the session
/// (shorter answers in the car), and the phone screen only stops the
/// conversations it started.
enum VoiceSurface: String, Sendable {
    case phone
    case carplay
}

enum VoiceState: Equatable, Sendable {
    case idle
    case connecting
    case listening
    case thinking
    case speaking
    case error(String)

    /// A conversation is open (or opening).
    var isActive: Bool {
        switch self {
        case .connecting, .listening, .thinking, .speaking: true
        case .idle, .error: false
        }
    }
}

/// "Talk to Rudder": a spoken conversation with the cloud machine.
///
/// The control plane mints a short-lived OpenAI Realtime client secret with
/// the session already configured (instructions, tools, audio formats, turn
/// detection), so this side only moves audio and runs tools. Tool calls go
/// back to the control plane (`/api/app/voice/tool`), which does the work
/// with the user's account, so the model never holds a Rudder token.
///
/// One shared instance: the phone screen and the CarPlay scene drive the same
/// conversation, and CarPlay can start one while the phone UI was never
/// opened, so it signs in from the Keychain on its own.
@MainActor
@Observable
final class VoiceAssistant {
    static let shared = VoiceAssistant()

    /// No user speech and no assistant audio for this long ends the
    /// conversation, so a forgotten session does not run on while driving.
    static let idleTimeout: TimeInterval = 90

    private(set) var state: VoiceState = .idle
    private(set) var surface: VoiceSurface?
    /// The last thing the user said, as transcribed.
    private(set) var userCaption = ""
    /// What Rudder is saying, as it says it.
    private(set) var assistantCaption = ""
    /// A non-fatal error from the voice service, shown small.
    private(set) var notice: String?
    /// 0...1: the microphone while listening, Rudder's voice while speaking.
    private(set) var level: Float = 0

    var isActive: Bool { state.isActive }

    @ObservationIgnored private var sessionID = 0
    @ObservationIgnored private var server: URL
    @ObservationIgnored private var token: String?
    @ObservationIgnored private var socket: RealtimeSocket?
    @ObservationIgnored private var audio: VoiceAudio?
    @ObservationIgnored private var tasks: [Task<Void, Never>] = []
    @ObservationIgnored private var connected = false
    @ObservationIgnored private var playing = false
    @ObservationIgnored private var userSpeaking = false
    /// The response the model is producing now (response.created until its
    /// response.done). Only one can run at a time.
    @ObservationIgnored private var activeResponseID: String?
    @ObservationIgnored private var responseInFlight = false
    /// This client sent response.create and has not seen response.created.
    @ObservationIgnored private var createPending = false
    /// The user stopped talking; turn detection is about to answer.
    @ObservationIgnored private var awaitingResponse = false
    /// A response was asked for while another was running: ask again when
    /// that one is done, unless something else started one.
    @ObservationIgnored private var responseWanted = false
    /// Tool batches, one per response.done, run strictly in order.
    @ObservationIgnored private var toolBatches: [[VoiceToolCall]] = []
    @ObservationIgnored private var toolsRunning = false
    @ObservationIgnored private var endRequested = false
    @ObservationIgnored private var captionIsFresh = true
    @ObservationIgnored private var pendingTools = 0
    @ObservationIgnored private var lastActivity = Date()
    /// The updates cursor, round-tripped to `/api/app/voice/updates` as is.
    @ObservationIgnored private var updatesSeen: [String: Any]?
    /// News for the user ("n2 finished") waiting for a quiet moment.
    @ObservationIgnored private var queuedUpdates: [String] = []
    @ObservationIgnored private var observers: [UUID: @MainActor (VoiceState) -> Void] = [:]

    private init() {
        server = Session.resolvedServer()
    }

    /// Opens a conversation. Does nothing while one is already open (the
    /// phone and the car share it). `focus` names what the user tapped, for
    /// example "rudder: Fix the login bug", and Rudder opens by saying what it
    /// needs.
    func start(surface: VoiceSurface, focus: String? = nil) {
        guard !state.isActive else { return }
        sessionID += 1
        let id = sessionID
        self.surface = surface
        resetConversation()
        transition(.connecting)
        tasks.append(Task { [weak self] in
            await self?.connect(session: id, surface: surface, focus: focus)
        })
    }

    /// Ends the conversation (if any) and releases the microphone.
    func stop() {
        finish(error: nil)
    }

    /// Calls `handler` on every state change until removed. CarPlay uses it to
    /// switch its voice control template.
    @discardableResult
    func addStateObserver(_ handler: @escaping @MainActor (VoiceState) -> Void) -> UUID {
        let id = UUID()
        observers[id] = handler
        return id
    }

    func removeStateObserver(_ id: UUID) {
        observers[id] = nil
    }

    // MARK: - Connecting

    private func connect(session id: Int, surface: VoiceSurface, focus: String?) async {
        guard let token = Self.storedToken() else {
            fail(session: id, "Sign in to Rudder on your iPhone first.")
            return
        }
        guard await Self.microphoneAllowed() else {
            fail(session: id, "Rudder needs the microphone. Allow it in Settings, Rudder, Microphone.")
            return
        }
        guard id == sessionID else { return }
        self.token = token
        server = Session.resolvedServer()

        let grant: VoiceGrant
        do {
            grant = try await mint(token: token, surface: surface, focus: focus)
        } catch {
            fail(session: id, error.localizedDescription)
            return
        }
        guard id == sessionID else { return }
        guard let url = URL(string: grant.url), url.scheme == "wss" || url.scheme == "ws", !grant.clientSecret.isEmpty else {
            fail(session: id, "The voice service sent an unusable address.")
            return
        }

        let socket = RealtimeSocket(url: url, secret: grant.clientSecret)
        let audio = VoiceAudio(handlers: handlers(session: id, socket: socket))
        self.socket = socket
        self.audio = audio
        // Connect while the audio starts: microphone chunks sent before the
        // handshake finishes wait in the socket, so the first words count.
        socket.open()
        receive(from: socket, audio: audio, session: id)
        do {
            try await Task.detached(priority: .userInitiated) { try audio.start() }.value
        } catch {
            fail(session: id, error.localizedDescription)
            return
        }
        guard id == sessionID else {
            audio.stop()
            socket.close()
            return
        }
        lastActivity = Date()
        if let opening = Self.opening(surface: surface, focus: focus, greeting: grant.greeting) {
            // A system message rather than `response.instructions`, which
            // would replace the session's instructions for this turn.
            socket.send([
                "type": "conversation.item.create",
                "item": [
                    "type": "message",
                    "role": "system",
                    "content": [["type": "input_text", "text": opening]],
                ] as [String: Any],
            ])
            requestResponse()
        }
        watchIdle(session: id)
        pollUpdates(session: id)
        refreshState()
    }

    private func mint(token: String, surface: VoiceSurface, focus: String?) async throws -> VoiceGrant {
        var request = URLRequest(url: server.appendingPathComponent("api/app/voice/session"), timeoutInterval: 30)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        var body = ["surface": surface.rawValue]
        if let focus, !focus.isEmpty {
            body["focus"] = focus
        }
        request.httpBody = try? JSONSerialization.data(withJSONObject: body)
        let result: (Data, URLResponse)
        do {
            result = try await URLSession.shared.data(for: request)
        } catch {
            throw VoiceError("Can't reach Rudder: \(error.localizedDescription)")
        }
        let (data, response) = result
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard status == 200 else {
            if status == 401 {
                throw VoiceError("Sign in to Rudder again on your iPhone.")
            }
            let message = ((try? JSONSerialization.jsonObject(with: data)) as? [String: Any])?["error"] as? String
            throw VoiceError(message ?? "Voice is not available right now (HTTP \(status)).")
        }
        guard let grant = try? JSONDecoder().decode(VoiceGrant.self, from: data) else {
            throw VoiceError("The voice service sent a reply this app could not read.")
        }
        return grant
    }

    /// Audio callbacks, hopping to the main actor in the order they fired.
    private func handlers(session id: Int, socket: RealtimeSocket) -> VoiceAudio.Handlers {
        VoiceAudio.Handlers(
            microphone: { data in
                socket.sendAudio(data)
            },
            level: { [weak self] value in
                let assistant = self
                voiceOnMain { assistant?.levelChanged(value, session: id) }
            },
            playing: { [weak self] isPlaying in
                let assistant = self
                voiceOnMain { assistant?.playbackChanged(isPlaying, session: id) }
            },
            stopped: { [weak self] message in
                let assistant = self
                voiceOnMain { assistant?.audioStopped(message, session: id) }
            }
        )
    }

    /// Reads server events off the main actor. Audio and barge-in are handled
    /// right here (they are latency sensitive); everything else goes to
    /// `handle` on the main actor, in order.
    private func receive(from socket: RealtimeSocket, audio: VoiceAudio, session id: Int) {
        tasks.append(Task.detached { [weak self] in
            while !Task.isCancelled {
                let data: Data
                do {
                    data = try await socket.receive()
                } catch {
                    let message = error.localizedDescription
                    await self?.socketClosed(message, session: id)
                    return
                }
                let event = RealtimeEvent.parse(data)
                switch event {
                case .audioDelta(let item, let chunk):
                    audio.play(base64: chunk, item: item)
                    continue
                case .speechStarted:
                    if let cut = audio.interrupt() {
                        socket.send([
                            "type": "conversation.item.truncate",
                            "item_id": cut.item,
                            "content_index": 0,
                            "audio_end_ms": cut.playedMs,
                        ])
                    }
                default:
                    break
                }
                await self?.handle(event, session: id)
            }
        })
    }

    // MARK: - Events

    private func handle(_ event: RealtimeEvent, session id: Int) {
        guard id == sessionID else { return }
        switch event {
        case .sessionCreated:
            connected = true
        case .speechStarted:
            userSpeaking = true
            lastActivity = Date()
        case .speechStopped:
            userSpeaking = false
            awaitingResponse = true
            lastActivity = Date()
        case .userTranscript(let text):
            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                userCaption = trimmed
            }
            lastActivity = Date()
        case .responseCreated(let responseID):
            activeResponseID = responseID
            responseInFlight = true
            createPending = false
            awaitingResponse = false
            captionIsFresh = true
        case .assistantTranscript(let delta):
            if captionIsFresh {
                assistantCaption = ""
                captionIsFresh = false
            }
            assistantCaption += delta
            lastActivity = Date()
        case .responseDone(let outcome):
            responseDone(outcome, session: id)
        case .error(let message):
            notice = message
            // A refused response.create (one already running, say) sends
            // neither response.created nor response.done; without this the
            // assistant would read as thinking forever and updates would stop.
            createPending = false
            awaitingResponse = false
        case .audioDelta, .other:
            break
        }
        refreshState()
    }

    private func levelChanged(_ value: Float, session id: Int) {
        guard id == sessionID else { return }
        level = value
    }

    private func playbackChanged(_ isPlaying: Bool, session id: Int) {
        guard id == sessionID else { return }
        playing = isPlaying
        lastActivity = Date()
        if !isPlaying {
            level = 0
            if endRequested {
                finish(error: nil)
                return
            }
        }
        refreshState()
    }

    private func audioStopped(_ message: String?, session id: Int) {
        guard id == sessionID else { return }
        finish(error: message)
    }

    private func socketClosed(_ message: String, session id: Int) {
        guard id == sessionID else { return }
        if endRequested {
            finish(error: nil)
        } else {
            finish(error: connected ? "The conversation was cut off. Try again." : "Could not reach the voice service: \(message)")
        }
    }

    // MARK: - Tools

    private func responseDone(_ outcome: ResponseOutcome, session id: Int) {
        if activeResponseID == nil || outcome.id == nil || outcome.id == activeResponseID {
            activeResponseID = nil
            responseInFlight = false
        }
        if outcome.status == "failed" {
            notice = outcome.failure ?? "Rudder could not answer that."
        }
        if !outcome.calls.isEmpty {
            // The batch asks for its own follow-up when it is answered.
            responseWanted = false
            answer(outcome.calls, session: id)
        } else if responseWanted, !responseInFlight, !createPending, !userSpeaking, !awaitingResponse {
            responseWanted = false
            requestResponse()
        }
    }

    /// The one place that sends response.create. With a response already
    /// running (turn detection can start one while a tool call is out), the
    /// create would be refused, so it waits for that response to finish.
    private func requestResponse() {
        guard let socket else { return }
        // The user is talking: turn detection answers when they stop, and
        // that response sees everything already in the conversation.
        if userSpeaking || awaitingResponse {
            return
        }
        if responseInFlight || createPending {
            responseWanted = true
            return
        }
        socket.send(["type": "response.create"])
        createPending = true
        lastActivity = Date()
        refreshState()
    }

    /// Queues a response's tool calls. Batches run one at a time, in order,
    /// so two responses' calls never interleave.
    private func answer(_ calls: [VoiceToolCall], session id: Int) {
        pendingTools += calls.count
        toolBatches.append(calls)
        guard !toolsRunning else { return }
        toolsRunning = true
        tasks.append(Task { [weak self] in
            await self?.drainToolBatches(session: id)
        })
    }

    /// Runs each call, hands its output back, then asks for the next response
    /// once per batch. `end` (end_conversation) lets the goodbye finish
    /// playing instead.
    private func drainToolBatches(session id: Int) async {
        defer {
            if sessionID == id {
                toolsRunning = false
            }
        }
        while sessionID == id, !toolBatches.isEmpty {
            let calls = toolBatches.removeFirst()
            var end = false
            for call in calls {
                let reply = await runTool(call)
                guard sessionID == id else { return }
                pendingTools = max(0, pendingTools - 1)
                lastActivity = Date()
                socket?.send([
                    "type": "conversation.item.create",
                    "item": [
                        "type": "function_call_output",
                        "call_id": call.callID,
                        "output": reply.output,
                    ],
                ])
                if reply.end {
                    end = true
                }
            }
            if end {
                endAfterPlayback(session: id)
                return
            }
            requestResponse()
        }
    }

    private func runTool(_ call: VoiceToolCall) async -> ToolReply {
        // Handled here, not on the server, so "stop listening" works even
        // when the control plane cannot be reached.
        if call.name == "end_conversation" {
            return ToolReply(output: "Ending.", end: true)
        }
        guard let token else {
            return ToolReply(output: "The user is signed out of Rudder on this phone.", end: false)
        }
        // A tool can wake the cloud machine, which takes up to ~90 seconds.
        var request = URLRequest(url: server.appendingPathComponent("api/app/voice/tool"), timeoutInterval: 120)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.httpBody = try? JSONSerialization.data(withJSONObject: [
            "name": call.name,
            "arguments": call.arguments,
            "call_id": call.callID,
        ])
        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            if status == 200, let output = body["output"] as? String {
                return ToolReply(output: output, end: body["end"] as? Bool ?? false)
            }
            let message = body["error"] as? String ?? "HTTP \(status)"
            return ToolReply(output: "The \(call.name) tool failed: \(message)", end: false)
        } catch {
            return ToolReply(output: "The \(call.name) tool could not reach Rudder: \(error.localizedDescription)", end: false)
        }
    }

    /// Ends once Rudder's last words have played, or after 4 seconds at most.
    private func endAfterPlayback(session id: Int) {
        endRequested = true
        guard playing else {
            finish(error: nil)
            return
        }
        tasks.append(Task { [weak self] in
            try? await Task.sleep(for: .seconds(4))
            guard let self, self.sessionID == id else { return }
            self.finish(error: nil)
        })
    }

    // MARK: - Live updates

    /// Asks the control plane every 5 seconds what changed (an agent finished,
    /// failed or needs the user), so Rudder can say so without being asked.
    /// The first call sends no cursor; each reply carries the next one.
    private func pollUpdates(session id: Int) {
        tasks.append(Task { [weak self] in
            var first = true
            while !Task.isCancelled {
                if !first {
                    try? await Task.sleep(for: .seconds(5))
                }
                first = false
                guard let self, self.sessionID == id else { return }
                await self.fetchUpdates(session: id)
            }
        })
    }

    private func fetchUpdates(session id: Int) async {
        guard let token else { return }
        var request = URLRequest(url: server.appendingPathComponent("api/app/voice/updates"), timeoutInterval: 20)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        let seen: Any = updatesSeen.map { $0 as Any } ?? NSNull()
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["seen": seen])
        // Errors are silent: the next tick tries again with the same cursor.
        guard let result = try? await URLSession.shared.data(for: request) else { return }
        let (data, response) = result
        guard (response as? HTTPURLResponse)?.statusCode == 200,
              let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              id == sessionID else { return }
        if let next = body["seen"] as? [String: Any] {
            updatesSeen = next
        }
        if let message = body["message"] as? String, !message.isEmpty {
            queuedUpdates.append(message)
            deliverUpdates()
        }
    }

    /// Speaks queued news once nobody is talking and nothing is in flight:
    /// never over Rudder, never over the user, never mid tool call.
    private func deliverUpdates() {
        guard !queuedUpdates.isEmpty, connected, let socket, !endRequested, !playing, !userSpeaking,
              !responseInFlight, !createPending, !awaitingResponse, pendingTools == 0 else { return }
        let text = queuedUpdates.joined(separator: "\n")
        queuedUpdates.removeAll()
        socket.send([
            "type": "conversation.item.create",
            "item": [
                "type": "message",
                "role": "system",
                "content": [["type": "input_text", "text": text]],
            ] as [String: Any],
        ])
        requestResponse()
    }

    // MARK: - State

    private func watchIdle(session id: Int) {
        tasks.append(Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(5))
                guard let self, self.sessionID == id else { return }
                // A turn that drew no response (VAD decided it was noise, or
                // a response.create was refused) must not hold "thinking"
                // forever: that would also block queued updates.
                if self.awaitingResponse || self.createPending, !self.responseInFlight,
                   Date().timeIntervalSince(self.lastActivity) > 10 {
                    self.awaitingResponse = false
                    self.createPending = false
                    self.refreshState()
                }
                let quiet = !self.playing && !self.userSpeaking && self.pendingTools == 0
                if quiet && Date().timeIntervalSince(self.lastActivity) > Self.idleTimeout {
                    self.finish(error: nil)
                    return
                }
            }
        })
    }

    private func refreshState() {
        guard state.isActive, connected else { return }
        let next: VoiceState
        if playing {
            next = .speaking
        } else if userSpeaking {
            next = .listening
        } else if pendingTools > 0 || responseInFlight || createPending || awaitingResponse {
            next = .thinking
        } else {
            next = .listening
        }
        transition(next)
        if next == .listening {
            deliverUpdates()
        }
    }

    private func fail(session id: Int, _ message: String) {
        guard id == sessionID else { return }
        finish(error: message)
    }

    private func finish(error: String?) {
        sessionID += 1
        for task in tasks {
            task.cancel()
        }
        tasks.removeAll()
        socket?.close()
        socket = nil
        audio?.stop()
        audio = nil
        token = nil
        surface = nil
        connected = false
        playing = false
        userSpeaking = false
        activeResponseID = nil
        responseInFlight = false
        createPending = false
        awaitingResponse = false
        responseWanted = false
        toolBatches.removeAll()
        toolsRunning = false
        endRequested = false
        pendingTools = 0
        updatesSeen = nil
        queuedUpdates.removeAll()
        level = 0
        transition(error.map { VoiceState.error($0) } ?? .idle)
    }

    private func resetConversation() {
        userCaption = ""
        assistantCaption = ""
        notice = nil
        level = 0
        captionIsFresh = true
        lastActivity = Date()
    }

    private func transition(_ next: VoiceState) {
        guard state != next else { return }
        state = next
        for observer in Array(observers.values) {
            observer(next)
        }
    }

    // MARK: - Helpers

    static func storedToken() -> String? {
        if let token = Keychain.read(), !token.isEmpty {
            return token
        }
        #if DEBUG
        if let probe = UserDefaults.standard.string(forKey: "probeToken"), !probe.isEmpty {
            return probe
        }
        #endif
        return nil
    }

    private static func microphoneAllowed() async -> Bool {
        switch AVAudioApplication.shared.recordPermission {
        case .granted:
            return true
        case .denied:
            return false
        default:
            return await AVAudioApplication.requestRecordPermission()
        }
    }

    /// What Rudder says first, if anything.
    private static func opening(surface: VoiceSurface, focus: String?, greeting: String?) -> String? {
        if let focus, !focus.isEmpty {
            let place = surface == .carplay ? "CarPlay" : "the Rudder app"
            return "The user tapped \(focus) in \(place). Tell them in one sentence what it needs and ask what to do."
        }
        if let greeting, !greeting.isEmpty {
            return "Greet the user briefly: \(greeting)"
        }
        return nil
    }
}

/// Runs `work` on the main actor in the order it was submitted. Audio
/// callbacks use this instead of a Task each, which could run out of order
/// (a "stopped playing" overtaking the "started playing" before it).
func voiceOnMain(_ work: @escaping @MainActor @Sendable () -> Void) {
    DispatchQueue.main.async {
        MainActor.assumeIsolated { work() }
    }
}

/// `POST /api/app/voice/session`.
private struct VoiceGrant: Decodable, Sendable {
    let url: String
    let clientSecret: String
    let model: String?
    let voice: String?
    let expiresAt: Double?
    let greeting: String?
}

private struct ToolReply: Sendable {
    let output: String
    let end: Bool
}

/// A `response.done`: how it ended and the tool calls it asks for.
struct ResponseOutcome: Sendable {
    let id: String?
    /// completed, cancelled, failed or incomplete.
    let status: String
    let failure: String?
    let calls: [VoiceToolCall]
}

struct VoiceToolCall: Sendable {
    let name: String
    let arguments: String
    let callID: String
}

/// The Realtime server events this client acts on.
enum RealtimeEvent: Sendable {
    case sessionCreated
    case audioDelta(item: String, audio: String)
    case assistantTranscript(String)
    case userTranscript(String)
    case speechStarted
    case speechStopped
    case responseCreated(id: String?)
    case responseDone(ResponseOutcome)
    case error(String)
    case other

    static func parse(_ data: Data) -> RealtimeEvent {
        guard let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let type = object["type"] as? String else { return .other }
        switch type {
        case "session.created":
            return .sessionCreated
        // The beta names too, in case the server pins an older model.
        case "response.output_audio.delta", "response.audio.delta":
            guard let delta = object["delta"] as? String else { return .other }
            return .audioDelta(item: object["item_id"] as? String ?? "", audio: delta)
        case "response.output_audio_transcript.delta", "response.audio_transcript.delta":
            guard let delta = object["delta"] as? String else { return .other }
            return .assistantTranscript(delta)
        case "conversation.item.input_audio_transcription.completed":
            return .userTranscript(object["transcript"] as? String ?? "")
        case "input_audio_buffer.speech_started":
            return .speechStarted
        case "input_audio_buffer.speech_stopped":
            return .speechStopped
        case "response.created":
            let response = object["response"] as? [String: Any]
            return .responseCreated(id: response?["id"] as? String)
        case "response.done":
            let response = object["response"] as? [String: Any]
            let status = response?["status"] as? String ?? ""
            let details = response?["status_details"] as? [String: Any]
            let failure = (details?["error"] as? [String: Any])?["message"] as? String
            // Only finished calls of a finished response run: a cancelled or
            // cut-off call can carry half its arguments.
            let output = status == "completed" ? response?["output"] as? [[String: Any]] ?? [] : []
            let calls = output.compactMap { item -> VoiceToolCall? in
                guard item["type"] as? String == "function_call",
                      item["status"] as? String == "completed",
                      let name = item["name"] as? String,
                      let callID = item["call_id"] as? String else { return nil }
                return VoiceToolCall(name: name, arguments: item["arguments"] as? String ?? "{}", callID: callID)
            }
            return .responseDone(ResponseOutcome(
                id: response?["id"] as? String,
                status: status,
                failure: failure,
                calls: calls
            ))
        case "error":
            let error = object["error"] as? [String: Any]
            return .error(error?["message"] as? String ?? "The voice service reported an error.")
        default:
            return .other
        }
    }
}

/// The Realtime WebSocket. Sends are safe from any thread (URLSession
/// serializes them); `receive` is called from one loop only.
final class RealtimeSocket: @unchecked Sendable {
    private let session: URLSession
    private let task: URLSessionWebSocketTask

    init(url: URL, secret: String) {
        var request = URLRequest(url: url)
        // The header only: OpenAI refuses a socket that also sends the
        // secret as a subprotocol ("only send one of ...").
        request.setValue("Bearer \(secret)", forHTTPHeaderField: "Authorization")
        session = URLSession(configuration: .default)
        task = session.webSocketTask(with: request)
        task.maximumMessageSize = 16 * 1_024 * 1_024
    }

    func open() {
        task.resume()
    }

    func send(_ object: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: object),
              let text = String(data: data, encoding: .utf8) else { return }
        send(text: text)
    }

    /// Built by hand: base64 needs no escaping, and this runs 20 times a second.
    func sendAudio(_ pcm: Data) {
        send(text: "{\"type\":\"input_audio_buffer.append\",\"audio\":\"\(pcm.base64EncodedString())\"}")
    }

    func receive() async throws -> Data {
        switch try await task.receive() {
        case .string(let text):
            return Data(text.utf8)
        case .data(let data):
            return data
        @unknown default:
            return Data()
        }
    }

    func close() {
        task.cancel(with: .normalClosure, reason: nil)
        session.invalidateAndCancel()
    }

    private func send(text: String) {
        task.send(.string(text)) { _ in }
    }
}
