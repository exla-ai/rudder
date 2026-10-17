import AuthenticationServices
import Network
import UIKit

/// Signs the cloud machine's Claude Code or Codex in, from the phone.
///
/// The CLI on the cloud machine waits for its sign-in page to redirect to
/// `http://localhost:<port>/<path>` (a server it runs there). That redirect
/// happens in the phone's browser, so while the page is open this listens on
/// the phone's own loopback at the same port, answers the request with a
/// "signed in" page, and hands its query to the web app through
/// `window.rudderAuthorized`. The web app sends it to the machine, which
/// replays it against the CLI. Nothing to copy or paste.
///
/// The page opens in ASWebAuthenticationSession, which shares Safari's
/// cookies, so someone already signed in to claude.ai or ChatGPT in Safari only
/// taps Authorize. Without a port it only shows a page (Claude's paste-a-code
/// page, Codex's device page) and reports `cancelled` when it closes.
@MainActor
final class AgentLogin: NSObject {
    enum Outcome {
        case callback(String)
        case cancelled
        case failed(String)
    }

    private var session: ASWebAuthenticationSession?
    private var listener: NWListener?
    private var completion: ((Outcome) -> Void)?
    private var expectedPath = "/"
    private var timeout: Task<Void, Never>?
    /// Which sheet is current: a closed sheet reports back after a delay, and
    /// must not end the sign-in that replaced it.
    private var generation = 0

    func start(url: URL, port: UInt16?, path: String?, completion: @escaping (Outcome) -> Void) {
        // A sign-in still open from before ends as cancelled first.
        deliver(.cancelled)
        teardown()
        self.completion = completion
        if let port, let path {
            guard listen(port: port, path: path) else { return }
        }
        generation += 1
        let current = generation
        let auth = ASWebAuthenticationSession(url: url, callbackURLScheme: "rudder") { [weak self] _, error in
            Task { @MainActor in self?.sessionEnded(error, generation: current) }
        }
        auth.presentationContextProvider = self
        auth.prefersEphemeralWebBrowserSession = false
        session = auth
        if !auth.start() {
            deliver(.failed("Could not open the sign-in page."))
            teardown()
            return
        }
        timeout = Task { [weak self] in
            try? await Task.sleep(for: .seconds(15 * 60))
            guard !Task.isCancelled else { return }
            self?.deliver(.failed("The sign-in took too long. Start it again."))
            self?.teardown()
        }
    }

    private func listen(port: UInt16, path: String) -> Bool {
        expectedPath = path
        guard let listenPort = NWEndpoint.Port(rawValue: port) else {
            deliver(.failed("The sign-in asked for an unusable port."))
            return false
        }
        let parameters = NWParameters.tcp
        // Loopback only: the redirect comes from this phone's browser, and
        // nothing on the network should be able to reach the listener.
        parameters.requiredInterfaceType = .loopback
        parameters.allowLocalEndpointReuse = true
        do {
            let listener = try NWListener(using: parameters, on: listenPort)
            listener.newConnectionHandler = { [weak self] connection in
                MainActor.assumeIsolated { self?.accept(connection) }
            }
            listener.stateUpdateHandler = { [weak self] state in
                guard case .failed(let error) = state else { return }
                MainActor.assumeIsolated { self?.listenerFailed(error) }
            }
            listener.start(queue: .main)
            self.listener = listener
            return true
        } catch {
            deliver(.failed("Could not wait for the sign-in here (\(error.localizedDescription))."))
            return false
        }
    }

    private func accept(_ connection: NWConnection) {
        connection.start(queue: .main)
        connection.receive(minimumIncompleteLength: 1, maximumLength: 16_384) { [weak self] data, _, _, _ in
            MainActor.assumeIsolated {
                guard let self else {
                    connection.cancel()
                    return
                }
                self.handle(connection, request: data ?? Data())
            }
        }
    }

    private func handle(_ connection: NWConnection, request: Data) {
        // "GET /callback?code=...&state=... HTTP/1.1"
        let head = String(decoding: request.prefix(8192), as: UTF8.self)
        let requestLine = head.components(separatedBy: "\r\n").first ?? ""
        let parts = requestLine.split(separator: " ")
        guard parts.count >= 2, parts[0] == "GET" else {
            respond(connection, status: "400 Bad Request", title: "Not a sign-in", detail: "")
            return
        }
        let target = String(parts[1])
        let pathPart = target.split(separator: "?", maxSplits: 1).first.map(String.init) ?? target
        guard pathPart == expectedPath, completion != nil else {
            // The browser also asks for /favicon.ico and the like.
            respond(connection, status: "404 Not Found", title: "Not found", detail: "")
            return
        }
        let query = target.firstIndex(of: "?").map { String(target[target.index(after: $0)...]) } ?? ""
        respond(connection, status: "200 OK", title: "Signed in", detail: "Finishing on your cloud machine. Back to Rudder…")
        deliver(.callback(query))
        listener?.cancel()
        listener = nil
        // Leave the page up for a moment so it reads as done, then close it.
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(700))
            self?.teardown()
        }
    }

    private func respond(_ connection: NWConnection, status: String, title: String, detail: String) {
        let body = Data(Self.page(title: title, detail: detail).utf8)
        let head = "HTTP/1.1 \(status)\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: \(body.count)\r\n"
            + "Cache-Control: no-store\r\nConnection: close\r\n\r\n"
        connection.send(content: Data(head.utf8) + body, completion: .contentProcessed { _ in
            connection.cancel()
        })
    }

    private func listenerFailed(_ error: NWError) {
        // Without the listener the redirect has nowhere to land: say so now,
        // while the web app can still offer the code instead.
        deliver(.failed("Could not wait for the sign-in here (\(error.localizedDescription))."))
        teardown()
    }

    private func sessionEnded(_ error: Error?, generation ended: Int) {
        guard ended == generation else { return }
        if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
            deliver(.cancelled)
        } else if let error {
            deliver(.failed(error.localizedDescription))
        } else {
            deliver(.cancelled)
        }
        teardown()
    }

    /// Calls the completion once; later outcomes (the sheet closing after a
    /// callback already came in) are dropped.
    private func deliver(_ outcome: Outcome) {
        guard let completion else { return }
        self.completion = nil
        completion(outcome)
    }

    private func teardown() {
        timeout?.cancel()
        timeout = nil
        listener?.cancel()
        listener = nil
        let open = session
        session = nil
        open?.cancel()
    }

    private static func page(title: String, detail: String) -> String {
        """
        <!doctype html><html><head><meta charset="utf-8">
        <meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
        <title>\(title)</title>
        <style>
        :root{color-scheme:light dark}
        body{font:17px -apple-system,system-ui;margin:0;min-height:100vh;display:flex;align-items:center;
        justify-content:center;text-align:center;padding:24px;box-sizing:border-box;background:#f7f6f3;color:#1c1b19}
        @media(prefers-color-scheme:dark){body{background:#151413;color:#ecebe8}}
        h1{font-size:22px;margin:0 0 8px}p{margin:0;color:#8a877f}
        </style></head><body><div><h1>\(title)</h1><p>\(detail)</p></div></body></html>
        """
    }
}

extension AgentLogin: ASWebAuthenticationPresentationContextProviding {
    nonisolated func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
            return scenes.flatMap(\.windows).first(where: \.isKeyWindow) ?? ASPresentationAnchor()
        }
    }
}
