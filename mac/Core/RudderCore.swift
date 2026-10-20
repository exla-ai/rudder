import Foundation

// The Mac app's pure logic: no AppKit, no WebKit, no I/O. Compiled into both
// the app and the RudderCoreTests bundle, so everything here is unit tested
// without launching the app.

/// URLs on the control plane, and which of them belong inside the app window.
enum RudderLinks {
    static let defaultServer = URL(string: "https://rudder-cloud-control.fly.dev")!

    /// The control plane: an http(s) override (RUDDER_CLOUD_URL or the
    /// RudderServerURL default), reduced to its origin, else the default.
    static func server(override: String?) -> URL {
        guard let raw = override?.trimmingCharacters(in: .whitespacesAndNewlines), !raw.isEmpty,
              var components = URLComponents(string: raw),
              let scheme = components.scheme?.lowercased(), scheme == "https" || scheme == "http",
              let host = components.host, !host.isEmpty else {
            return defaultServer
        }
        components.scheme = scheme
        components.path = ""
        components.query = nil
        components.fragment = nil
        components.user = nil
        components.password = nil
        return components.url ?? defaultServer
    }

    static func appURL(_ server: URL) -> URL {
        server.appendingPathComponent("app")
    }

    /// Google (or GitHub) sign-in that ends at rudder://auth?token=.
    static func loginURL(_ server: URL, provider: String = "google") -> URL {
        var components = URLComponents(url: server.appendingPathComponent("app/login"), resolvingAgainstBaseURL: false)!
        components.queryItems = [
            URLQueryItem(name: "native", value: "1"),
            URLQueryItem(name: "provider", value: provider == "github" ? "github" : "google"),
        ]
        return components.url!
    }

    /// The state the menu bar polls. `passive=1` asks the control plane not to
    /// count a background poll as someone using the machine (see README).
    static func stateURL(_ server: URL) -> URL {
        var components = URLComponents(url: server.appendingPathComponent("api/app/state"), resolvingAgainstBaseURL: false)!
        components.queryItems = [URLQueryItem(name: "passive", value: "1")]
        return components.url!
    }

    static func pairURL(_ server: URL) -> URL {
        server.appendingPathComponent("api/app/pair/finish")
    }

    /// A page of the app from a path like `/app/p/<ws>/a/<run>`. Anything that
    /// is not under /app (a full url, `//host`, `/api`) is refused.
    static func pageURL(_ server: URL, path: String) -> URL? {
        guard path == "/app" || path.hasPrefix("/app/") || path.hasPrefix("/app?") || path.hasPrefix("/app#"),
              !path.contains("\\"), !path.contains("..") else { return nil }
        guard let url = URL(string: path, relativeTo: server)?.absoluteURL, sameOrigin(url, server) else { return nil }
        return url
    }

    static func sameOrigin(_ a: URL, _ b: URL) -> Bool {
        a.scheme?.lowercased() == b.scheme?.lowercased()
            && a.host?.lowercased() == b.host?.lowercased()
            && effectivePort(a) == effectivePort(b)
    }

    private static func effectivePort(_ url: URL) -> Int? {
        if let port = url.port { return port }
        switch url.scheme?.lowercased() {
        case "https": return 443
        case "http": return 80
        default: return nil
        }
    }

    /// Navigations that stay in the window: same origin, and the app or its API.
    static func isInApp(_ url: URL, server: URL) -> Bool {
        guard sameOrigin(url, server) else { return false }
        let path = url.path
        return path == "/app" || path.hasPrefix("/app/") || path.hasPrefix("/api/")
    }

    /// The last page, if it is still one to reopen on launch.
    static func restorable(_ string: String?, server: URL) -> URL? {
        guard let string, let url = URL(string: string), isInApp(url, server: server) else { return nil }
        let path = url.path
        guard path == "/app" || path.hasPrefix("/app/"), path != "/app/login", !path.hasPrefix("/app/native-done") else {
            return nil
        }
        return url
    }

    /// `encodeURIComponent`, so paths match the web app's own links.
    static func encodeComponent(_ value: String) -> String {
        // ASCII only: CharacterSet.alphanumerics would let non-ASCII letters through.
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.!~*'()")
        return value.addingPercentEncoding(withAllowedCharacters: allowed) ?? value
    }

    static func agentPath(workspaceId: String, runId: String) -> String {
        "/app/p/\(encodeComponent(workspaceId))/a/\(encodeComponent(runId))"
    }
}

/// rudder:// links: the sign-in callback and laptop pairing codes.
enum AuthCallback {
    /// rudder://auth?token=rdr_... -> the bearer token.
    static func token(from url: URL) -> String? {
        guard url.scheme?.lowercased() == "rudder", url.host?.lowercased() == "auth",
              let value = query(url, "token") else { return nil }
        let token = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard token.range(of: "^rdr_[A-Za-z0-9._~+/=-]{8,512}$", options: .regularExpression) != nil else { return nil }
        return token
    }

    /// rudder://pair?code=ABCD-EFGH -> the code.
    static func pairCode(from url: URL) -> String? {
        guard url.scheme?.lowercased() == "rudder", url.host?.lowercased() == "pair",
              let code = query(url, "code")?.trimmingCharacters(in: .whitespacesAndNewlines),
              (4...32).contains(code.count),
              code.range(of: "^[A-Za-z0-9-]+$", options: .regularExpression) != nil else { return nil }
        return code
    }

    private static func query(_ url: URL, _ name: String) -> String? {
        URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first(where: { $0.name == name })?.value
    }
}

/// One agent the menu bar lists.
struct NeedsYouItem: Equatable {
    var workspaceId: String
    var runId: String
    var title: String
    var project: String
    var reason: String
    var updatedAt: Double

    var path: String { RudderLinks.agentPath(workspaceId: workspaceId, runId: runId) }
}

/// "Needs you", counted exactly as the web app counts it (app.js `needsYou`):
/// a pending ask, or status waiting / review / failed.
enum NeedsYou {
    static let statuses: Set<String> = ["waiting", "review", "failed"]

    static func isNeedsYou(_ agent: [String: Any]) -> Bool {
        if let asks = agent["asks"] as? [Any], asks.contains(where: { $0 is [String: Any] }) {
            return true
        }
        return statuses.contains(agent["status"] as? String ?? "")
    }

    /// Newest first. nil when the body is not the state JSON.
    static func items(fromStateJSON data: Data) -> [NeedsYouItem]? {
        guard let root = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return nil }
        let projects = root["projects"] as? [[String: Any]] ?? []
        var out: [NeedsYouItem] = []
        for project in projects {
            guard let workspaceId = project["workspaceId"] as? String else { continue }
            let projectName = (project["name"] as? String) ?? (project["repo"] as? String) ?? "Project"
            for agent in project["agents"] as? [[String: Any]] ?? [] {
                guard let runId = agent["id"] as? String, agent["pending"] as? Bool != true, isNeedsYou(agent) else { continue }
                let title = nonEmpty(agent["title"]) ?? nonEmpty(agent["task"]) ?? "Untitled agent"
                out.append(NeedsYouItem(
                    workspaceId: workspaceId,
                    runId: runId,
                    title: title,
                    project: projectName,
                    reason: reason(agent),
                    updatedAt: timestamp(agent["updatedAt"])
                ))
            }
        }
        return out.sorted { $0.updatedAt > $1.updatedAt }
    }

    static func count(fromStateJSON data: Data) -> Int? {
        items(fromStateJSON: data)?.count
    }

    static func reason(_ agent: [String: Any]) -> String {
        if let asks = agent["asks"] as? [[String: Any]], let ask = asks.first {
            switch ask["kind"] as? String {
            case "question": return "has a question"
            case "plan": return "has a plan for you"
            default: return "needs approval"
            }
        }
        switch agent["status"] as? String {
        case "waiting": return "waiting for you"
        case "review": return "ready for review"
        case "failed": return "failed"
        default: return "needs you"
        }
    }

    /// Milliseconds since 1970 from a number or an ISO 8601 string.
    static func timestamp(_ value: Any?) -> Double {
        if let number = value as? Double { return number }
        if let number = value as? Int { return Double(number) }
        if let string = value as? String {
            let formatter = ISO8601DateFormatter()
            formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            if let date = formatter.date(from: string) { return date.timeIntervalSince1970 * 1000 }
            formatter.formatOptions = [.withInternetDateTime]
            if let date = formatter.date(from: string) { return date.timeIntervalSince1970 * 1000 }
        }
        return 0
    }

    private static func nonEmpty(_ value: Any?) -> String? {
        guard let string = (value as? String)?.trimmingCharacters(in: .whitespacesAndNewlines), !string.isEmpty else { return nil }
        return string
    }
}

/// What `rudder cloud pull <runId> --json` printed.
struct PullResult: Equatable {
    var path: String
    var runId: String?
    var title: String?
}

enum CLIOutput {
    /// The last stdout line that is a JSON object with a `path`. Progress
    /// lines before it (or ANSI color) are ignored.
    static func pullResult(stdout: String) -> PullResult? {
        for object in jsonObjects(stripANSI(stdout)).reversed() {
            if let path = object["path"] as? String, path.hasPrefix("/") {
                return PullResult(path: path, runId: object["runId"] as? String, title: object["title"] as? String)
            }
        }
        return nil
    }

    /// Why a CLI run failed, in one line: a JSON `error`, else the last
    /// stderr line, else the exit status.
    static func errorMessage(stdout: String, stderr: String, status: Int32) -> String {
        for object in jsonObjects(stripANSI(stdout)).reversed() {
            if let error = object["error"] as? String, !error.isEmpty { return error }
        }
        let lines = stripANSI(stderr).split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
        if var last = lines.last {
            if last.hasPrefix("rudder: ") { last.removeFirst("rudder: ".count) }
            return String(last.prefix(300))
        }
        return "rudder exited with status \(status)"
    }

    static func stripANSI(_ text: String) -> String {
        text.replacingOccurrences(of: "\u{1B}\\[[0-9;?]*[ -/]*[@-~]", with: "", options: .regularExpression)
    }

    /// Every line (or the whole text) that parses as a JSON object, in order.
    static func jsonObjects(_ text: String) -> [[String: Any]] {
        var out: [[String: Any]] = []
        let whole = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if whole.hasPrefix("{"), let object = parse(whole) {
            return [object]
        }
        for line in text.split(whereSeparator: \.isNewline) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("{"), let object = parse(trimmed) { out.append(object) }
        }
        return out
    }

    private static func parse(_ text: String) -> [String: Any]? {
        (try? JSONSerialization.jsonObject(with: Data(text.utf8))) as? [String: Any]
    }
}

/// Inputs from the page are checked before they reach a process or a path.
enum Validate {
    static func runId(_ value: String) -> Bool {
        value.range(of: "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$", options: .regularExpression) != nil
    }

    /// owner/name, as GitHub allows them.
    static func repo(_ value: String) -> Bool {
        value.range(of: "^[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9._-]{1,100}$", options: .regularExpression) != nil
            && !value.hasSuffix("/.") && !value.hasSuffix("/..")
    }

    /// owner/name from a git remote (https, ssh, scp-style), lowercased.
    static func repoSlug(fromRemote remote: String) -> String? {
        var text = remote.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        if text.hasSuffix("/") { text.removeLast() }
        if text.hasSuffix(".git") { text.removeLast(4) }
        var path: String
        if text.contains("://") {
            guard let url = URL(string: text) else { return nil }
            path = url.path
        } else if let colon = text.firstIndex(of: ":"), !text[..<colon].contains("/") {
            path = String(text[text.index(after: colon)...])   // git@github.com:owner/name
        } else {
            return nil
        }
        let parts = path.split(separator: "/").map(String.init)
        guard parts.count >= 2 else { return nil }
        return "\(parts[parts.count - 2])/\(parts[parts.count - 1])".lowercased()
    }

    static func remote(_ remote: String, matches repo: String) -> Bool {
        repoSlug(fromRemote: remote) == repo.lowercased()
    }
}

/// What the page sees as `window.RUDDER_NATIVE`, and how the app answers it.
enum Bridge {
    static func nativePayload(token: String, version: String, local: Bool) -> [String: Any] {
        [
            "platform": "mac",
            "token": token,
            "version": version,
            // The page may call `notify` and the app posts a macOS notification.
            "notifications": true,
            // The `rudder` CLI is installed here, so agents can come to this Mac.
            "local": local,
            // This build answers `openLocal`, `localStatus` and `openPath`.
            "openLocal": true,
            // <input type=file> opens the macOS open panel.
            "attachments": true,
        ]
    }

    static func bootstrapScript(token: String, version: String, local: Bool) -> String {
        let payload = nativePayload(token: token, version: version, local: local)
        return "window.RUDDER_NATIVE = Object.freeze(\(json(payload)));"
    }

    /// `window.rudderX && window.rudderX({...})`, or nil for a name that is
    /// not one of ours (the name is spliced into script text).
    static func callbackScript(_ function: String, _ payload: [String: Any]) -> String? {
        guard function.range(of: "^rudder[A-Z][A-Za-z]{0,40}$", options: .regularExpression) != nil,
              JSONSerialization.isValidJSONObject(payload) else { return nil }
        return "window.\(function) && window.\(function)(\(json(payload)))"
    }

    static func json(_ object: [String: Any]) -> String {
        guard JSONSerialization.isValidJSONObject(object),
              let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              let text = String(data: data, encoding: .utf8) else { return "{}" }
        // U+2028/2029 end a line inside a JS string literal in old engines.
        return text.replacingOccurrences(of: "\u{2028}", with: "\\u2028").replacingOccurrences(of: "\u{2029}", with: "\\u2029")
    }
}

/// Small pieces of running the user's own tools.
enum LocalTools {
    static let pathStart = "__RUDDER_PATH_START__"
    static let pathEnd = "__RUDDER_PATH_END__"

    /// The PATH a login shell printed between the markers, ignoring whatever
    /// the user's shell startup files print around it.
    static func shellPATH(fromOutput output: String) -> String? {
        guard let start = output.range(of: pathStart, options: .backwards),
              let end = output.range(of: pathEnd, range: start.upperBound..<output.endIndex) else { return nil }
        let path = output[start.upperBound..<end.lowerBound].trimmingCharacters(in: .whitespacesAndNewlines)
        return path.isEmpty ? nil : path
    }

    /// Where GUI apps rarely look but CLIs usually live, appended after the
    /// login shell's PATH (or used alone when the shell gave none).
    static func searchPATH(shellPATH: String?, home: String) -> String {
        var dirs = (shellPATH ?? "").split(separator: ":").map(String.init).filter { !$0.isEmpty }
        let extra = [
            "/opt/homebrew/bin", "/usr/local/bin", "\(home)/.local/bin", "\(home)/.npm-global/bin",
            "\(home)/.bun/bin", "\(home)/.volta/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin",
        ]
        for dir in extra where !dirs.contains(dir) { dirs.append(dir) }
        return dirs.joined(separator: ":")
    }

    /// Single-quoted for sh/zsh: safe for any path.
    static func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// The `.command` file Terminal runs to open the dashboard in a checkout.
    /// A login shell, so `node` (which `rudder` runs on) is on PATH.
    static func dashboardScript(checkout: String, rudder: String) -> String {
        "#!/bin/zsh -l\ncd \(shellQuote(checkout)) || exit 1\nexec \(shellQuote(rudder))\n"
    }

    /// The Xcode document to open in a folder listing: a workspace first,
    /// else a project. nil when there is neither.
    static func xcodeDocument(in names: [String]) -> String? {
        let visible = names.filter { !$0.hasPrefix(".") }.sorted()
        return visible.first(where: { $0.hasSuffix(".xcworkspace") }) ?? visible.first(where: { $0.hasSuffix(".xcodeproj") })
    }
}
