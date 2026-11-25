import XCTest

/// Drives the real app on a simulator through ios/UITests/steps.json and
/// leaves a screenshot and the accessibility tree for every "shot" step in
/// the test runner's Documents/qa (ios/scripts/qa.sh copies it out).
///
/// Steps, one object each:
///   {"waitFor": "Skip", "timeout": 60}   wait for an element whose label holds the text
///   {"tap": "Settings"}                  tap the first such element
///   {"tapAt": [0.5, 0.9]}                tap a point, as fractions of the screen
///   {"type": "some text"}                type into the focused field
///   {"swipe": "up"}                      swipe the screen up, down, left or right
///   {"wait": 2}                          seconds
///   {"shot": "home"}                     NN-home.png and NN-home.txt
/// A step that finds nothing is written to log.txt and the run goes on, so
/// one renamed button costs one screen, not the whole pass. The test fails at
/// the end when any step missed.
@MainActor
final class ClickThrough: XCTestCase {
    func testClickThrough() throws {
        let env = ProcessInfo.processInfo.environment
        let code = try XCTUnwrap(env["QA_PAIR_CODE"], "QA_PAIR_CODE is not set")
        let server = try XCTUnwrap(env["QA_SERVER"], "QA_SERVER is not set")
        let stepsURL = try XCTUnwrap(Bundle(for: ClickThrough.self).url(forResource: "steps", withExtension: "json"))
        let steps = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: stepsURL)) as? [[String: Any]])

        let out = URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Documents/qa")
        try? FileManager.default.removeItem(at: out)
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        var log: [String] = []
        var misses = 0
        var shots = 0

        continueAfterFailure = true
        let app = XCUIApplication()
        // iOS asks about notifications over the first screen: answer it, or no tap lands.
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        app.launchArguments = ["-pairCode", code, "-RudderServerURL", server]
        app.launch()

        func find(_ text: String, timeout: TimeInterval) -> XCUIElement? {
            let exact = app.descendants(matching: .any).matching(NSPredicate(format: "label ==[c] %@", text)).firstMatch
            if exact.waitForExistence(timeout: min(timeout, 2)) { return exact }
            let holds = app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS[c] %@", text)).firstMatch
            return holds.waitForExistence(timeout: timeout) ? holds : nil
        }

        for (index, step) in steps.enumerated() {
            let name = "step \(index + 1) \(step)"
            let refuse = springboard.buttons["Don\u{2019}t Allow"]
            if refuse.exists { refuse.tap() }
            if let text = step["waitFor"] as? String {
                let timeout = (step["timeout"] as? NSNumber)?.doubleValue ?? 20
                if find(text, timeout: timeout) == nil { misses += 1; log.append("MISS \(name)") } else { log.append("ok \(name)") }
            } else if let text = step["tap"] as? String {
                if let element = find(text, timeout: 8) {
                    // Read the label first: the tap can take the element off the screen.
                    log.append("ok \(name) -> \"\(element.label)\"")
                    if element.isHittable { element.tap() } else { element.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap() }
                } else { misses += 1; log.append("MISS \(name)") }
            } else if let point = step["tapAt"] as? [NSNumber], point.count == 2 {
                app.coordinate(withNormalizedOffset: CGVector(dx: point[0].doubleValue, dy: point[1].doubleValue)).tap()
                log.append("ok \(name)")
            } else if let text = step["type"] as? String {
                app.typeText(text)
                log.append("ok \(name)")
            } else if let direction = step["swipe"] as? String {
                switch direction {
                case "up": app.swipeUp()
                case "down": app.swipeDown()
                case "left": app.swipeLeft()
                default: app.swipeRight()
                }
                log.append("ok \(name)")
            } else if let seconds = (step["wait"] as? NSNumber)?.doubleValue {
                Thread.sleep(forTimeInterval: seconds)
            } else if let label = step["shot"] as? String {
                shots += 1
                let base = String(format: "%02d-%@", shots, label)
                try XCUIScreen.main.screenshot().pngRepresentation.write(to: out.appendingPathComponent("\(base).png"))
                try app.debugDescription.write(to: out.appendingPathComponent("\(base).txt"), atomically: true, encoding: .utf8)
                log.append("shot \(base)")
            } else {
                XCTFail("unknown step \(step)")
            }
            try log.joined(separator: "\n").write(to: out.appendingPathComponent("log.txt"), atomically: true, encoding: .utf8)
        }
        XCTAssertEqual(misses, 0, "\(misses) steps found nothing; see log.txt")
    }
}
