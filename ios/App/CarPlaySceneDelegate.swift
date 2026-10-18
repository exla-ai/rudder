import CarPlay
import UIKit

/// Rudder in the car: a voice-based conversation app (Apple's CarPlay
/// category for this), so the screen is one short list and everything else
/// is spoken.
///
/// The root list has "Talk to Rudder" and the agents that need the user.
/// Tapping either opens the shared VoiceAssistant (an agent row asks Rudder to
/// open by saying what that agent needs) and shows the system voice control
/// template, switched between Listening, Thinking and Rudder as the
/// conversation goes. The template closes when the conversation ends, and the
/// list refreshes.
///
/// `@objc(CarPlaySceneDelegate)` so Info.plist can name the class without a
/// module prefix, whatever the build calls the module.
@objc(CarPlaySceneDelegate)
final class CarPlaySceneDelegate: UIResponder, CPTemplateApplicationSceneDelegate {
    private static let maxNeedsRows = 8
    /// How often the list rereads the agents while it is on screen.
    private static let refreshInterval: Duration = .seconds(20)

    private var interfaceController: CPInterfaceController?
    private var listTemplate: CPListTemplate?
    private var voiceTemplate: CPVoiceControlTemplate?
    private var observer: UUID?
    private var refreshTask: Task<Void, Never>?
    private var pollTask: Task<Void, Never>?
    /// Shown under "Talk to Rudder" after a conversation ended with an error.
    private var lastError: String?

    func templateApplicationScene(
        _ templateApplicationScene: CPTemplateApplicationScene,
        didConnect interfaceController: CPInterfaceController
    ) {
        self.interfaceController = interfaceController
        let list = CPListTemplate(title: "Rudder", sections: sections(for: .loading))
        listTemplate = list
        interfaceController.setRootTemplate(list, animated: false, completion: nil)
        observer = VoiceAssistant.shared.addStateObserver { [weak self] state in
            self?.voiceStateChanged(state)
        }
        if VoiceAssistant.shared.isActive {
            voiceStateChanged(VoiceAssistant.shared.state)
        }
        refresh()
        startPolling()
    }

    func templateApplicationScene(
        _ templateApplicationScene: CPTemplateApplicationScene,
        didDisconnectInterfaceController interfaceController: CPInterfaceController
    ) {
        // Unplugging the car ends a conversation the car started; one the
        // phone started carries on there.
        if VoiceAssistant.shared.surface == .carplay {
            VoiceAssistant.shared.stop()
        }
        if let observer {
            VoiceAssistant.shared.removeStateObserver(observer)
        }
        observer = nil
        refreshTask?.cancel()
        refreshTask = nil
        pollTask?.cancel()
        pollTask = nil
        voiceTemplate = nil
        listTemplate = nil
        self.interfaceController = nil
    }

    // MARK: - Voice

    private func startVoice(focus: String?) {
        lastError = nil
        let assistant = VoiceAssistant.shared
        if assistant.isActive {
            // Already talking (maybe from the phone): show it.
            voiceStateChanged(assistant.state)
        } else {
            assistant.start(surface: .carplay, focus: focus)
        }
    }

    private func voiceStateChanged(_ state: VoiceState) {
        switch state {
        case .connecting, .thinking:
            showVoice(activating: "thinking")
        case .listening:
            showVoice(activating: "listening")
        case .speaking:
            showVoice(activating: "speaking")
        case .idle:
            hideVoice()
            refresh()
        case .error(let message):
            lastError = message
            hideVoice()
            refresh()
        }
    }

    private func showVoice(activating identifier: String) {
        guard let interfaceController else { return }
        if let voiceTemplate {
            voiceTemplate.activateVoiceControlState(withIdentifier: identifier)
            return
        }
        let template = CPVoiceControlTemplate(voiceControlStates: [
            CPVoiceControlState(identifier: "listening", titleVariants: ["Listening"], image: UIImage(systemName: "waveform"), repeats: true),
            CPVoiceControlState(identifier: "thinking", titleVariants: ["Thinking"], image: UIImage(systemName: "ellipsis"), repeats: true),
            CPVoiceControlState(identifier: "speaking", titleVariants: ["Rudder"], image: UIImage(systemName: "speaker.wave.2"), repeats: true),
        ])
        voiceTemplate = template
        template.activateVoiceControlState(withIdentifier: identifier)
        interfaceController.presentTemplate(template, animated: true, completion: nil)
    }

    private func hideVoice() {
        guard voiceTemplate != nil else { return }
        voiceTemplate = nil
        interfaceController?.dismissTemplate(animated: true, completion: nil)
    }

    // MARK: - The list

    private enum Needs {
        case loading
        case signedOut
        case waking
        case failed(String)
        case agents([NeedsRow])
    }

    private struct NeedsRow {
        let title: String
        let detail: String
        let focus: String
    }

    /// Rereads the list every 20 seconds while it is what the driver sees:
    /// not during a conversation (the voice template covers it, and the
    /// conversation's end refreshes it anyway).
    private func startPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.refreshInterval)
                guard !Task.isCancelled, let self, self.interfaceController != nil else { return }
                if self.voiceTemplate == nil && !VoiceAssistant.shared.isActive {
                    self.refresh()
                }
            }
        }
    }

    private func refresh() {
        refreshTask?.cancel()
        refreshTask = Task { [weak self] in
            guard let self else { return }
            let needs = await self.fetchNeeds()
            guard !Task.isCancelled else { return }
            self.listTemplate?.updateSections(self.sections(for: needs))
        }
    }

    private func sections(for needs: Needs) -> [CPListSection] {
        let talk = CPListItem(
            text: "Talk to Rudder",
            detailText: lastError ?? "Ask about your agents or give one a task",
            image: UIImage(systemName: "waveform")
        )
        talk.handler = { [weak self] _, completion in
            MainActor.assumeIsolated { self?.startVoice(focus: nil) }
            completion()
        }
        var sections = [CPListSection(items: [talk])]

        var items: [CPListItem] = []
        switch needs {
        case .loading:
            items = [Self.note("Checking your agents")]
        case .signedOut:
            items = [Self.note("Sign in to Rudder on your iPhone")]
        case .waking:
            items = [Self.note("Your cloud machine is waking up")]
        case .failed(let message):
            items = [Self.note(message)]
        case .agents(let rows):
            items = rows.prefix(Self.maxNeedsRows).map { row in
                let item = CPListItem(text: row.title, detailText: row.detail, image: UIImage(systemName: "exclamationmark.bubble"))
                let focus = row.focus
                item.handler = { [weak self] _, completion in
                    MainActor.assumeIsolated { self?.startVoice(focus: focus) }
                    completion()
                }
                return item
            }
            if items.isEmpty {
                items = [Self.note("Nothing needs you right now")]
            }
        }
        sections.append(CPListSection(items: items, header: "Needs you", sectionIndexTitle: nil))
        return sections
    }

    /// A row that only says something; tapping it does nothing.
    private static func note(_ text: String) -> CPListItem {
        let item = CPListItem(text: text, detailText: nil)
        item.isEnabled = false
        return item
    }

    /// `GET /api/app/state`, reduced to the agents that need the user, with
    /// the same rule as the web app's `needsYou`: an open prompt, or waiting,
    /// ready for review, or failed.
    private func fetchNeeds() async -> Needs {
        guard let token = Keychain.read(), !token.isEmpty else { return .signedOut }
        var request = URLRequest(url: Session.resolvedServer().appendingPathComponent("api/app/state"), timeoutInterval: 20)
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        guard let result = try? await URLSession.shared.data(for: request) else {
            return .failed("Can't reach Rudder")
        }
        let (data, response) = result
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        if status == 401 { return .signedOut }
        if status == 503, body["waking"] as? Bool == true { return .waking }
        guard status == 200 else { return .failed("Rudder is not answering") }
        // A home machine whose control socket is down is asleep or booting;
        // opening the state wakes it, and its agents list is stale meanwhile.
        if let home = body["home"] as? [String: Any], home["connected"] as? Bool == false {
            return .waking
        }

        var found: [(row: NeedsRow, updated: Double)] = []
        for project in body["projects"] as? [[String: Any]] ?? [] {
            let projectName = project["name"] as? String ?? "Project"
            for agent in project["agents"] as? [[String: Any]] ?? [] {
                let asks = (agent["asks"] as? [Any] ?? []).compactMap { $0 as? [String: Any] }
                let agentStatus = agent["status"] as? String ?? ""
                guard !asks.isEmpty || ["waiting", "review", "failed"].contains(agentStatus) else { continue }
                let title = Self.nonEmpty(agent["title"]) ?? Self.nonEmpty(agent["task"]) ?? "Untitled agent"
                let detail = "\(projectName) · \(Self.label(status: agentStatus, ask: asks.first))"
                let updated = (agent["updatedAt"] as? NSNumber)?.doubleValue ?? 0
                found.append((row: NeedsRow(title: title, detail: detail, focus: "\(projectName): \(title)"), updated: updated))
            }
        }
        found.sort { $0.updated > $1.updated }
        return .agents(found.map { $0.row })
    }

    private static func nonEmpty(_ value: Any?) -> String? {
        guard let text = (value as? String)?.trimmingCharacters(in: .whitespacesAndNewlines), !text.isEmpty else { return nil }
        return text
    }

    /// The web app's labels (ASK_LABEL and STATUS in app.js).
    private static func label(status: String, ask: [String: Any]?) -> String {
        if let ask {
            switch ask["kind"] as? String {
            case "question": return "Has a question"
            case "plan": return "Plan ready"
            default: return "Needs approval"
            }
        }
        switch status {
        case "waiting": return "Needs input"
        case "review": return "Ready for review"
        case "failed": return "Failed"
        default: return status.capitalized
        }
    }
}
