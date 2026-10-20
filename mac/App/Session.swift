import AppKit
import AuthenticationServices
import Foundation
import Observation
import Security

/// Who is signed in, and how to sign in. The macOS twin of ios/App/Session.swift.
///
/// Sign-in runs Google (or GitHub) in an `ASWebAuthenticationSession` against
/// `/app/login?native=1`, which ends at `rudder://auth?token=rdr_...`: a bearer
/// token for the same account `rudder login` uses. It lives in the Keychain
/// and is handed to the web app at document start.
@MainActor
@Observable
final class Session {
    private(set) var token: String?
    var signingIn = false
    var errorMessage: String?

    let server: URL
    private var authSession: ASWebAuthenticationSession?
    private let anchor = PresentationAnchor()

    init() {
        let override = ProcessInfo.processInfo.environment["RUDDER_CLOUD_URL"]
            ?? UserDefaults.standard.string(forKey: "RudderServerURL")
        server = RudderLinks.server(override: override)
        token = Keychain.read()
    }

    var appURL: URL { RudderLinks.appURL(server) }

    func signIn(provider: String = "google") {
        guard !signingIn else { return }
        errorMessage = nil
        signingIn = true
        let session = ASWebAuthenticationSession(
            url: RudderLinks.loginURL(server, provider: provider),
            callbackURLScheme: "rudder"
        ) { [weak self] url, error in
            Task { @MainActor in
                self?.finishSignIn(url: url, error: error)
            }
        }
        session.presentationContextProvider = anchor
        // The default browser's cookies, so an existing Google session is reused.
        session.prefersEphemeralWebBrowserSession = false
        authSession = session
        if !session.start() {
            signingIn = false
            authSession = nil
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
        if let url, let token = AuthCallback.token(from: url) {
            Keychain.write(token)
            self.token = token
        } else {
            errorMessage = "Sign-in did not finish. Try again."
        }
    }

    /// Sign in with a one-time code from `rudder cloud app` on another computer.
    func pair(code: String) async {
        let trimmed = code.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !signingIn else { return }
        errorMessage = nil
        signingIn = true
        defer { signingIn = false }
        var request = URLRequest(url: RudderLinks.pairURL(server))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["code": trimmed])
        do {
            let (data, _) = try await URLSession.shared.data(for: request)
            let body = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            if let token = body["token"] as? String, token.hasPrefix("rdr_") {
                Keychain.write(token)
                self.token = token
            } else {
                errorMessage = (body["error"] as? String) ?? "That code didn't work."
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// rudder://pair?code=... while signed out. rudder://auth is only ever
    /// accepted from the sign-in session above: a token arriving from any
    /// other link could sign this Mac into someone else's account.
    func handle(url: URL) {
        guard token == nil, let code = AuthCallback.pairCode(from: url) else { return }
        Task { await pair(code: code) }
    }

    func signOut() {
        Keychain.delete()
        token = nil
        NSApp.dockTile.badgeLabel = nil
    }
}

/// The window the sign-in sheet attaches to.
private final class PresentationAnchor: NSObject, ASWebAuthenticationPresentationContextProviding {
    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            NSApp.keyWindow ?? NSApp.mainWindow ?? NSApp.windows.first(where: \.isVisible) ?? ASPresentationAnchor()
        }
    }
}

/// The bearer token, a generic password in the login keychain.
enum Keychain {
    private static let service = "ai.exla.rudder"
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
            kSecAttrLabel as String: "Rudder",
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
