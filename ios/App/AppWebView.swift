import SafariServices
import SwiftUI
import WebKit

/// The Rudder app (`/app`) in a WKWebView.
///
/// The web app checks `window.RUDDER_NATIVE` (injected before any page script
/// runs) to send the bearer token on its API calls, and talks back through the
/// `rudder` message handler: `signIn` (token rejected), `signOut`,
/// `openExternal` (links leave the app), `badge` (agents that need you), and
/// `pick` (the attach menu's Photos, Camera and Files, see AttachmentPicker),
/// `authorize` (signing the cloud machine's Claude Code or Codex in, see
/// AgentLogin), and `voice` (the full-screen "Talk to Rudder" conversation,
/// see VoiceAssistant). `RUDDER_NATIVE.attachments` says this build may open
/// the camera, `RUDDER_NATIVE.picker` that it answers `pick`,
/// `RUDDER_NATIVE.login` that it answers `authorize`,
/// `RUDDER_NATIVE.voice` that it answers `voice`, and
/// `RUDDER_NATIVE.digest` that it answers `digestReminder` (the 9:01
/// notification, see DigestReminder).
struct AppWebView: UIViewRepresentable {
    let session: Session
    let token: String

    func makeCoordinator() -> Coordinator {
        Coordinator(session: session)
    }

    func makeUIView(context: Context) -> WKWebView {
        let controller = WKUserContentController()
        controller.addUserScript(WKUserScript(
            source: nativeBootstrap(token: token),
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        ))
        controller.add(context.coordinator, name: "rudder")

        let configuration = WKWebViewConfiguration()
        configuration.userContentController = controller
        configuration.allowsInlineMediaPlayback = true
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true
        configuration.applicationNameForUserAgent = "RudderiOS/\(Bundle.main.shortVersion)"

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.allowsBackForwardNavigationGestures = true
        webView.isOpaque = false
        webView.backgroundColor = UIColor(Theme.background)
        webView.scrollView.backgroundColor = UIColor(Theme.background)
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.scrollView.keyboardDismissMode = .interactive
        #if DEBUG
        webView.isInspectable = true
        #endif

        let refresh = UIRefreshControl()
        refresh.addTarget(context.coordinator, action: #selector(Coordinator.pullToRefresh(_:)), for: .valueChanged)
        webView.scrollView.refreshControl = refresh

        context.coordinator.webView = webView
        context.coordinator.lastTick = session.foregroundTick
        context.coordinator.lastDigestTick = session.openDigestTick
        // Launched by tapping the digest notification: open on the digest.
        var start = session.openDigestTick > 0 ? session.appURL.appendingPathComponent("digest") : session.appURL
        #if DEBUG
        // `-startPath /app/p/<project>/a/<agent>` opens a screen directly (simulator tests).
        if let path = UserDefaults.standard.string(forKey: "startPath"), path.hasPrefix("/app"),
           let url = URL(string: path, relativeTo: session.server) {
            start = url
        }
        #endif
        webView.load(URLRequest(url: start))
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        // Coming back to the foreground: ask the page for fresh state instead of
        // reloading it (which would lose scroll position and drafts).
        if context.coordinator.lastTick != session.foregroundTick {
            context.coordinator.lastTick = session.foregroundTick
            webView.evaluateJavaScript("window.rudderRefresh && window.rudderRefresh()")
        }
        if context.coordinator.lastDigestTick != session.openDigestTick {
            context.coordinator.lastDigestTick = session.openDigestTick
            webView.evaluateJavaScript("window.rudderOpenDigest ? window.rudderOpenDigest() : location.assign('/app/digest')")
        }
    }

    static func dismantleUIView(_ webView: WKWebView, coordinator: Coordinator) {
        webView.configuration.userContentController.removeScriptMessageHandler(forName: "rudder")
    }

    private func nativeBootstrap(token: String) -> String {
        let payload: [String: String] = [
            "token": token,
            "platform": "ios",
            "version": Bundle.main.shortVersion,
            // The camera and photo usage strings are in Info.plist, so the web
            // app may offer "Take Photo" (without them iOS kills the app).
            "attachments": "1",
            // Photos, Camera and Files each open their native picker directly.
            "picker": "1",
            // Agent sign-ins come back through a loopback listener, no code to paste.
            "login": "1",
            // The mic button opens the native voice assistant.
            "voice": "1",
            // The digest's 9:01 local notification.
            "digest": "1",
        ]
        let json = (try? JSONSerialization.data(withJSONObject: payload))
            .flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
        return "window.RUDDER_NATIVE = Object.freeze(\(json));"
    }

    @MainActor
    final class Coordinator: NSObject, WKScriptMessageHandler, WKNavigationDelegate, WKUIDelegate {
        let session: Session
        let attachmentPicker: AttachmentPicker
        let agentLogin: AgentLogin
        weak var webView: WKWebView?
        var lastTick = 0
        var lastDigestTick = 0

        init(session: Session) {
            self.session = session
            self.attachmentPicker = AttachmentPicker()
            self.agentLogin = AgentLogin()
        }

        @objc func pullToRefresh(_ control: UIRefreshControl) {
            webView?.evaluateJavaScript("window.rudderRefresh ? window.rudderRefresh() : location.reload()")
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) {
                control.endRefreshing()
            }
        }

        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            guard let body = message.body as? [String: Any], let type = body["type"] as? String else { return }
            switch type {
            case "signIn", "signOut":
                session.signOut()
            case "openExternal":
                if let string = body["url"] as? String, let url = URL(string: string) {
                    openExternally(url)
                }
            case "badge":
                if let count = body["count"] as? Int {
                    session.setBadge(count)
                } else if let count = body["count"] as? Double {
                    session.setBadge(Int(count))
                }
            case "digestReminder":
                let enabled = body["enabled"] as? Bool ?? true
                Task { @MainActor in
                    let result = await DigestReminder.shared.set(enabled: enabled)
                    self.webView?.evaluateJavaScript(
                        "window.rudderDigestReminder && window.rudderDigestReminder({enabled: \(result.enabled), denied: \(result.denied)})",
                        completionHandler: nil
                    )
                }
            case "haptic":
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
            case "pick":
                guard let id = body["id"] as? String, let presenter = UIApplication.shared.topViewController else { return }
                let kind = body["kind"] as? String ?? "files"
                attachmentPicker.present(kind: kind, from: presenter) { [weak self] result in
                    self?.deliverPicked(id: id, result: result)
                }
            case "authorize":
                guard let id = body["id"] as? String,
                      let string = body["url"] as? String, let url = URL(string: string), url.scheme == "https" else { return }
                // With a port and path, wait for the page's redirect on this
                // phone's loopback; without, only show the page.
                var port: UInt16?
                var path: String?
                if let number = (body["port"] as? NSNumber)?.intValue, let callbackPath = body["path"] as? String {
                    guard (1024...65535).contains(number), callbackPath.hasPrefix("/") else { return }
                    port = UInt16(number)
                    path = callbackPath
                }
                agentLogin.start(url: url, port: port, path: path) { [weak self] outcome in
                    self?.deliverAuthorized(id: id, outcome: outcome)
                }
            case "voice":
                presentVoice()
            default:
                break
            }
        }

        /// Opens "Talk to Rudder" over the app, once.
        private func presentVoice() {
            guard let presenter = UIApplication.shared.topViewController,
                  !(presenter is UIHostingController<VoiceView>) else { return }
            let host = UIHostingController(rootView: VoiceView(onClose: {}))
            host.rootView = VoiceView(onClose: { [weak host] in
                host?.dismiss(animated: true)
            })
            host.modalPresentationStyle = .fullScreen
            host.view.backgroundColor = .black
            presenter.present(host, animated: true)
        }

        /// Tells the page how an agent sign-in ended (`window.rudderAuthorized`):
        /// the callback's query to send to the cloud machine, or why not.
        private func deliverAuthorized(id: String, outcome: AgentLogin.Outcome) {
            var payload: [String: Any] = ["id": id]
            switch outcome {
            case .callback(let query): payload["callback"] = query
            case .cancelled: payload["cancelled"] = true
            case .failed(let message): payload["error"] = message
            }
            guard let data = try? JSONSerialization.data(withJSONObject: payload),
                  let json = String(data: data, encoding: .utf8) else { return }
            webView?.evaluateJavaScript("window.rudderAuthorized && window.rudderAuthorized(\(json))")
        }

        /// Hands picked files to the page as base64 (`window.rudderPicked`),
        /// which turns them back into File objects for the upload path.
        private func deliverPicked(id: String, result: PickResult) {
            let payload: [String: Any] = [
                "id": id,
                "files": result.files.map { ["name": $0.name, "type": $0.type, "data": $0.data.base64EncodedString()] },
                "skipped": result.skipped,
            ]
            guard let data = try? JSONSerialization.data(withJSONObject: payload),
                  let json = String(data: data, encoding: .utf8) else { return }
            webView?.evaluateJavaScript("window.rudderPicked && window.rudderPicked(\(json))")
        }

        // Navigation inside the app stays in the web view; everything else
        // (previews, GitHub, docs) opens outside it.
        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void
        ) {
            guard let url = navigationAction.request.url else {
                decisionHandler(.cancel)
                return
            }
            if url.scheme == "about" || url.scheme == "blob" || url.scheme == "data" {
                decisionHandler(.allow)
                return
            }
            let sameHost = url.host == session.server.host
            let inApp = sameHost && (url.path == "/app" || url.path.hasPrefix("/app/") || url.path.hasPrefix("/api/"))
            if inApp && navigationAction.targetFrame != nil {
                if url.path == "/app/login" {
                    // The web app should not do this in the shell, but if it
                    // does, sign in natively: Google refuses embedded views.
                    decisionHandler(.cancel)
                    session.signOut()
                    return
                }
                decisionHandler(.allow)
                return
            }
            decisionHandler(.cancel)
            openExternally(url)
        }

        func webView(
            _ webView: WKWebView,
            createWebViewWith configuration: WKWebViewConfiguration,
            for navigationAction: WKNavigationAction,
            windowFeatures: WKWindowFeatures
        ) -> WKWebView? {
            if let url = navigationAction.request.url {
                openExternally(url)
            }
            return nil
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            showOffline(webView, error: error)
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
            showOffline(webView, error: error)
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            webView.reload()
        }

        private func showOffline(_ webView: WKWebView, error: Error) {
            let nsError = error as NSError
            if nsError.code == NSURLErrorCancelled {
                return
            }
            let message = nsError.localizedDescription
                .replacingOccurrences(of: "&", with: "&amp;")
                .replacingOccurrences(of: "<", with: "&lt;")
            let html = """
            <html><head><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
            <style>body{font:17px -apple-system;display:flex;height:100vh;margin:0;align-items:center;justify-content:center;
            text-align:center;padding:24px;box-sizing:border-box;color:#555;background:transparent}
            @media(prefers-color-scheme:dark){body{color:#aaa}}a{color:#4a7bd8;text-decoration:none;font-weight:600}</style></head>
            <body><div><p>Can't reach Rudder.</p><p style="font-size:14px">\(message)</p>
            <p><a href="\(session.appURL.absoluteString)">Try again</a></p></div></body></html>
            """
            webView.loadHTMLString(html, baseURL: session.server)
        }

        private func openExternally(_ url: URL) {
            guard url.scheme == "http" || url.scheme == "https" else {
                UIApplication.shared.open(url)
                return
            }
            // Universal links (GitHub, Claude) go to their apps when installed.
            UIApplication.shared.open(url, options: [.universalLinksOnly: true]) { opened in
                guard !opened else { return }
                Task { @MainActor in
                    let safari = SFSafariViewController(url: url)
                    safari.dismissButtonStyle = .close
                    UIApplication.shared.topViewController?.present(safari, animated: true)
                }
            }
        }
    }
}

extension Bundle {
    var shortVersion: String {
        (infoDictionary?["CFBundleShortVersionString"] as? String) ?? "1.0"
    }
}

extension UIApplication {
    var topViewController: UIViewController? {
        let scenes = connectedScenes.compactMap { $0 as? UIWindowScene }
        var top = scenes.flatMap(\.windows).first(where: \.isKeyWindow)?.rootViewController
        while let presented = top?.presentedViewController {
            top = presented
        }
        return top
    }
}
