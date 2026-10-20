import AppKit
import SwiftUI

/// The shell's look: the web app's own palette (cloud/app/styles.css `--bg`,
/// `--surface`, `--accent`), so the native screens and the page read as one app.
enum Theme {
    static let canvas = Color("Canvas")
    static let accent = Color.accentColor
    static let surface = Color(nsColor: dynamic(light: 0xFFFFFF, dark: 0x1A1C20))
    static let accentInk = Color(nsColor: dynamic(light: 0xFFFFFF, dark: 0x0F1424))
    static let line = Color.primary.opacity(0.11)
    static let titlebarHeight: CGFloat = 28

    private static func dynamic(light: UInt32, dark: UInt32) -> NSColor {
        NSColor(name: nil) { appearance in
            let hex = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(
                srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
                green: CGFloat((hex >> 8) & 0xFF) / 255,
                blue: CGFloat(hex & 0xFF) / 255,
                alpha: 1
            )
        }
    }
}

// MARK: Window

/// Window behaviour SwiftUI's `Window` scene does not expose: one window (no
/// tabs), dragged by its background, and the app's color behind everything so
/// a resize never shows white. The frame is remembered by SwiftUI's own
/// autosave for the `main` scene.
struct WindowConfigurator: NSViewRepresentable {

    func makeNSView(context: Context) -> NSView { Probe() }
    func updateNSView(_ nsView: NSView, context: Context) {}

    private final class Probe: NSView {
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window, window.tabbingMode != .disallowed else { return }
            window.tabbingMode = .disallowed
            window.isMovableByWindowBackground = true
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.backgroundColor = NSColor(named: "Canvas")
        }
    }
}

// MARK: Backdrop

/// The signed-out backdrop: the app's background, a soft glow of the accent
/// behind the mark, and a dot grid that fades out toward the edges.
struct Backdrop: View {
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let dark = scheme == .dark
        ZStack {
            Theme.canvas
            RadialGradient(
                colors: [Theme.accent.opacity(dark ? 0.20 : 0.11), .clear],
                center: UnitPoint(x: 0.5, y: 0.30),
                startRadius: 0,
                endRadius: 460
            )
            Canvas { context, size in
                let step: CGFloat = 22
                let dot = Path(ellipseIn: CGRect(x: 0, y: 0, width: 1.6, height: 1.6))
                var y: CGFloat = step / 2
                while y < size.height {
                    var x: CGFloat = step / 2
                    while x < size.width {
                        context.fill(dot.offsetBy(dx: x, dy: y), with: .color(.primary))
                        x += step
                    }
                    y += step
                }
            }
            .opacity(dark ? 0.10 : 0.09)
            .mask(
                RadialGradient(
                    colors: [.white, .white.opacity(0.35), .clear],
                    center: UnitPoint(x: 0.5, y: 0.38),
                    startRadius: 40,
                    endRadius: 520
                )
            )
            LinearGradient(
                colors: [.clear, .black.opacity(dark ? 0.28 : 0.035)],
                startPoint: .center,
                endPoint: .bottom
            )
        }
        .ignoresSafeArea()
        .accessibilityHidden(true)
    }
}

/// The R mark on a soft shadow, like the app icon sitting on the page.
struct Mark: View {
    var size: CGFloat = 76

    var body: some View {
        Image("Logo")
            .resizable()
            .interpolation(.high)
            .aspectRatio(contentMode: .fit)
            .frame(width: size, height: size)
            .shadow(color: .black.opacity(0.10), radius: 1, y: 1)
            .shadow(color: .black.opacity(0.16), radius: size * 0.22, y: size * 0.12)
            .accessibilityHidden(true)
    }
}

// MARK: Buttons

/// Buttons with hover and pressed states: `primary` is the accent, `secondary`
/// a quiet surface with a hairline.
struct RudderButtonStyle: ButtonStyle {
    enum Kind { case primary, secondary }
    var kind: Kind = .primary
    var height: CGFloat = 38

    func makeBody(configuration: Configuration) -> some View {
        StyledBody(configuration: configuration, kind: kind, height: height)
    }

    private struct StyledBody: View {
        let configuration: Configuration
        let kind: Kind
        let height: CGFloat
        @Environment(\.isEnabled) private var isEnabled
        @Environment(\.colorScheme) private var scheme
        @State private var hovering = false

        var body: some View {
            let pressed = configuration.isPressed
            configuration.label
                .font(.system(size: 13.5, weight: .semibold))
                .foregroundStyle(kind == .primary ? Theme.accentInk : Color.primary)
                .frame(maxWidth: .infinity, minHeight: height)
                .padding(.horizontal, 14)
                .background {
                    let shape = RoundedRectangle(cornerRadius: 10, style: .continuous)
                    switch kind {
                    case .primary:
                        shape.fill(Theme.accent)
                            .overlay(shape.fill(LinearGradient(
                                colors: [.white.opacity(0.14), .clear],
                                startPoint: .top,
                                endPoint: .bottom
                            )))
                            .overlay(shape.fill(Color.black.opacity(pressed ? 0.14 : 0)))
                            .overlay(shape.fill(Color.white.opacity(hovering && !pressed ? 0.07 : 0)))
                            .shadow(color: Theme.accent.opacity(isEnabled ? (scheme == .dark ? 0.25 : 0.32) : 0), radius: hovering ? 10 : 6, y: 3)
                    case .secondary:
                        shape.fill(Theme.surface)
                            .overlay(shape.fill(Color.primary.opacity(pressed ? 0.08 : hovering ? 0.04 : 0)))
                            .overlay(shape.strokeBorder(Theme.line, lineWidth: 1))
                            .shadow(color: .black.opacity(scheme == .dark ? 0 : 0.04), radius: 2, y: 1)
                    }
                }
                .opacity(isEnabled ? 1 : 0.5)
                .scaleEffect(pressed ? 0.985 : 1)
                .animation(.easeOut(duration: 0.12), value: pressed)
                .animation(.easeOut(duration: 0.15), value: hovering)
                .contentShape(Rectangle())
                .onHover { hovering = $0 && isEnabled }
        }
    }
}

// MARK: Sign in

struct SignInView: View {
    @Bindable var session: Session
    @State private var code = ""
    @State private var appeared = false
    @State private var pending: Pending?
    @FocusState private var codeFocused: Bool

    private enum Pending { case google, github, pair }

    private var codeReady: Bool {
        code.trimmingCharacters(in: .whitespacesAndNewlines).count >= 8
    }

    var body: some View {
        ZStack {
            Backdrop()
            VStack(spacing: 0) {
                Spacer(minLength: 40)
                header
                    .modifier(Rise(on: appeared, delay: 0))
                actions
                    .frame(width: 300)
                    .padding(.top, 34)
                    .modifier(Rise(on: appeared, delay: 0.06))
                pairing
                    .frame(width: 300)
                    .padding(.top, 26)
                    .modifier(Rise(on: appeared, delay: 0.12))
                if let error = session.errorMessage {
                    Label(error, systemImage: "exclamationmark.circle.fill")
                        .font(.system(size: 12))
                        .foregroundStyle(.red)
                        .multilineTextAlignment(.center)
                        .frame(width: 300)
                        .padding(.top, 14)
                        .transition(.opacity.combined(with: .move(edge: .top)))
                }
                Spacer(minLength: 40)
                Text("Version \(Bundle.main.shortVersion)")
                    .font(.system(size: 11))
                    .foregroundStyle(.tertiary)
                    .padding(.bottom, 18)
                    .modifier(Rise(on: appeared, delay: 0.2))
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding(.horizontal, 24)
            .animation(.easeOut(duration: 0.2), value: session.errorMessage)
        }
        .onAppear {
            withAnimation(.spring(response: 0.6, dampingFraction: 0.86)) { appeared = true }
        }
        .onChange(of: session.signingIn) { _, signingIn in
            if !signingIn { pending = nil }
        }
    }

    private var header: some View {
        VStack(spacing: 0) {
            Mark(size: 76)
                .padding(.bottom, 24)
            Text("Rudder")
                .font(.system(size: 32, weight: .semibold))
                .tracking(-0.6)
            Text("Your agents, wherever you are.")
                .font(.system(size: 15))
                .foregroundStyle(.secondary)
                .padding(.top, 6)
        }
    }

    private var actions: some View {
        VStack(spacing: 10) {
            Button {
                pending = .google
                session.signIn(provider: "google")
            } label: {
                buttonLabel("Continue with Google", busy: pending == .google)
            }
            .buttonStyle(RudderButtonStyle(kind: .primary, height: 40))
            .keyboardShortcut(codeFocused ? nil : .defaultAction)

            Button {
                pending = .github
                session.signIn(provider: "github")
            } label: {
                buttonLabel("Continue with GitHub", busy: pending == .github, onAccent: false)
            }
            .buttonStyle(RudderButtonStyle(kind: .secondary, height: 40))
        }
        .disabled(session.signingIn)
    }

    private var pairing: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Rectangle().fill(Theme.line).frame(height: 1)
                Text("or pair this Mac")
                    .font(.system(size: 11.5, weight: .medium))
                    .foregroundStyle(.secondary)
                    .fixedSize()
                Rectangle().fill(Theme.line).frame(height: 1)
            }
            .padding(.bottom, 4)

            HStack(spacing: 8) {
                TextField("Pairing code", text: $code)
                    .textFieldStyle(.plain)
                    .font(code.isEmpty ? .system(size: 13.5) : .system(size: 14, weight: .medium, design: .monospaced))
                    .autocorrectionDisabled()
                    .focused($codeFocused)
                    .onSubmit(pair)
                    .padding(.horizontal, 11)
                    .frame(height: 34)
                    .background {
                        let shape = RoundedRectangle(cornerRadius: 9, style: .continuous)
                        shape.fill(Theme.surface)
                            .overlay(shape.strokeBorder(codeFocused ? Theme.accent.opacity(0.85) : Theme.line, lineWidth: codeFocused ? 1.5 : 1))
                            .shadow(color: Theme.accent.opacity(codeFocused ? 0.18 : 0), radius: 4)
                    }
                    .animation(.easeOut(duration: 0.15), value: codeFocused)

                Button(action: pair) {
                    buttonLabel("Pair", busy: pending == .pair, onAccent: codeReady)
                }
                .buttonStyle(RudderButtonStyle(kind: codeReady ? .primary : .secondary, height: 34))
                .frame(width: 72)
                .disabled(!codeReady)
                .keyboardShortcut(codeFocused ? .defaultAction : nil)
            }
            .disabled(session.signingIn)

            (Text("Run ") + Text("rudder cloud app").font(.system(size: 11, design: .monospaced)) + Text(" on another computer for a code."))
                .font(.system(size: 11.5))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: .infinity)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder
    private func buttonLabel(_ title: String, busy: Bool, onAccent: Bool = true) -> some View {
        ZStack {
            Text(title).opacity(busy ? 0 : 1)
            if busy {
                ProgressView()
                    .controlSize(.small)
                    .colorScheme(onAccent ? .dark : .light)
            }
        }
    }

    private func pair() {
        guard codeReady, !session.signingIn else { return }
        pending = .pair
        Task { await session.pair(code: code) }
    }
}

/// Fades and lifts a view in on first appearance, staggered by `delay`.
private struct Rise: ViewModifier {
    let on: Bool
    let delay: Double

    func body(content: Content) -> some View {
        content
            .opacity(on ? 1 : 0)
            .offset(y: on ? 0 : 10)
            .animation(.spring(response: 0.6, dampingFraction: 0.86).delay(delay), value: on)
    }
}

// MARK: Loading and retry

/// Shown until the web view's first page has drawn: the mark, breathing.
struct LoadingView: View {
    @State private var breathe = false
    @State private var showHint = false

    var body: some View {
        ZStack {
            Theme.canvas.ignoresSafeArea()
            VStack(spacing: 22) {
                Mark(size: 60)
                    .scaleEffect(breathe ? 1.0 : 0.94)
                    .opacity(breathe ? 1 : 0.72)
                    .animation(.easeInOut(duration: 1.1).repeatForever(autoreverses: true), value: breathe)
                LoadingBar()
                    .frame(width: 96, height: 3)
                Text("Opening Rudder")
                    .font(.system(size: 12.5))
                    .foregroundStyle(.secondary)
                    .opacity(showHint ? 1 : 0)
                    .animation(.easeIn(duration: 0.4), value: showHint)
            }
            .offset(y: -Theme.titlebarHeight / 2)
        }
        .onAppear { breathe = true }
        .task {
            // Only say something when it is taking a while.
            try? await Task.sleep(for: .seconds(1.2))
            showHint = true
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Opening Rudder")
    }
}

/// A thin indeterminate bar: a short accent segment gliding across a track.
private struct LoadingBar: View {
    var body: some View {
        TimelineView(.animation) { timeline in
            let t = timeline.date.timeIntervalSinceReferenceDate
            let phase = (t.truncatingRemainder(dividingBy: 1.4)) / 1.4
            GeometryReader { proxy in
                let width = proxy.size.width
                let segment = width * 0.38
                let eased = 0.5 - cos(phase * .pi) / 2
                Capsule().fill(Theme.line)
                    .overlay(alignment: .leading) {
                        Capsule()
                            .fill(Theme.accent)
                            .frame(width: segment)
                            .offset(x: -segment + (width + segment) * eased)
                    }
                    .clipShape(Capsule())
            }
        }
    }
}

/// A load that failed: calm, with one obvious way forward.
struct FailedView: View {
    let message: String
    let retry: () -> Void
    @State private var appeared = false

    var body: some View {
        ZStack {
            Theme.canvas.ignoresSafeArea()
            VStack(spacing: 0) {
                ZStack {
                    Circle().fill(Theme.surface)
                        .overlay(Circle().strokeBorder(Theme.line, lineWidth: 1))
                        .shadow(color: .black.opacity(0.06), radius: 10, y: 4)
                    Image(systemName: "wifi.exclamationmark")
                        .font(.system(size: 24, weight: .medium))
                        .foregroundStyle(.secondary)
                }
                .frame(width: 64, height: 64)
                .padding(.bottom, 20)
                Text("Can't reach Rudder")
                    .font(.system(size: 19, weight: .semibold))
                Text(message)
                    .font(.system(size: 13))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 320)
                    .padding(.top, 6)
                Button(action: retry) {
                    Text("Try Again")
                }
                .buttonStyle(RudderButtonStyle(kind: .primary, height: 34))
                .frame(width: 140)
                .keyboardShortcut(.defaultAction)
                .padding(.top, 22)
            }
            .padding(24)
            .offset(y: -Theme.titlebarHeight / 2)
            .opacity(appeared ? 1 : 0)
            .offset(y: appeared ? 0 : 8)
        }
        .onAppear {
            withAnimation(.spring(response: 0.5, dampingFraction: 0.9)) { appeared = true }
        }
    }
}
