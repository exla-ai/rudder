import AuthenticationServices
import Foundation
import Observation
import Security
import UIKit
import UserNotifications

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

    let server: URL
    private var authSession: ASWebAuthenticationSession?
    private let presenter = PresentationAnchor()

    init() {
        if let override = UserDefaults.standard.string(forKey: "RudderServerURL"), let url = URL(string: override) {
            server = url
        } else {
            server = Session.defaultServer
        }
        token = Keychain.read()
        #if DEBUG
        // Simulator runs can be scripted: `-probeToken rdr_...` signs in directly.
        if let probe = UserDefaults.standard.string(forKey: "probeToken"), !probe.isEmpty {
            token = probe
        }
        #endif
    }

    var appURL: URL { server.appendingPathComponent("app") }

    func signIn(provider: String = "google") {
        guard !signingIn else { return }
        errorMessage = nil
        signingIn = true
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
        }
    }

    private func finishSignIn(url: URL?, error: Error?) {
        signingIn = false
        authSession = nil
        if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
            return
        }
        if let error {
            errorMessage = error.localizedDescription
            return
        }
        let items = url.flatMap { URLComponents(url: $0, resolvingAgainstBaseURL: false)?.queryItems } ?? []
        if let token = items.first(where: { $0.name == "token" })?.value, token.hasPrefix("rdr_") {
            Keychain.write(token)
            self.token = token
        } else {
            errorMessage = "Sign-in did not finish. Try again."
        }
    }

    func signOut() {
        Keychain.delete()
        token = nil
        setBadge(0)
    }

    func didBecomeActive() {
        foregroundTick += 1
    }

    func setBadge(_ count: Int) {
        let center = UNUserNotificationCenter.current()
        Task {
            let settings = await center.notificationSettings()
            if settings.authorizationStatus == .notDetermined {
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
