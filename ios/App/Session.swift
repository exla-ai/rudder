import AuthenticationServices
import Foundation
import Observation
import Security
import UIKit
import UserNotifications
import WebKit

/// Who is signed in, and how to sign in.
///
/// Sign-in runs Google (or GitHub) in an `ASWebAuthenticationSession` against
/// the control plane's `/app/login?native=1`. When the browser flow finishes,
/// the control plane redirects to `rudder://auth?token=rdr_...`: a bearer token
/// for the same account `rudder login` uses on the laptop. It lives in the
/// Keychain and is handed to the web app at document start.
@MainActor
@Observable
final class Session {
    static let defaultServer = URL(string: "https://rudder-cloud-control.fly.dev")!

    private(set) var token: String?
    var signingIn = false
    var errorMessage: String?
    /// Bumped when the app comes to the foreground so the web view refreshes.
    private(set) var foregroundTick = 0
    /// Bumped when the 9:01 digest notification is tapped (DigestReminder).
    private(set) var openDigestTick = 0

    let server: URL
    private var authSession: ASWebAuthenticationSession?
    private let presenter = PresentationAnchor()

    init() {
        server = Session.resolvedServer()
        token = Keychain.read()
        #if DEBUG
        // Simulator runs can be scripted: `-probeToken rdr_...` signs in directly.
        if let probe = UserDefaults.standard.string(forKey: "probeToken"), !probe.isEmpty {
            token = probe
        }
        // `-pairCode ABCD-EFGH` runs the same pairing a typed code or a
        // rudder://pair link does, so the simulator can sign in unattended.
        if let code = UserDefaults.standard.string(forKey: "pairCode"), !code.isEmpty {
            Task { await self.pair(code: code) }
        }
        #endif
        noteOpened()
    }

    // MARK: Sign-in steps

    private static let deviceKey = "analyticsDevice"
    /// The sign-in under way (google, github or pair), for its outcome.
    private var signInProvider = "google"

    /// The steps before there is an account (first open, the sign-in screen,
    /// a sign-in button, signed in or not), keyed by a random id this phone
    /// keeps in UserDefaults. The control plane hashes it, and nothing else
    /// about the phone goes with it. The web app cannot count these: it loads
    /// only after sign-in.
    private func deviceEvent(_ name: String, _ props: [String: String] = [:]) {
        guard let device = UserDefaults.standard.string(forKey: Self.deviceKey) else { return }
        var request = URLRequest(url: server.appendingPathComponent("api/app/anon-event"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        var all = props
        all["platform"] = "ios"
        let body: [String: Any] = ["name": name, "device": device, "props": all]
        request.httpBody = try? JSONSerialization.data(withJSONObject: body)
        let ping = request
        Task { _ = try? await URLSession.shared.data(for: ping) }
    }

    /// Gives this phone its id on its first launch and counts that launch. A
    /// phone that is already signed in (an update) only gets its id.
    private func noteOpened() {
        let first = UserDefaults.standard.string(forKey: Self.deviceKey) == nil
        if first {
            UserDefaults.standard.set(UUID().uuidString.lowercased(), forKey: Self.deviceKey)
        }
        guard token == nil else { return }
        if first {
            deviceEvent("app_first_opened")
        }
        deviceEvent("signin_viewed")
    }

    /// The control plane: the default, or a `RudderServerURL` override. A
    /// function so the voice assistant and CarPlay, which can run without
    /// this object, resolve it the same way.
    static func resolvedServer() -> URL {
        if let override = UserDefaults.standard.string(forKey: "RudderServerURL"), let url = URL(string: override) {
            return url
        }
        return defaultServer
    }

    var appURL: URL { server.appendingPathComponent("app") }

    func signIn(provider: String = "google") {
        guard !signingIn else { return }
        errorMessage = nil
        signingIn = true
        signInProvider = provider
        deviceEvent("signin_clicked", ["provider": provider])
        var components = URLComponents(url: server.appendingPathComponent("app/login"), resolvingAgainstBaseURL: false)!
        components.queryItems = [
            URLQueryItem(name: "native", value: "1"),
            URLQueryItem(name: "provider", value: provider),
        ]
        let session = ASWebAuthenticationSession(url: components.url!, callbackURLScheme: "rudder") { [weak self] url, error in
            Task { @MainActor in
                self?.finishSignIn(url: url, error: error)
            }
        }
        session.presentationContextProvider = presenter
        // Share the Safari cookie jar so an existing Google session is reused.
        session.prefersEphemeralWebBrowserSession = false
        authSession = session
        if !session.start() {
            signingIn = false
            errorMessage = "Could not open the sign-in window."
            deviceEvent("signin_failed", ["provider": provider, "reason": "no_window"])
        }
    }

    private func finishSignIn(url: URL?, error: Error?) {
        signingIn = false
        authSession = nil
        if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
            deviceEvent("signin_failed", ["provider": signInProvider, "reason": "cancelled"])
            return
        }
        if let error {
            errorMessage = error.localizedDescription
            deviceEvent("signin_failed", ["provider": signInProvider, "reason": "error"])
            return
        }
        let items = url.flatMap { URLComponents(url: $0, resolvingAgainstBaseURL: false)?.queryItems } ?? []
        if let token = items.first(where: { $0.name == "token" })?.value, token.hasPrefix("rdr_") {
            Keychain.write(token)
            self.token = token
            deviceEvent("signin_completed", ["provider": signInProvider])
        } else {
            // The control plane counted this refusal itself (signin_failed).
            // The server says why (an email already signed up another way, a
            // cancelled or expired sign-in).
            errorMessage = items.first(where: { $0.name == "message" })?.value ?? "Sign-in did not finish. Try again."
        }
    }

    /// Sign in with a one-time code from `rudder cloud app` on the laptop. The
    /// laptop is already signed in, so the phone needs no OAuth provider.
    func pair(code: String) async {
        let trimmed = code.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !signingIn else { return }
        errorMessage = nil
        signingIn = true
        defer { signingIn = false }
        deviceEvent("signin_clicked", ["provider": "pair"])
        var request = URLRequest(url: server.appendingPathComponent("api/app/pair/finish"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("ios", forHTTPHeaderField: "X-Rudder-Platform")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["code": trimmed])
        do {
            let (data, _) = try await URLSession.shared.data(for: request)
            let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            if let token = body["token"] as? String, token.hasPrefix("rdr_") {
                Keychain.write(token)
                self.token = token
                deviceEvent("signin_completed", ["provider": "pair"])
            } else {
                // The control plane counted a refused code itself (signin_failed).
                errorMessage = (body["error"] as? String) ?? "That code didn't work."
            }
        } catch {
            errorMessage = error.localizedDescription
            deviceEvent("signin_failed", ["provider": "pair", "reason": "error"])
        }
    }

    /// rudder://pair?code=ABCD-EFGH (printed by `rudder cloud app`).
    func handle(url: URL) {
        guard url.scheme == "rudder", url.host == "pair",
              let code = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "code" })?.value else { return }
        Task { await pair(code: code) }
    }

    /// Signs this device out so a different account can sign in next: the
    /// token is revoked on the server, and the web view's storage (the old
    /// account's org scope, last screen, drafts and service worker cache) and
    /// the 9:01 digest notification go with it. Choosing the account again is
    /// the server's `prompt=select_account`, since the sign-in sheet shares
    /// Safari's Google and GitHub sessions.
    func signOut() {
        let revoked = token
        Keychain.delete()
        token = nil
        signingIn = false
        authSession?.cancel()
        authSession = nil
        setBadge(0)
        UNUserNotificationCenter.current().removePendingNotificationRequests(withIdentifiers: [DigestReminder.identifier])
        let store = WKWebsiteDataStore.default()
        store.removeData(ofTypes: WKWebsiteDataStore.allWebsiteDataTypes(), modifiedSince: .distantPast) {}
        if let revoked {
            var request = URLRequest(url: server.appendingPathComponent("api/app/sign-out"))
            request.httpMethod = "POST"
            request.setValue("Bearer \(revoked)", forHTTPHeaderField: "Authorization")
            // Best effort: signed out on this device whether or not it lands.
            URLSession.shared.dataTask(with: request).resume()
        }
    }

    func didBecomeActive() {
        foregroundTick += 1
    }

    func openDigest() {
        openDigestTick += 1
    }

    func setBadge(_ count: Int) {
        let center = UNUserNotificationCenter.current()
        Task {
            let settings = await center.notificationSettings()
            if settings.authorizationStatus == .notDetermined {
                // Ask only when there is something to show, not on first launch.
                guard count > 0 else { return }
                _ = try? await center.requestAuthorization(options: [.badge])
            }
            try? await center.setBadgeCount(max(0, count))
        }
    }
}

/// The window ASWebAuthenticationSession presents over.
private final class PresentationAnchor: NSObject, ASWebAuthenticationPresentationContextProviding {
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
            return scenes.flatMap(\.windows).first(where: \.isKeyWindow) ?? ASPresentationAnchor()
        }
    }
}

/// The bearer token, in the Keychain (this device only, after first unlock so a
/// background refresh can still read it).
enum Keychain {
    private static let service = "dev.viraat.rudder"
    private static let account = "rudder-token"

    static func read() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func write(_ token: String) {
        delete()
        let item: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueData as String: Data(token.utf8),
        ]
        SecItemAdd(item as CFDictionary, nil)
    }

    static func delete() {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
