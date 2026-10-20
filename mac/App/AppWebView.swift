import AppKit
import SwiftUI
import WebKit

/// The Rudder app (`/app`) in a WKWebView: the macOS twin of ios/App/AppWebView.swift.
///
/// `window.RUDDER_NATIVE` is injected before any page script runs (platform
/// "mac", the bearer token, `notifications`, `local`, `openLocal`). The page
/// talks back through `window.webkit.messageHandlers.rudder.postMessage`:
///
/// - `signIn` / `signOut`: the token was rejected, or the user signed out.
/// - `openExternal {url}`: the default browser.
/// - `badge {count}`: the Dock badge.
/// - `notify {title, body, path}`: a macOS notification; a click opens `path`.
/// - `localStatus {id?}`: answered with `window.rudderLocalStatus({id, rudder,
///   claude, codex, rudderVersion?})`.
/// - `openLocal {id, runId, repo}`: `rudder cloud pull <runId> --json` in this
///   Mac's checkout of `repo`, answered with `window.rudderLocal({id, ok, path?,
///   title?, checkout?, error?, cancelled?})`, then Terminal / Finder / Xcode.
/// - `openPath {path, app}`: for a workspace this app pulled, "dashboard"
///   (Terminal running `rudder` in its checkout), "terminal", "finder" or
///   "xcode"; answered with `window.rudderOpenedPath({path, app, ok})`.
struct AppWebView: NSViewRepresentable {
    let model: AppModel
    let token: String

    func makeCoordinator() -> Coordinator {
        Coordinator(model: model)
    }

    func makeNSView(context: Context) -> WKWebView {
        let controller = WKUserContentController()
        controller.addUserScript(WKUserScript(
            source: Bridge.bootstrapScript(token: token, version: Bundle.main.shortVersion, local: LocalAgents.shared.lastKnownInstalled),
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        ))
        controller.add(context.coordinator, name: "rudder")

        let configuration = WKWebViewConfiguration()
        configuration.userContentController = controller
        configuration.defaultWebpagePreferences.allowsContentJavaScript = true
        configuration.applicationNameForUserAgent = "RudderMac/\(Bundle.main.shortVersion)"

        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.allowsBackForwardNavigationGestures = true
        webView.allowsMagnification = true
        if let background = NSColor(named: "AccentColor") {
            webView.underPageBackgroundColor = background
        }
        #if DEBUG
        webView.isInspectable = true
        #endif

        context.coordinator.attach(webView)
        webView.load(URLRequest(url: model.startURL()))
        return webView
    }

    func updateNSView(_ webView: WKWebView, context: Context) {}

    static func dismantleNSView(_ webView: WKWebView, coordinator: Coordinator) {
        webView.configuration.userContentController.removeScriptMessageHandler(forName: "rudder")
        coordinator.detach()
    }

    @MainActor
    final class Coordinator: NSObject, WKScriptMessageHandler, WKNavigationDelegate, WKUIDelegate {
        let model: AppModel
        weak var webView: WKWebView?
        private var urlObservation: NSKeyValueObservation?

        init(model: AppModel) {
            self.model = model
        }

        func attach(_ webView: WKWebView) {
            self.webView = webView
            model.webView = webView
            let server = model.session.server
            let key = AppModel.lastURLKey
            // pushState navigations change `url` without a page load, so the
            // last page is remembered from the property, not from didFinish.
            urlObservation = webView.observe(\.url, options: [.new]) { _, change in
                guard let string = change.newValue??.absoluteString,
                      RudderLinks.restorable(string, server: server) != nil else { return }
                UserDefaults.standard.set(string, forKey: key)
            }
            // Tell the page what is installed here once this launch has looked.
            Task { [weak self] in
                let status = await LocalAgents.shared.status()
                self?.reply("rudderLocalStatus", status)
            }
        }

        func detach() {
            urlObservation = nil
            if model.webView === webView { model.webView = nil }
        }

        // MARK: Messages from the page

        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            guard let body = message.body as? [String: Any], let type = body["type"] as? String else { return }
            // Only the app's own pages may drive the bridge.
            if let url = message.frameInfo.request.url, !RudderLinks.isInApp(url, server: model.session.server) { return }
            switch type {
            case "signIn", "signOut":
                model.signOut()
            case "openExternal":
                if let string = body["url"] as? String, let url = URL(string: string) {
                    openExternally(url)
                }
            case "badge":
                if let count = (body["count"] as? NSNumber)?.intValue {
                    model.setBadge(count)
                }
            case "notify":
                let title = body["title"] as? String ?? "Rudder"
                let text = body["body"] as? String ?? ""
                let path = (body["path"] as? String).flatMap { RudderLinks.pageURL(model.session.server, path: $0) != nil ? $0 : nil }
                Notifier.shared.post(title: title, body: text, path: path)
            case "localStatus":
                let id = body["id"] as? String
                Task { [weak self] in
                    var status = await LocalAgents.shared.status()
                    if let id { status["id"] = id }
                    self?.reply("rudderLocalStatus", status)
                }
            case "openLocal":
                let id = body["id"] as? String ?? ""
                let runId = body["runId"] as? String ?? ""
                let repo = body["repo"] as? String ?? ""
                Task { [weak self] in
                    guard let self else { return }
                    var result = await LocalAgents.shared.pull(runId: runId, repo: repo, session: self.model.session)
                    result["id"] = id
                    result["runId"] = runId
                    self.reply("rudderLocal", result)
                    if result["ok"] as? Bool == true, let path = result["path"] as? String {
                        LocalAgents.shared.offerOpen(path: path, title: result["title"] as? String, checkout: result["checkout"] as? String)
                    } else if result["cancelled"] as? Bool != true, let error = result["error"] as? String {
                        Notifier.shared.post(title: "Could not copy the agent to this Mac", body: error, path: nil)
                    }
                }
            case "openPath":
                guard let path = body["path"] as? String, let app = body["app"] as? String else { return }
                if app == "dashboard" {
                    Task { [weak self] in
                        let ok = await LocalAgents.shared.openDashboard(forPulled: path)
                        self?.reply("rudderOpenedPath", ["path": path, "app": app, "ok": ok])
                    }
                } else {
                    let ok = LocalAgents.shared.open(path: path, in: app)
                    reply("rudderOpenedPath", ["path": path, "app": app, "ok": ok])
                }
            default:
                break
            }
        }

        private func reply(_ function: String, _ payload: [String: Any]) {
            guard let script = Bridge.callbackScript(function, payload) else { return }
            webView?.evaluateJavaScript(script, completionHandler: nil)
        }

        // MARK: Navigation

        // Pages of the app stay in the window; everything else (previews,
        // GitHub, docs) opens in the default browser.
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
            if RudderLinks.isInApp(url, server: model.session.server) && navigationAction.targetFrame != nil {
                if url.path == "/app/login" {
                    // Sign in natively: Google refuses OAuth inside web views.
                    decisionHandler(.cancel)
                    model.signOut()
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

        // <input type=file>: the macOS open panel.
        func webView(
            _ webView: WKWebView,
            runOpenPanelWith parameters: WKOpenPanelParameters,
            initiatedByFrame frame: WKFrameInfo,
            completionHandler: @escaping @MainActor ([URL]?) -> Void
        ) {
            let panel = NSOpenPanel()
            panel.canChooseFiles = true
            panel.canChooseDirectories = parameters.allowsDirectories
            panel.allowsMultipleSelection = parameters.allowsMultipleSelection
            if let window = webView.window {
                panel.beginSheetModal(for: window) { response in
                    completionHandler(response == .OK ? panel.urls : nil)
                }
            } else {
                completionHandler(panel.runModal() == .OK ? panel.urls : nil)
            }
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
            // Cancelled loads, and the policy cancel of an external link.
            if nsError.code == NSURLErrorCancelled || nsError.domain == "WebKitErrorDomain" && nsError.code == 102 {
                return
            }
            let message = nsError.localizedDescription
                .replacingOccurrences(of: "&", with: "&amp;")
                .replacingOccurrences(of: "<", with: "&lt;")
            let html = """
            <html><head><meta name="viewport" content="width=device-width,initial-scale=1">
            <style>body{font:15px -apple-system;display:flex;height:100vh;margin:0;align-items:center;justify-content:center;
            text-align:center;padding:24px;box-sizing:border-box;color:#555;background:transparent}
            @media(prefers-color-scheme:dark){body{color:#aaa}}a{color:#4a7bd8;text-decoration:none;font-weight:600}</style></head>
            <body><div><p>Can't reach Rudder.</p><p style="font-size:13px">\(message)</p>
            <p><a href="\(model.session.appURL.absoluteString)">Try again</a></p></div></body></html>
            """
            webView.loadHTMLString(html, baseURL: model.session.server)
        }

        private func openExternally(_ url: URL) {
            guard let scheme = url.scheme?.lowercased(), ["http", "https", "mailto"].contains(scheme) else { return }
            NSWorkspace.shared.open(url)
        }
    }
}

extension Bundle {
    var shortVersion: String {
        (infoDictionary?["CFBundleShortVersionString"] as? String) ?? "1.0"
    }
}
