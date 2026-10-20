import AppKit
import Foundation

/// What a process printed, and how it ended.
struct ProcessOutput {
    var status: Int32
    var stdout: String
    var stderr: String
    var timedOut: Bool
}

/// The user's own tools on this Mac: whether `rudder`, `claude` and `codex`
/// are installed, and `rudder cloud pull`, which brings a cloud agent here.
///
/// GUI apps start with launchd's bare PATH, so tools installed by Homebrew,
/// npm or nvm are found through the PATH a login shell prints (once per launch),
/// plus the usual install directories. Arguments always go to the process as
/// an argv array, never through a shell.
@MainActor
final class LocalAgents {
    static let shared = LocalAgents()
    static let foldersKey = "localRepoFolders"
    static let pulledKey = "pulledWorkspacePaths"
    static let installedKey = "localRudderInstalled"
    static let checkoutsKey = "pulledWorkspaceCheckouts"

    private var searchPATH: String?
    private var inFlight: Set<String> = []
    /// Workspaces this app pulled. The page may only ask to open these.
    private(set) var pulledPaths: [String]
    /// The checkout each pulled workspace came from (where `rudder` lists it).
    private var pulledCheckouts: [String: String]

    init() {
        pulledPaths = UserDefaults.standard.stringArray(forKey: Self.pulledKey) ?? []
        pulledCheckouts = UserDefaults.standard.dictionary(forKey: Self.checkoutsKey) as? [String: String] ?? [:]
    }

    /// Whether `rudder` was installed the last time we looked (for
    /// RUDDER_NATIVE.local before the first check of this launch finishes).
    var lastKnownInstalled: Bool {
        UserDefaults.standard.bool(forKey: Self.installedKey)
    }

    // MARK: Status

    /// `{rudder, claude, codex, rudderVersion?}` for `window.rudderLocalStatus`.
    func status() async -> [String: Any] {
        let rudder = await which("rudder")
        let claude = await which("claude")
        let codex = await which("codex")
        UserDefaults.standard.set(rudder != nil, forKey: Self.installedKey)
        var out: [String: Any] = ["rudder": rudder != nil, "claude": claude != nil, "codex": codex != nil]
        if let rudder {
            let version = await Self.run(rudder, ["--version"], cwd: nil, env: await environment(), timeout: 20)
            let line = CLIOutput.stripANSI(version.stdout).split(whereSeparator: \.isNewline).last
                .map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
            if version.status == 0, !line.isEmpty, line.count < 40 { out["rudderVersion"] = line }
        }
        return out
    }

    // MARK: Pull

    /// Brings agent `runId` of `repo` (owner/name) to this Mac. The reply for
    /// `window.rudderLocal`: `{ok, path?, title?, checkout?, error?, cancelled?}`.
    func pull(runId: String, repo: String, session: Session) async -> [String: Any] {
        guard Validate.runId(runId) else { return ["ok": false, "error": "That agent id is not valid."] }
        guard Validate.repo(repo) else { return ["ok": false, "error": "That repository name is not valid."] }
        guard session.token != nil else { return ["ok": false, "error": "Sign in first."] }
        guard !inFlight.contains(runId) else { return ["ok": false, "error": "This agent is already on its way here."] }
        inFlight.insert(runId)
        defer { inFlight.remove(runId) }

        guard let rudder = await which("rudder") else {
            return ["ok": false, "error": "The rudder CLI is not installed on this Mac. Install it with: npm install -g @exla/rudder"]
        }
        guard let checkout = await checkout(for: repo) else {
            return ["ok": false, "cancelled": true, "error": "No local checkout of \(repo) was chosen."]
        }
        var env = await environment()
        // The CLI acts as this app's account, even if `rudder login` was never run here.
        if let token = session.token { env["RUDDER_CLOUD_TOKEN"] = token }
        env["RUDDER_CLOUD_URL"] = session.server.absoluteString
        // The CLI retries a waking machine once by itself; three minutes covers that.
        let out = await Self.run(rudder, ["cloud", "pull", runId, "--json"], cwd: checkout, env: env, timeout: 180)
        if out.status == 0, let result = CLIOutput.pullResult(stdout: out.stdout) {
            remember(pulled: result.path, checkout: checkout.path)
            var reply: [String: Any] = ["ok": true, "path": result.path, "checkout": checkout.path]
            if let title = result.title { reply["title"] = title }
            return reply
        }
        let error = out.timedOut
            ? "rudder cloud pull took longer than 3 minutes and was stopped. The cloud agent is still running."
            : CLIOutput.errorMessage(stdout: out.stdout, stderr: out.stderr, status: out.status)
        return ["ok": false, "error": error]
    }

    private func remember(pulled path: String, checkout: String) {
        pulledPaths.removeAll { $0 == path }
        pulledPaths.insert(path, at: 0)
        pulledPaths = Array(pulledPaths.prefix(50))
        pulledCheckouts[path] = checkout
        pulledCheckouts = pulledCheckouts.filter { pulledPaths.contains($0.key) }
        UserDefaults.standard.set(pulledPaths, forKey: Self.pulledKey)
        UserDefaults.standard.set(pulledCheckouts, forKey: Self.checkoutsKey)
    }

    // MARK: Checkouts

    /// The local checkout of `repo`: remembered, or asked for once.
    func checkout(for repo: String) async -> URL? {
        let key = repo.lowercased()
        var folders = UserDefaults.standard.dictionary(forKey: Self.foldersKey) as? [String: String] ?? [:]
        if let saved = folders[key], Self.isCheckout(saved) {
            return URL(fileURLWithPath: saved, isDirectory: true)
        }
        while let chosen = chooseFolder(repo: repo) {
            guard Self.isCheckout(chosen.path) else {
                let again = ask(
                    "That folder is not a git checkout",
                    "Choose the folder you cloned \(repo) into (the one with a .git folder).",
                    buttons: ["Choose Another", "Cancel"]
                )
                if again == 0 { continue }
                return nil
            }
            if let git = await which("git") {
                let remote = await Self.run(git, ["-C", chosen.path, "remote", "get-url", "origin"], cwd: nil, env: await environment(), timeout: 15)
                let origin = remote.stdout.trimmingCharacters(in: .whitespacesAndNewlines)
                if remote.status == 0, !origin.isEmpty, !Validate.remote(origin, matches: repo) {
                    // `rudder cloud pull` needs a clone of the project's own repo.
                    let choice = ask(
                        "This checkout is not \(repo)",
                        "Its origin is \(origin). Choose a clone of \(repo).",
                        buttons: ["Choose Another", "Cancel"]
                    )
                    if choice == 0 { continue }
                    return nil
                }
            }
            folders[key] = chosen.path
            UserDefaults.standard.set(folders, forKey: Self.foldersKey)
            return chosen
        }
        return nil
    }

    /// Forget a remembered checkout (Settings, or a folder that moved).
    func forgetCheckout(for repo: String) {
        var folders = UserDefaults.standard.dictionary(forKey: Self.foldersKey) as? [String: String] ?? [:]
        folders.removeValue(forKey: repo.lowercased())
        UserDefaults.standard.set(folders, forKey: Self.foldersKey)
    }

    private func chooseFolder(repo: String) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.canCreateDirectories = false
        panel.prompt = "Use This Checkout"
        panel.message = "Where is your local clone of \(repo)? The agent's workspace is created next to it, the same way Rudder does on this Mac."
        let code = URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("code", isDirectory: true)
        if FileManager.default.fileExists(atPath: code.path) { panel.directoryURL = code }
        NSApp.activate()
        return panel.runModal() == .OK ? panel.url : nil
    }

    static func isCheckout(_ path: String) -> Bool {
        let fm = FileManager.default
        return fm.fileExists(atPath: (path as NSString).appendingPathComponent(".git"))
            || fm.fileExists(atPath: (path as NSString).appendingPathComponent(".jj"))
    }

    // MARK: Opening a pulled workspace

    /// Offers the dashboard, the workspace in Terminal, Finder and (when there
    /// is a project) Xcode for a copy that just arrived.
    func offerOpen(path: String, title: String?, checkout: String?) {
        let hasXcode = Self.xcodeDocument(at: path) != nil
        var buttons = ["Open Rudder in Terminal", "Open Workspace in Terminal", "Open in Finder"]
        if hasXcode { buttons.append("Open in Xcode") }
        buttons.append("Done")
        var detail = "This is a copy: the cloud agent keeps running. The copy's workspace is \((path as NSString).abbreviatingWithTildeInPath)"
        if let checkout {
            detail += ", and it is paused in the Rudder dashboard of \((checkout as NSString).abbreviatingWithTildeInPath), ready to continue."
        } else {
            detail += "."
        }
        let choice = ask("A copy of \(title ?? "the agent") is on this Mac", detail, buttons: buttons)
        switch buttons[safe: choice] ?? "" {
        case "Open Rudder in Terminal":
            Task { _ = await openDashboard(forPulled: path) }
        case "Open Workspace in Terminal": _ = open(path: path, in: "terminal")
        case "Open in Finder": _ = open(path: path, in: "finder")
        case "Open in Xcode": _ = open(path: path, in: "xcode")
        default: break
        }
    }

    /// Opens a pulled workspace in Finder, Terminal or Xcode. Paths this app
    /// did not pull are refused.
    func open(path: String, in app: String) -> Bool {
        guard pulledPaths.contains(path), FileManager.default.fileExists(atPath: path) else { return false }
        let url = URL(fileURLWithPath: path, isDirectory: true)
        let workspace = NSWorkspace.shared
        switch app {
        case "finder":
            return workspace.open(url)
        case "terminal":
            guard let terminal = workspace.urlForApplication(withBundleIdentifier: "com.apple.Terminal") else { return false }
            workspace.open([url], withApplicationAt: terminal, configuration: NSWorkspace.OpenConfiguration(), completionHandler: nil)
            return true
        case "xcode":
            guard let document = Self.xcodeDocument(at: path) else { return false }
            if let xcode = workspace.urlForApplication(withBundleIdentifier: "com.apple.dt.Xcode") {
                workspace.open([document], withApplicationAt: xcode, configuration: NSWorkspace.OpenConfiguration(), completionHandler: nil)
                return true
            }
            return workspace.open(document)
        default:
            return false
        }
    }

    /// A Terminal window running `rudder` in the checkout a pulled copy came
    /// from, where the copy is listed as paused. Done with a `.command` file
    /// (Terminal runs those), so the app needs no Apple Events permission.
    func openDashboard(forPulled path: String) async -> Bool {
        guard pulledPaths.contains(path), let checkout = pulledCheckouts[path],
              FileManager.default.fileExists(atPath: checkout),
              let rudder = await which("rudder"),
              let terminal = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.apple.Terminal") else { return false }
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = caches.appendingPathComponent("ai.exla.rudder.mac", isDirectory: true)
        let script = dir.appendingPathComponent("rudder-dashboard.command")
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try LocalTools.dashboardScript(checkout: checkout, rudder: rudder).write(to: script, atomically: true, encoding: .utf8)
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: script.path)
        } catch {
            return false
        }
        NSWorkspace.shared.open([script], withApplicationAt: terminal, configuration: NSWorkspace.OpenConfiguration(), completionHandler: nil)
        return true
    }

    static func xcodeDocument(at path: String) -> URL? {
        let names = (try? FileManager.default.contentsOfDirectory(atPath: path)) ?? []
        return LocalTools.xcodeDocument(in: names).map { URL(fileURLWithPath: path).appendingPathComponent($0) }
    }

    /// A modal alert; returns the index of the button pressed.
    private func ask(_ title: String, _ detail: String, buttons: [String]) -> Int {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = detail
        for button in buttons { alert.addButton(withTitle: button) }
        NSApp.activate()
        let response = alert.runModal()
        return response.rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
    }

    // MARK: Finding tools

    func which(_ tool: String) async -> String? {
        let path = await resolvedPATH()
        for dir in path.split(separator: ":") {
            let candidate = (String(dir) as NSString).appendingPathComponent(tool)
            if FileManager.default.isExecutableFile(atPath: candidate) { return candidate }
        }
        return nil
    }

    /// This app's environment with the login shell's PATH, and no CLI self-update
    /// in the middle of a pull.
    func environment() async -> [String: String] {
        var env = ProcessInfo.processInfo.environment
        env["PATH"] = await resolvedPATH()
        env["RUDDER_DISABLE_AUTO_UPDATE"] = "1"
        env["NO_COLOR"] = "1"
        env["TERM"] = "dumb"
        return env
    }

    func resolvedPATH() async -> String {
        if let searchPATH { return searchPATH }
        let home = NSHomeDirectory()
        let shell = Self.loginShell()
        let script = "printf '%s%s%s' '\(LocalTools.pathStart)' \"$PATH\" '\(LocalTools.pathEnd)'"
        // Interactive as well as login: nvm and friends add to PATH in .zshrc.
        let out = await Self.run(
            shell, ["-l", "-i", "-c", script],
            cwd: URL(fileURLWithPath: home),
            env: ["HOME": home, "USER": NSUserName(), "LOGNAME": NSUserName(), "SHELL": shell, "TERM": "dumb"],
            timeout: 10
        )
        let path = LocalTools.searchPATH(shellPATH: LocalTools.shellPATH(fromOutput: out.stdout), home: home)
        searchPATH = path
        return path
    }

    nonisolated static func loginShell() -> String {
        if let entry = getpwuid(getuid()), let raw = entry.pointee.pw_shell {
            let shell = String(cString: raw)
            if FileManager.default.isExecutableFile(atPath: shell) { return shell }
        }
        return "/bin/zsh"
    }

    // MARK: Running processes

    nonisolated static func run(
        _ executable: String, _ arguments: [String], cwd: URL?, env: [String: String], timeout: TimeInterval
    ) async -> ProcessOutput {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: runBlocking(executable, arguments, cwd: cwd, env: env, timeout: timeout))
            }
        }
    }

    nonisolated private static func runBlocking(
        _ executable: String, _ arguments: [String], cwd: URL?, env: [String: String], timeout: TimeInterval
    ) -> ProcessOutput {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.environment = env
        if let cwd { process.currentDirectoryURL = cwd }
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr
        process.standardInput = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            return ProcessOutput(status: -1, stdout: "", stderr: error.localizedDescription, timedOut: false)
        }

        // Drain both pipes while the process runs, or a chatty child blocks on a full pipe.
        let collected = Collected()
        let group = DispatchGroup()
        for (pipe, isOut) in [(stdout, true), (stderr, false)] {
            group.enter()
            DispatchQueue.global(qos: .utility).async {
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                collected.set(data, out: isOut)
                group.leave()
            }
        }
        let watchdog = DispatchWorkItem {
            if process.isRunning {
                collected.markTimedOut()
                process.terminate()
            }
        }
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + timeout, execute: watchdog)
        process.waitUntilExit()
        watchdog.cancel()
        // A grandchild still holding the pipe must not hang us.
        _ = group.wait(timeout: .now() + 5)
        let result = collected.snapshot()
        return ProcessOutput(
            status: process.terminationStatus,
            stdout: String(decoding: result.out, as: UTF8.self),
            stderr: String(decoding: result.err, as: UTF8.self),
            timedOut: result.timedOut
        )
    }
}

/// Pipe output gathered from two reader threads.
private final class Collected: @unchecked Sendable {
    private let lock = NSLock()
    private var out = Data()
    private var err = Data()
    private var timedOut = false

    func set(_ data: Data, out isOut: Bool) {
        lock.lock(); defer { lock.unlock() }
        if isOut { out = data } else { err = data }
    }

    func markTimedOut() {
        lock.lock(); defer { lock.unlock() }
        timedOut = true
    }

    func snapshot() -> (out: Data, err: Data, timedOut: Bool) {
        lock.lock(); defer { lock.unlock() }
        return (out, err, timedOut)
    }
}

extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
