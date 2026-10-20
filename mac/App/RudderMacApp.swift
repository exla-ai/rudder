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
                .frame(minWidth: 640, minHeight: 480)
                .onOpenURL { url in model.session.handle(url: url) }
        }
        .defaultSize(width: 1180, height: 820)
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

/// Signed out: the native sign-in screen. Signed in: the Rudder app.
struct RootView: View {
    @Bindable var model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Group {
            if let token = model.session.token {
                // A new token is a new page: RUDDER_NATIVE is fixed per document.
                AppWebView(model: model, token: token)
                    .id(token)
            } else {
                SignInView(session: model.session)
            }
        }
        .background(Color("AccentColor"))
        .onAppear {
            model.openMainWindow = { openWindow(id: RudderMacApp.mainWindowID) }
        }
        .onChange(of: model.session.token) { _, token in
            if token != nil { model.pollNow() }
        }
    }
}

struct SignInView: View {
    @Bindable var session: Session
    @State private var code = ""

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            Image("Logo")
                .resizable()
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                .frame(width: 72, height: 72)
                .accessibilityHidden(true)
                .padding(.bottom, 22)
            Text("Rudder")
                .font(.system(size: 30, weight: .semibold))
            Text("Your agents, wherever you are.")
                .font(.system(size: 15))
                .foregroundStyle(.secondary)
                .padding(.top, 6)

            VStack(spacing: 10) {
                Button {
                    session.signIn(provider: "google")
                } label: {
                    Text("Continue with Google")
                        .font(.system(size: 14, weight: .semibold))
                        .frame(maxWidth: .infinity, minHeight: 30)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)

                Button("Use GitHub instead") {
                    session.signIn(provider: "github")
                }
                .buttonStyle(.link)

                HStack(spacing: 8) {
                    TextField("Pairing code", text: $code)
                        .textFieldStyle(.roundedBorder)
                        .font(.system(size: 14, design: .monospaced))
                        .onSubmit { Task { await session.pair(code: code) } }
                    Button("Pair") {
                        Task { await session.pair(code: code) }
                    }
                    .disabled(code.trimmingCharacters(in: .whitespaces).count < 8)
                }
                .padding(.top, 14)
                Text("Or a code from `rudder cloud app` on another computer.")
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)

                if let error = session.errorMessage {
                    Text(error)
                        .font(.system(size: 12))
                        .foregroundStyle(.red)
                        .multilineTextAlignment(.center)
                }
            }
            .frame(width: 300)
            .padding(.top, 30)
            .disabled(session.signingIn)
            Spacer()
            if session.signingIn {
                ProgressView().controlSize(.small).padding(.bottom, 20)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(24)
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
