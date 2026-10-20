import AppKit
import SwiftUI

/// Rudder for macOS: the Rudder app (`/app`) in a window, a menu bar item
/// listing the agents that need you, notifications, and pulling a cloud agent
/// onto this Mac with the local `rudder` CLI.
@main
struct RudderMacApp: App {
    static let mainWindowID = "main"

    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model = AppModel.shared

    var body: some Scene {
        Window("Rudder", id: Self.mainWindowID) {
            RootView(model: model)
                .frame(minWidth: 720, minHeight: 520)
                .onOpenURL { url in model.session.handle(url: url) }
        }
        .windowStyle(.hiddenTitleBar)
        .windowResizability(.contentMinSize)
        .defaultSize(width: 1180, height: 820)
        .defaultPosition(.center)
        .commands {
            CommandGroup(replacing: .appSettings) {
                Button("Settings…") { model.openSettings() }
                    .keyboardShortcut(",", modifiers: .command)
                    .disabled(model.session.token == nil)
                Button("Sign Out") { model.signOut() }
                    .disabled(model.session.token == nil)
            }
            // One window: no File > New Window.
            CommandGroup(replacing: .newItem) {}
            CommandGroup(before: .toolbar) {
                Button("Reload") { model.reload() }
                    .keyboardShortcut("r", modifiers: .command)
                Button("Back") { model.goBack() }
                    .keyboardShortcut("[", modifiers: .command)
                Button("Forward") { model.goForward() }
                    .keyboardShortcut("]", modifiers: .command)
                Divider()
            }
        }

        MenuBarExtra {
            MenuBarContent(model: model)
        } label: {
            MenuBarLabel(model: model)
        }
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        #if DEBUG
        // RUDDER_APPEARANCE=light|dark pins the appearance for screenshots.
        switch ProcessInfo.processInfo.environment["RUDDER_APPEARANCE"] {
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        default: break
        }
        #endif
        // Before launch finishes, so a click on a notification that launched the app is delivered.
        Notifier.shared.install()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        AppModel.shared.startPolling()
    }

    /// Closing the window keeps the menu bar item and its notifications.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { AppModel.shared.showMainWindow() }
        return true
    }
}

/// Signed out: the native sign-in screen. Signed in: the Rudder app, behind a
/// native loading screen until its first page has drawn.
struct RootView: View {
    @Bindable var model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        ZStack {
            Theme.canvas.ignoresSafeArea()
            if let token = model.session.token {
                SignedInView(model: model, token: token)
                    .transition(.opacity)
            } else {
                signedOut
                    .transition(.opacity)
            }
        }
        .animation(.easeInOut(duration: 0.35), value: model.session.token == nil)
        .background(WindowConfigurator())
        .onAppear {
            model.openMainWindow = { openWindow(id: RudderMacApp.mainWindowID) }
        }
        .onChange(of: model.session.token) { _, token in
            if token != nil { model.pollNow() }
        }
    }

    @ViewBuilder private var signedOut: some View {
        #if DEBUG
        // Screenshots of the loading and retry screens without signing in:
        // RUDDER_PREVIEW_STATE=loading|failed (Debug builds only).
        switch ProcessInfo.processInfo.environment["RUDDER_PREVIEW_STATE"] {
        case "loading": LoadingView()
        case "failed": FailedView(message: "The Internet connection appears to be offline.", retry: {})
        case "web": SignedInView(model: model, token: "rdr_preview")
        default: SignInView(session: model.session)
        }
        #else
        SignInView(session: model.session)
        #endif
    }
}

/// The web app under a titlebar strip of its own background, with the loading
/// and retry screens crossfaded over it.
struct SignedInView: View {
    @Bindable var model: AppModel
    let token: String

    var body: some View {
        ZStack {
            VStack(spacing: 0) {
                // The traffic lights sit here, over the app's own color, and the
                // strip drags the window; the page starts below it.
                Theme.canvas.frame(height: Theme.titlebarHeight)
                // A new token is a new page: RUDDER_NATIVE is fixed per document.
                AppWebView(model: model, token: token)
                    .id(token)
                    .opacity(model.webState == .ready ? 1 : 0)
            }
            .ignoresSafeArea()

            switch model.webState {
            case .loading:
                LoadingView().transition(.opacity)
            case .failed(let message):
                FailedView(message: message) { model.retryLoad() }
                    .transition(.opacity)
            case .ready:
                EmptyView()
            }
        }
        .animation(.easeInOut(duration: 0.4), value: model.webState)
    }
}

/// The menu bar item: the mark, and how many agents need you.
struct MenuBarLabel: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let count = model.needs.count
        Group {
            if count > 0 {
                Text("\(Image(systemName: "steeringwheel")) \(count)")
            } else {
                Image(systemName: "steeringwheel")
            }
        }
        .accessibilityLabel(count == 1 ? "Rudder, 1 agent needs you" : "Rudder, \(count) agents need you")
        .onAppear {
            if model.openMainWindow == nil {
                model.openMainWindow = { openWindow(id: RudderMacApp.mainWindowID) }
            }
        }
    }
}

struct MenuBarContent: View {
    let model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        if model.session.token == nil {
            Button("Sign In…") { show() }
        } else if model.needs.isEmpty {
            Text(model.lastPollFailed ? "Can't reach Rudder" : "Nothing needs you")
        } else {
            Text(model.needs.count == 1 ? "1 agent needs you" : "\(model.needs.count) agents need you")
            ForEach(Array(model.needs.prefix(12).enumerated()), id: \.offset) { _, item in
                Button("\(item.title) · \(item.reason) · \(item.project)") {
                    model.open(path: item.path)
                }
            }
        }
        Divider()
        Button("Open Rudder") { show() }
            .keyboardShortcut("o")
        Button("Quit Rudder") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }

    private func show() {
        model.openMainWindow = { openWindow(id: RudderMacApp.mainWindowID) }
        model.showMainWindow()
    }
}
