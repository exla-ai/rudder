import AppKit
import SwiftUI

/// Rudder > About Rudder: the mark, the name, the version, what
/// Rudder is, the website and the copyright, in the app's own look instead of
/// the stock panel. One window; About again brings it forward.
@MainActor
enum AboutWindow {
    private static var window: NSWindow?

    static func show() {
        let window = self.window ?? make()
        self.window = window
        if !window.isVisible {
            // SwiftUI sizes the window to its content on layout: center it at that size.
            window.layoutIfNeeded()
            window.center()
        }
        NSApp.activate()
        window.makeKeyAndOrderFront(nil)
    }

    private static func make() -> NSWindow {
        let window = AboutPanel(
            contentRect: NSRect(origin: .zero, size: AboutView.size),
            styleMask: [.titled, .closable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        // Sized by its SwiftUI content, plus the titlebar the backdrop runs under.
        window.contentView = NSHostingView(rootView: AboutView())
        window.title = "About Rudder"
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.isMovableByWindowBackground = true
        window.isReleasedWhenClosed = false
        window.isRestorable = false
        window.isExcludedFromWindowsMenu = true
        window.tabbingMode = .disallowed
        window.collectionBehavior = [.fullScreenNone]
        window.backgroundColor = NSColor(named: "Canvas")
        // Only the close button, like the system's About panels.
        window.standardWindowButton(.miniaturizeButton)?.isHidden = true
        window.standardWindowButton(.zoomButton)?.isHidden = true
        return window
    }
}

/// Escape closes it too.
private final class AboutPanel: NSWindow {
    override func cancelOperation(_ sender: Any?) {
        close()
    }
}

struct AboutView: View {
    /// Below the titlebar.
    static let size = CGSize(width: 300, height: 330)
    private static let site = URL(string: "https://rudder.exla.ai")!

    private let version = AboutView.info("CFBundleShortVersionString")
    private let copyright = AboutView.info("NSHumanReadableCopyright")
    @State private var copied = false
    @State private var copyReset: Task<Void, Never>?

    var body: some View {
        VStack(spacing: 0) {
            Mark(size: 92)
            Text("Rudder")
                .font(.system(size: 25, weight: .semibold))
                .tracking(-0.5)
                .padding(.top, 20)
            Text("Your coding agents, wherever you are.")
                .font(.system(size: 13))
                .foregroundStyle(.secondary)
                .padding(.top, 5)
            versionButton
                .padding(.top, 16)
            Spacer(minLength: 0)
            Link(destination: Self.site) {
                HStack(spacing: 3) {
                    Text("rudder.exla.ai")
                    Image(systemName: "arrow.up.right")
                        .font(.system(size: 9.5, weight: .semibold))
                }
            }
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(Theme.accent)
            .pointingHandCursor()
            Text(copyright)
                .font(.system(size: 11))
                .foregroundStyle(.tertiary)
                .padding(.top, 8)
        }
        .padding(.top, 16)
        .padding(.bottom, 22)
        .frame(width: Self.size.width, height: Self.size.height)
        .background {
            Backdrop(focus: UnitPoint(x: 0.5, y: 0.26), reach: 0.42)
                .ignoresSafeArea()
        }
    }

    /// The version; a click copies it for a bug report.
    private var versionButton: some View {
        Button(action: copyVersion) {
            ZStack {
                Text("Version \(version)")
                    .opacity(copied ? 0 : 1)
                Label("Copied", systemImage: "checkmark")
                    .opacity(copied ? 1 : 0)
            }
            .font(.system(size: 11.5, weight: .medium).monospacedDigit())
            .foregroundStyle(.secondary)
            .padding(.horizontal, 11)
            .frame(height: 23)
            .background {
                Capsule().fill(Theme.surface)
                    .overlay(Capsule().strokeBorder(Theme.line, lineWidth: 1))
            }
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .help("Copy version")
        .accessibilityLabel(copied ? "Copied" : "Version \(version). Copy")
    }

    private func copyVersion() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString("Rudder \(version)", forType: .string)
        withAnimation(.easeOut(duration: 0.15)) { copied = true }
        copyReset?.cancel()
        copyReset = Task {
            try? await Task.sleep(for: .seconds(1.5))
            guard !Task.isCancelled else { return }
            withAnimation(.easeOut(duration: 0.25)) { copied = false }
        }
    }

    private static func info(_ key: String) -> String {
        guard let value = Bundle.main.object(forInfoDictionaryKey: key) as? String else {
            preconditionFailure("Info.plist has no \(key)")
        }
        return value
    }
}

private extension View {
    func pointingHandCursor() -> some View {
        onHover { inside in
            (inside ? NSCursor.pointingHand : NSCursor.arrow).set()
        }
    }
}
