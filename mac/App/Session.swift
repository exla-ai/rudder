import AppKit
import Foundation
import Observation
import Security
import WebKit

/// Who is signed in, and how to sign in.
///
/// Sign-in is the device login `rudder login` uses, in the default browser:
/// start a login, open Google (or GitHub) for it, and poll until the control
/// plane hands back a bearer `rdr_` token. Only this Mac knows the device
/// code, so no link can sign it into someone else's account. The token lives
/// in the Keychain and is handed to the web app at document start.
@MainActor
@Observable
final class Session {
    private(set) var token: String?
    var signingIn = false
    /// The browser is open on the sign-in page and this Mac is polling.
    private(set) var waitingForBrowser = false
    var errorMessage: String?

    let server: URL
    private var signInTask: Task<Void, Never>?

    init() {
        let override = ProcessInfo.processInfo.environment["RUDDER_CLOUD_URL"]
            ?? UserDefaults.standard.string(forKey: "RudderServerURL")
        server = RudderLinks.server(override: override)
        token = Keychain.read()
    }

    var appURL: URL { RudderLinks.appURL(server) }

    func signIn(_ provider: SignInProvider) {
        guard !signingIn else { return }
        errorMessage = nil
        signingIn = true
        signInTask = Task { await browserSignIn(provider) }
    }

    /// Stops waiting. The browser tab stays open; its login simply expires.
    func cancelSignIn() {
        signInTask?.cancel()
    }

    private func browserSignIn(_ provider: SignInProvider) async {
        defer {
            signingIn = false
            waitingForBrowser = false
            signInTask = nil
        }
        do {
            let login = try await startDeviceLogin()
            let page = RudderLinks.browserLoginURL(server, provider: provider, deviceCode: login.deviceCode)
            guard NSWorkspace.shared.open(page) else {
                errorMessage = "Could not open your browser."
                return
            }
            waitingForBrowser = true
            let deadline = ContinuousClock.now + .seconds(login.expiresIn)
            while ContinuousClock.now < deadline {
                try await Task.sleep(for: .seconds(login.interval))
                switch try await pollDeviceLogin(login.deviceCode) {
                case .waiting:
                    continue
                case .signedIn(let token):
                    Keychain.write(token)
                    self.token = token
                    NSApp.activate()
                    return
                case .denied:
                    errorMessage = "The sign-in was denied on your phone."
                    return
                case .expired:
                    errorMessage = "The sign-in expired. Try again."
                    return
                }
            }
            errorMessage = "The sign-in expired. Try again."
        } catch is CancellationError {
            // Cancelled on the sign-in screen.
        } catch let error as URLError where error.code == .cancelled {
            // Cancelled mid-request.
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func startDeviceLogin() async throws -> DeviceSignIn.Start {
        let body = ["deviceName": ProcessInfo.processInfo.hostName, "client": "mac"]
        let (data, status) = try await post(RudderLinks.loginStartURL(server), body)
        return try DeviceSignIn.start(status: status, body: data)
    }

    private func pollDeviceLogin(_ deviceCode: String) async throws -> DeviceSignIn.Poll {
        let (data, status) = try await post(RudderLinks.loginPollURL(server), ["deviceCode": deviceCode])
        return try DeviceSignIn.poll(status: status, body: data)
    }

    private func post(_ url: URL, _ body: [String: String]) async throws -> (Data, Int) {
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw DeviceSignIn.Failure(message: "Signing in got a reply that was not HTTP.")
        }
        return (data, http.statusCode)
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

    /// rudder://pair?code=... while signed out. Any other rudder:// link (the
    /// browser's "Open Rudder" after sign-in) only brings the app forward.
    func handle(url: URL) {
        guard token == nil, let code = AuthCallback.pairCode(from: url) else { return }
        Task { await pair(code: code) }
    }

    /// Signs this Mac out so a different account can sign in next: the token
    /// is revoked on the server and the web view's storage (the old account's
    /// org scope, last screen and drafts) is cleared. The account picker is
    /// the server's `prompt=select_account`.
    func signOut() {
        let revoked = token
        Keychain.delete()
        token = nil
        NSApp.dockTile.badgeLabel = nil
        WKWebsiteDataStore.default().removeData(ofTypes: WKWebsiteDataStore.allWebsiteDataTypes(), modifiedSince: .distantPast) {}
        if let revoked {
            var request = URLRequest(url: server.appendingPathComponent("api/app/sign-out"))
            request.httpMethod = "POST"
            request.setValue("Bearer \(revoked)", forHTTPHeaderField: "Authorization")
            // Best effort: signed out on this Mac whether or not it lands.
            URLSession.shared.dataTask(with: request).resume()
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
