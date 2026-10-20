import XCTest

// RudderCore.swift is compiled into this bundle directly, so these run with
// no app host: `xcodebuild test -scheme RudderCoreTests`.
final class RudderCoreTests: XCTestCase {
    let server = URL(string: "https://rudder.example.com")!

    // MARK: URLs

    func testServerOverride() {
        XCTAssertEqual(RudderLinks.server(override: nil), RudderLinks.defaultServer)
        XCTAssertEqual(RudderLinks.server(override: "  "), RudderLinks.defaultServer)
        XCTAssertEqual(RudderLinks.server(override: "ftp://x.com"), RudderLinks.defaultServer)
        XCTAssertEqual(RudderLinks.server(override: "not a url"), RudderLinks.defaultServer)
        XCTAssertEqual(RudderLinks.server(override: "https://a.example.com/app?x=1").absoluteString, "https://a.example.com")
        XCTAssertEqual(RudderLinks.server(override: "HTTP://localhost:8787/").absoluteString, "http://localhost:8787")
    }

    func testAppAndLoginURLs() {
        XCTAssertEqual(RudderLinks.appURL(server).absoluteString, "https://rudder.example.com/app")
        XCTAssertEqual(RudderLinks.loginURL(server).absoluteString, "https://rudder.example.com/app/login?native=1&provider=google")
        XCTAssertEqual(RudderLinks.loginURL(server, provider: "github").absoluteString,
                       "https://rudder.example.com/app/login?native=1&provider=github")
        XCTAssertEqual(RudderLinks.loginURL(server, provider: "evil").absoluteString,
                       "https://rudder.example.com/app/login?native=1&provider=google")
        XCTAssertEqual(RudderLinks.stateURL(server).absoluteString, "https://rudder.example.com/api/app/state?passive=1")
    }

    func testPageURL() {
        XCTAssertEqual(RudderLinks.pageURL(server, path: "/app/settings")?.absoluteString, "https://rudder.example.com/app/settings")
        XCTAssertEqual(RudderLinks.pageURL(server, path: "/app")?.absoluteString, "https://rudder.example.com/app")
        XCTAssertNil(RudderLinks.pageURL(server, path: "//evil.com/app"))
        XCTAssertNil(RudderLinks.pageURL(server, path: "https://evil.com/app"))
        XCTAssertNil(RudderLinks.pageURL(server, path: "/api/app/state"))
        XCTAssertNil(RudderLinks.pageURL(server, path: "/application"))
        XCTAssertNil(RudderLinks.pageURL(server, path: "/app/../admin"))
    }

    func testInAppAndRestorable() {
        XCTAssertTrue(RudderLinks.isInApp(URL(string: "https://rudder.example.com/app/p/1")!, server: server))
        XCTAssertTrue(RudderLinks.isInApp(URL(string: "https://rudder.example.com:443/api/app/state")!, server: server))
        XCTAssertFalse(RudderLinks.isInApp(URL(string: "https://rudder.example.com/")!, server: server))
        XCTAssertFalse(RudderLinks.isInApp(URL(string: "http://rudder.example.com/app")!, server: server))
        XCTAssertFalse(RudderLinks.isInApp(URL(string: "https://github.com/app")!, server: server))
        XCTAssertEqual(RudderLinks.restorable("https://rudder.example.com/app/digest", server: server)?.path, "/app/digest")
        XCTAssertNil(RudderLinks.restorable("https://rudder.example.com/app/login", server: server))
        XCTAssertNil(RudderLinks.restorable("https://rudder.example.com/api/app/state", server: server))
        XCTAssertNil(RudderLinks.restorable("https://other.com/app", server: server))
        XCTAssertNil(RudderLinks.restorable(nil, server: server))
    }

    func testAgentPathEncodesLikeEncodeURIComponent() {
        XCTAssertEqual(RudderLinks.agentPath(workspaceId: "ws 1", runId: "a/b"), "/app/p/ws%201/a/a%2Fb")
        XCTAssertEqual(RudderLinks.encodeComponent("é!*"), "%C3%A9!*")
    }

    // MARK: rudder:// links

    func testTokenFromCallback() {
        XCTAssertEqual(AuthCallback.token(from: URL(string: "rudder://auth?token=rdr_abcDEF123-_x")!), "rdr_abcDEF123-_x")
        XCTAssertNil(AuthCallback.token(from: URL(string: "rudder://auth?token=abc12345678")!))
        XCTAssertNil(AuthCallback.token(from: URL(string: "rudder://pair?token=rdr_abcDEF123")!))
        XCTAssertNil(AuthCallback.token(from: URL(string: "https://auth?token=rdr_abcDEF123")!))
        XCTAssertNil(AuthCallback.token(from: URL(string: "rudder://auth?token=rdr_short")!))
        XCTAssertNil(AuthCallback.token(from: URL(string: "rudder://auth")!))
        XCTAssertNil(AuthCallback.token(from: URL(string: "rudder://auth?token=rdr_abc%20def12345")!))
    }

    func testPairCode() {
        XCTAssertEqual(AuthCallback.pairCode(from: URL(string: "rudder://pair?code=ABCD-EFGH")!), "ABCD-EFGH")
        XCTAssertNil(AuthCallback.pairCode(from: URL(string: "rudder://pair?code=%3Cscript%3E")!))
        XCTAssertNil(AuthCallback.pairCode(from: URL(string: "rudder://auth?code=ABCD-EFGH")!))
    }

    // MARK: Needs you

    func testNeedsYouCountsLikeTheWebApp() throws {
        let state = """
        {"projects":[
          {"workspaceId":"w1","name":"site","repo":"me/site","agents":[
            {"id":"r1","title":"Fix nav","status":"running","asks":[{"id":"q","kind":"permission"}],"updatedAt":3000},
            {"id":"r2","title":"Docs","status":"waiting","updatedAt":"1970-01-01T00:00:05Z"},
            {"id":"r3","status":"review","task":"Ship it","updatedAt":1000},
            {"id":"r4","status":"failed","updatedAt":500},
            {"id":"r5","status":"running","asks":[]},
            {"id":"r6","status":"done"}
          ]},
          {"workspaceId":"w2","repo":"me/api","agents":[{"id":"r7","status":"working","updatedAt":100,"asks":[{"id":"z","kind":"question"}]}]},
          {"name":"no id","agents":[{"id":"r8","status":"failed"}]}
        ]}
        """
        let items = try XCTUnwrap(NeedsYou.items(fromStateJSON: Data(state.utf8)))
        XCTAssertEqual(items.map(\.runId), ["r2", "r1", "r3", "r4", "r7"])
        XCTAssertEqual(NeedsYou.count(fromStateJSON: Data(state.utf8)), 5)
        let first = try XCTUnwrap(items.first(where: { $0.runId == "r1" }))
        XCTAssertEqual(first.reason, "needs approval")
        XCTAssertEqual(first.project, "site")
        XCTAssertEqual(first.path, "/app/p/w1/a/r1")
        XCTAssertEqual(items.first(where: { $0.runId == "r3" })?.title, "Ship it")
        XCTAssertEqual(items.first(where: { $0.runId == "r4" })?.title, "Untitled agent")
        XCTAssertEqual(items.first(where: { $0.runId == "r7" })?.project, "me/api")
        XCTAssertEqual(items.first(where: { $0.runId == "r7" })?.reason, "has a question")
    }

    func testNeedsYouBadBodies() {
        XCTAssertNil(NeedsYou.items(fromStateJSON: Data("not json".utf8)))
        XCTAssertNil(NeedsYou.items(fromStateJSON: Data("[1,2]".utf8)))
        XCTAssertEqual(NeedsYou.count(fromStateJSON: Data("{}".utf8)), 0)
    }

    // MARK: CLI output

    func testPullResultFromJSONLine() {
        let out = "\u{1B}[2mFetching bundle...\u{1B}[0m\n{\"path\":\"/Users/me/.rudder-workspaces/x/fix-nav\",\"runId\":\"r1\",\"title\":\"Fix nav\"}\n"
        XCTAssertEqual(CLIOutput.pullResult(stdout: out),
                       PullResult(path: "/Users/me/.rudder-workspaces/x/fix-nav", runId: "r1", title: "Fix nav"))
    }

    func testPullResultFromPrettyJSON() {
        let out = "{\n  \"path\": \"/tmp/w\",\n  \"runId\": \"r2\"\n}\n"
        XCTAssertEqual(CLIOutput.pullResult(stdout: out), PullResult(path: "/tmp/w", runId: "r2", title: nil))
    }

    func testPullResultRejectsRelativeOrMissing() {
        XCTAssertNil(CLIOutput.pullResult(stdout: "{\"path\":\"relative/dir\"}"))
        XCTAssertNil(CLIOutput.pullResult(stdout: "done"))
        XCTAssertNil(CLIOutput.pullResult(stdout: ""))
    }

    func testErrorMessage() {
        XCTAssertEqual(CLIOutput.errorMessage(stdout: "{\"error\":\"no such run\"}", stderr: "x", status: 1), "no such run")
        XCTAssertEqual(CLIOutput.errorMessage(stdout: "", stderr: "warn\nrudder: Not logged in to Rudder Cloud.\n\n", status: 1),
                       "Not logged in to Rudder Cloud.")
        XCTAssertEqual(CLIOutput.errorMessage(stdout: "", stderr: "", status: 3), "rudder exited with status 3")
    }

    // MARK: Validation

    func testRunIdAndRepo() {
        XCTAssertTrue(Validate.runId("20261020-abc.def_1"))
        XCTAssertFalse(Validate.runId("-rf"))
        XCTAssertFalse(Validate.runId("a b"))
        XCTAssertFalse(Validate.runId("../x"))
        XCTAssertFalse(Validate.runId(""))
        XCTAssertTrue(Validate.repo("exla-ai/rudder"))
        XCTAssertFalse(Validate.repo("exla-ai/rudder/x"))
        XCTAssertFalse(Validate.repo("exla-ai/.."))
        XCTAssertFalse(Validate.repo("rudder"))
    }

    func testRemoteMatching() {
        XCTAssertTrue(Validate.remote("git@github.com:Exla-AI/rudder.git", matches: "exla-ai/rudder"))
        XCTAssertTrue(Validate.remote("https://github.com/exla-ai/rudder", matches: "exla-ai/rudder"))
        XCTAssertTrue(Validate.remote("https://github.com/exla-ai/rudder.git/", matches: "exla-ai/rudder"))
        XCTAssertTrue(Validate.remote("ssh://git@github.com/exla-ai/rudder.git", matches: "exla-ai/rudder"))
        XCTAssertFalse(Validate.remote("git@github.com:viraatdas/rudder.git", matches: "exla-ai/rudder"))
        XCTAssertFalse(Validate.remote("", matches: "exla-ai/rudder"))
        XCTAssertNil(Validate.repoSlug(fromRemote: "/local/path/repo"))
    }

    // MARK: Bridge

    func testBootstrapScript() throws {
        let script = Bridge.bootstrapScript(token: "rdr_x\"y", version: "1.0.0", local: false)
        XCTAssertTrue(script.hasPrefix("window.RUDDER_NATIVE = Object.freeze({"))
        XCTAssertTrue(script.hasSuffix("});"))
        let json = String(script.dropFirst("window.RUDDER_NATIVE = Object.freeze(".count).dropLast(2))
        let object = try XCTUnwrap(try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        XCTAssertEqual(object["platform"] as? String, "mac")
        XCTAssertEqual(object["token"] as? String, "rdr_x\"y")
        XCTAssertEqual(object["notifications"] as? Bool, true)
        XCTAssertEqual(object["local"] as? Bool, false)
        XCTAssertEqual(object["openLocal"] as? Bool, true)
    }

    func testCallbackScript() {
        XCTAssertEqual(Bridge.callbackScript("rudderLocal", ["id": "1", "ok": true]),
                       "window.rudderLocal && window.rudderLocal({\"id\":\"1\",\"ok\":true})")
        XCTAssertNil(Bridge.callbackScript("alert", ["a": 1]))
        XCTAssertNil(Bridge.callbackScript("rudderX;alert(1)", ["a": 1]))
        XCTAssertEqual(Bridge.json(["t": "a\u{2028}b"]), "{\"t\":\"a\\u2028b\"}")
    }

    // MARK: Local tools

    func testShellPATH() {
        let noisy = "Last login\n__RUDDER_PATH_START__/opt/homebrew/bin:/usr/bin__RUDDER_PATH_END__\nbye"
        XCTAssertEqual(LocalTools.shellPATH(fromOutput: noisy), "/opt/homebrew/bin:/usr/bin")
        XCTAssertNil(LocalTools.shellPATH(fromOutput: "nothing"))
        XCTAssertNil(LocalTools.shellPATH(fromOutput: "__RUDDER_PATH_START____RUDDER_PATH_END__"))
        let search = LocalTools.searchPATH(shellPATH: "/a:/usr/bin", home: "/Users/me")
        XCTAssertTrue(search.hasPrefix("/a:/usr/bin:/opt/homebrew/bin"))
        XCTAssertEqual(search.components(separatedBy: ":").filter { $0 == "/usr/bin" }.count, 1)
        XCTAssertTrue(LocalTools.searchPATH(shellPATH: nil, home: "/h").contains("/h/.local/bin"))
    }

    func testDashboardScript() {
        XCTAssertEqual(LocalTools.shellQuote("/Users/me/it's here"), "'/Users/me/it'\\''s here'")
        XCTAssertEqual(LocalTools.dashboardScript(checkout: "/Users/me/code/site", rudder: "/opt/homebrew/bin/rudder"),
                       "#!/bin/zsh -l\ncd '/Users/me/code/site' || exit 1\nexec '/opt/homebrew/bin/rudder'\n")
        XCTAssertEqual(LocalTools.dashboardScript(checkout: "/a/$(x)", rudder: "/r"), "#!/bin/zsh -l\ncd '/a/$(x)' || exit 1\nexec '/r'\n")
    }

    func testXcodeDocument() {
        XCTAssertEqual(LocalTools.xcodeDocument(in: ["App.xcodeproj", "App.xcworkspace", "README.md"]), "App.xcworkspace")
        XCTAssertEqual(LocalTools.xcodeDocument(in: ["B.xcodeproj", "A.xcodeproj"]), "A.xcodeproj")
        XCTAssertNil(LocalTools.xcodeDocument(in: ["package.json", ".hidden.xcodeproj"]))
    }
}
