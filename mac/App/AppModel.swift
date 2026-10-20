import AppKit
import Foundation
import Observation
import UserNotifications
import WebKit

/// App-wide state: the session, the main window's web view, and the agents
/// that need you (for the menu bar and the Dock badge).
@MainActor
@Observable
final class AppModel {
    static let shared = AppModel()

    let session = Session()
    private(set) var needs: [NeedsYouItem] = []
    private(set) var lastPollFailed = false

    @ObservationIgnored weak var webView: WKWebView?
    /// A page to open once the web view exists (a notification clicked while
    /// the window was closed).
    @ObservationIgnored var pendingURL: URL?
    /// Set by the main window and the menu bar from SwiftUI's `openWindow`.
    @ObservationIgnored var openMainWindow: (() -> Void)?

    @ObservationIgnored private var pollTask: Task<Void, Never>?
    @ObservationIgnored private var seen: Set<String>?

    static let pollInterval: Duration = .seconds(30)
    static let lastURLKey = "lastURL"

    // MARK: Window and navigation

    func showMainWindow() {
        NSApp.activate()
        if let window = NSApp.windows.first(where: { $0.identifier?.rawValue.hasPrefix(RudderMacApp.mainWindowID) == true }) {
            if window.isMiniaturized { window.deminiaturize(nil) }
            window.makeKeyAndOrderFront(nil)
        } else {
            openMainWindow?()
        }
    }

    /// Opens a page of the app (`/app/...`) in the main window.
    func open(path: String?) {
        let url = RudderLinks.pageURL(session.server, path: path ?? "/app") ?? session.appURL
        if let webView {
            webView.load(URLRequest(url: url))
        } else {
            pendingURL = url
        }
        showMainWindow()
    }

    /// Where the web view starts: a pending page, else the last page, else /app.
    func startURL() -> URL {
        if let url = pendingURL {
            pendingURL = nil
            return url
        }
        return RudderLinks.restorable(UserDefaults.standard.string(forKey: Self.lastURLKey), server: session.server)
            ?? session.appURL
    }

    func reload() {
        guard let webView else { return }
        if webView.url == nil { webView.load(URLRequest(url: session.appURL)) } else { webView.reload() }
    }

    func goBack() { webView?.goBack() }
    func goForward() { webView?.goForward() }
    func openSettings() { open(path: "/app/settings") }

    func setBadge(_ count: Int) {
        NSApp.dockTile.badgeLabel = count > 0 ? String(count) : nil
    }

    func signOut() {
        session.signOut()
        needs = []
        seen = nil
        UserDefaults.standard.removeObject(forKey: Self.lastURLKey)
    }

    // MARK: Needs-you polling (menu bar)

    func startPolling() {
        guard pollTask == nil else { return }
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.pollOnce()
                try? await Task.sleep(for: AppModel.pollInterval)
            }
        }
    }

    func pollNow() {
        Task { await pollOnce() }
    }

    private func pollOnce() async {
        guard let token = session.token else {
            needs = []
            seen = nil
            return
        }
        var request = URLRequest(url: RudderLinks.stateURL(session.server))
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        // Read by the control plane to leave the machine asleep (see README).
        request.setValue("1", forHTTPHeaderField: "X-Rudder-Passive")
        request.setValue(TimeZone.current.identifier, forHTTPHeaderField: "X-Rudder-Timezone")
        request.timeoutInterval = 20
        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            guard status == 200, let items = NeedsYou.items(fromStateJSON: data) else {
                lastPollFailed = true
                if status == 401 {
                    needs = []
                    seen = nil
                    setBadge(0)
                }
                return
            }
            lastPollFailed = false
            apply(items)
        } catch {
            lastPollFailed = true
        }
    }

    private func apply(_ items: [NeedsYouItem]) {
        needs = items
        setBadge(items.count)
        let keys = Set(items.map { "\($0.workspaceId)|\($0.runId)|\($0.reason)" })
        defer { seen = keys }
        // The first poll is a baseline: nothing is news yet.
        guard let previous = seen else { return }
        let fresh = items.filter { !previous.contains("\($0.workspaceId)|\($0.runId)|\($0.reason)") }
        guard let newest = fresh.first else { return }
        if fresh.count == 1 {
            Notifier.shared.post(title: "\(newest.title) \(newest.reason)", body: newest.project, path: newest.path)
        } else {
            Notifier.shared.post(title: "\(fresh.count) agents need you", body: fresh.map(\.title).prefix(3).joined(separator: ", "), path: "/app")
        }
    }
}

/// Local notifications: the page's `notify` and the menu bar's "needs you".
/// A click opens the notification's page in the main window.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Notifier()

    func install() {
        UNUserNotificationCenter.current().delegate = self
    }

    func post(title: String, body: String, path: String?) {
        Task {
            let center = UNUserNotificationCenter.current()
            var settings = await center.notificationSettings()
            if settings.authorizationStatus == .notDetermined {
                _ = try? await center.requestAuthorization(options: [.alert, .sound, .badge])
                settings = await center.notificationSettings()
            }
            guard settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional else { return }
            let content = UNMutableNotificationContent()
            content.title = String(title.prefix(200))
            content.body = String(body.prefix(500))
            content.sound = .default
            if let path { content.userInfo = ["path": path] }
            let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
            try? await center.add(request)
        }
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let path = response.notification.request.content.userInfo["path"] as? String
        await MainActor.run { AppModel.shared.open(path: path) }
    }
}
