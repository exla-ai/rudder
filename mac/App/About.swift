import AppKit
import SwiftUI

/// Rudder > About Rudder: the helm, the name, the version, what Rudder is, the
/// website and the copyright, on the chart paper of the Rudder.dmg window. One
/// window; About again brings it forward.
@MainActor
enum AboutWindow {
    private static var window: NSWindow?

    static func show() {
        let window = self.window ?? make()
        self.window = window
        if !window.isVisible {
            // A new view each time it opens, so the helm turns in again.
            window.contentView = NSHostingView(rootView: AboutView())
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
    /// Below the titlebar, which the chart runs under.
    static let size = CGSize(width: 300, height: 332)
    private static let site = URL(string: "https://rudder.exla.ai")!

    private let version = AboutView.info("CFBundleShortVersionString")
    private let copyright = AboutView.info("NSHumanReadableCopyright")

    var body: some View {
        VStack(spacing: 0) {
            Helm(size: 84)
                .padding(.top, 30)
            Text("Rudder")
                .font(.system(size: 28, weight: .semibold))
                .tracking(-0.6)
                .padding(.top, 28)
            Text("Your coding agents, wherever you are.")
                .font(.system(size: 13))
                .foregroundStyle(.secondary)
                .padding(.top, 4)
            VersionTag(version: version)
                .padding(.top, 20)
            Spacer(minLength: 0)
            Link(destination: Self.site) {
                HStack(spacing: 3) {
                    Text("rudder.exla.ai")
                    Image(systemName: "arrow.up.right")
                        .font(.system(size: 9.5, weight: .semibold))
                }
            }
            .font(.system(size: 12.5, weight: .medium))
            .foregroundStyle(Theme.accent)
            .pointingHandCursor()
            Text(copyright)
                .font(.system(size: 11))
                .foregroundStyle(.tertiary)
                .padding(.top, 6)
        }
        .padding(.bottom, 20)
        .frame(width: Self.size.width, height: Self.size.height)
        .background {
            Chart()
        }
    }

    private static func info(_ key: String) -> String {
        guard let value = Bundle.main.object(forInfoDictionaryKey: key) as? String else {
            preconditionFailure("Info.plist has no \(key)")
        }
        return value
    }
}

// MARK: Chart

/// The chart paper of the Rudder.dmg window (mac/dmg/background.py): the app's
/// background with depth contours that show toward the edges, clear behind the
/// helm and the words, and a little shade at the foot.
private struct Chart: View {
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let dark = scheme == .dark
        ZStack {
            Theme.canvas
            Canvas { context, size in
                let spacing: CGFloat = 25
                for i in 0...Int(size.height / spacing) + 1 {
                    let k = Double(i)
                    let base = 6 + CGFloat(i) * spacing
                    var line = Path()
                    var x: CGFloat = -8
                    line.move(to: CGPoint(x: x, y: base + Self.swell(x, k)))
                    while x < size.width + 8 {
                        x += 4
                        line.addLine(to: CGPoint(x: x, y: base + Self.swell(x, k)))
                    }
                    context.stroke(line, with: .color(.primary), lineWidth: 1)
                }
            }
            .opacity(dark ? 0.11 : 0.09)
            .mask(EllipticalGradient(colors: [.clear, .white], startRadiusFraction: 0.3, endRadiusFraction: 0.7))
            LinearGradient(
                colors: [.clear, .black.opacity(dark ? 0.24 : 0.03)],
                startPoint: .center,
                endPoint: .bottom
            )
        }
        .ignoresSafeArea()
        .accessibilityHidden(true)
    }

    /// How far a contour strays from its line at `x`: the DMG's three swells, scaled to a small window.
    private static func swell(_ x: CGFloat, _ k: Double) -> CGFloat {
        7 * sin(x / 72 + k * 0.73) + 3.5 * sin(x / 33 + k * 1.91) + 1.8 * sin(x / 16 + k * 2.7)
    }
}

// MARK: Helm

/// The app icon drawn live so its wheel can turn: it swings into place as the
/// window opens with a wake rippling out from it, and gives a little under the
/// pointer. Three still rings around it echo the DMG's. Reduce Motion keeps it still.
private struct Helm: View {
    let size: CGFloat
    @Environment(\.colorScheme) private var scheme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// False until the opening turn starts.
    @State private var turned = false
    @State private var hovering = false
    @State private var wakes: [Int] = []
    @State private var nextWake = 0

    private var angle: Angle {
        if reduceMotion { return .zero }
        return .degrees(!turned ? -75 : hovering ? 22.5 : 0)
    }

    var body: some View {
        let plate = RoundedRectangle(cornerRadius: size * 0.225, style: .continuous)
        ZStack {
            plate.fill(Color(red: 0x11 / 255, green: 0x15 / 255, blue: 0x1B / 255))
                .overlay(plate.strokeBorder(.white.opacity(scheme == .dark ? 0.10 : 0), lineWidth: 1))
                .shadow(color: .black.opacity(0.10), radius: 1, y: 1)
                .shadow(color: .black.opacity(0.16), radius: size * 0.22, y: size * 0.12)
            Wheel()
                .rotationEffect(angle)
                .animation(.spring(response: 1.15, dampingFraction: 0.58), value: turned)
                .animation(.spring(response: 0.6, dampingFraction: 0.55), value: hovering)
        }
        .frame(width: size, height: size)
        .background { water }
        .contentShape(plate)
        .onHover { inside in
            hovering = inside
            if inside { wake() }
        }
        .task {
            guard !reduceMotion else { return }
            turned = true
            try? await Task.sleep(for: .milliseconds(120))
            wake()
            try? await Task.sleep(for: .milliseconds(330))
            wake()
        }
        .accessibilityHidden(true)
    }

    /// The accent's glow, the still rings and any wake, centered on the plate.
    /// The rings fade out before the name below, like the DMG's around its label.
    private var water: some View {
        let dark = scheme == .dark
        let reach = size * 3.1
        return ZStack {
            RadialGradient(
                colors: [Theme.accent.opacity(dark ? 0.20 : 0.12), .clear],
                center: .center,
                startRadius: 0,
                endRadius: size * 2.2
            )
            .frame(width: size * 4.4, height: size * 4.4)
            rings(reach: reach)
        }
        .allowsHitTesting(false)
    }

    private func rings(reach: CGFloat) -> some View {
        ZStack {
            ZStack {
                let strength = scheme == .dark ? 1.4 : 1
                ring(diameter: size * 1.38, opacity: 0.30 * strength, width: 1.3)
                ring(diameter: size * 1.76, opacity: 0.16 * strength, width: 1.15)
                ring(diameter: size * 2.24, opacity: 0.07 * strength, width: 1)
            }
            .opacity(reduceMotion || turned ? 1 : 0)
            .animation(.easeOut(duration: 0.9).delay(0.25), value: turned)
            ForEach(wakes, id: \.self) { _ in
                Wake(from: size, to: size * 2.8)
            }
        }
        .frame(width: reach, height: reach)
        .mask(
            LinearGradient(
                stops: [
                    .init(color: .white, location: 0),
                    .init(color: .white, location: 0.56),
                    .init(color: .clear, location: 0.76),
                ],
                startPoint: .top,
                endPoint: .bottom
            )
        )
    }

    private func ring(diameter: CGFloat, opacity: Double, width: CGFloat) -> some View {
        Circle()
            .strokeBorder(Theme.accent.opacity(opacity), lineWidth: width)
            .frame(width: diameter, height: diameter)
    }

    /// One ripple out from the plate; at most three at once.
    private func wake() {
        guard !reduceMotion, wakes.count < 3 else { return }
        let id = nextWake
        nextWake += 1
        wakes.append(id)
        Task {
            try? await Task.sleep(for: .seconds(Wake.duration + 0.1))
            wakes.removeAll { $0 == id }
        }
    }
}

/// A ring that spreads from the plate's edge and fades.
private struct Wake: View {
    static let duration = 1.7
    let from: CGFloat
    let to: CGFloat
    @State private var spread = false

    var body: some View {
        Circle()
            .strokeBorder(Theme.accent, lineWidth: spread ? 0.75 : 1.8)
            .frame(width: spread ? to : from, height: spread ? to : from)
            .opacity(spread ? 0 : 0.5)
            .onAppear {
                withAnimation(.easeOut(duration: Self.duration)) { spread = true }
            }
    }
}

/// The helm of the app icon (cloud/app/icon.svg), in that icon's units: eight
/// handles, the top one in the accent, the rim, four spokes and the hub.
private struct Wheel: View {
    private static let kingSpoke = Color(red: 0x5B / 255, green: 0x7C / 255, blue: 0xFA / 255)

    var body: some View {
        Canvas { context, size in
            let scale = min(size.width, size.height) / 64 * 0.05797
            context.translateBy(x: size.width / 2, y: size.height / 2)
            context.scaleBy(x: scale, y: scale)
            context.rotate(by: .degrees(-22.5))
            let white = GraphicsContext.Shading.color(.white)
            for i in 0..<8 {
                var handle = Path()
                handle.move(to: CGPoint(x: 0, y: -236))
                handle.addLine(to: CGPoint(x: 0, y: -330))
                context.stroke(
                    handle.applying(CGAffineTransform(rotationAngle: CGFloat(i) * .pi / 4)),
                    with: i == 0 ? .color(Self.kingSpoke) : white,
                    style: StrokeStyle(lineWidth: 76, lineCap: .round)
                )
            }
            context.stroke(Path(ellipseIn: CGRect(x: -214, y: -214, width: 428, height: 428)), with: white, lineWidth: 64)
            for i in 0..<4 {
                let spoke = Path(CGRect(x: -22, y: -200, width: 44, height: 400))
                context.fill(spoke.applying(CGAffineTransform(rotationAngle: CGFloat(i) * .pi / 4)), with: white)
            }
            context.fill(Path(ellipseIn: CGRect(x: -84, y: -84, width: 168, height: 168)), with: white)
        }
    }
}

// MARK: Version

/// The version, which a click copies for a bug report, joined to Check for Updates.
private struct VersionTag: View {
    let version: String
    @State private var copied = false
    @State private var copyReset: Task<Void, Never>?
    private let updates = Updates.shared

    var body: some View {
        HStack(spacing: 0) {
            Button(action: copy) {
                ZStack {
                    Text("Version \(Text(version).foregroundStyle(.primary))")
                        .foregroundStyle(.secondary)
                        .opacity(copied ? 0 : 1)
                    Label("Copied", systemImage: "checkmark")
                        .foregroundStyle(Theme.accent)
                        .opacity(copied ? 1 : 0)
                }
            }
            .buttonStyle(TagSegment())
            .help("Copy version")
            .accessibilityLabel(copied ? "Copied" : "Version \(version). Copy")

            Rectangle().fill(Theme.line).frame(width: 1, height: 13)

            Button("Check for Updates…") { updates.check() }
                .buttonStyle(TagSegment())
                .foregroundStyle(.secondary)
                .disabled(!updates.canCheck)
        }
        .font(.system(size: 11.5, weight: .medium).monospacedDigit())
        // Clips the segments' tints; the surface and its shadow go behind, unclipped.
        .clipShape(Capsule())
        .background {
            Capsule().fill(Theme.surface)
                .shadow(color: .black.opacity(0.05), radius: 2, y: 1)
        }
        .overlay(Capsule().strokeBorder(Theme.line, lineWidth: 1))
    }

    private func copy() {
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
}

/// One half of the version tag: a hover and a press tint inside the capsule.
private struct TagSegment: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        Segment(configuration: configuration)
    }

    private struct Segment: View {
        let configuration: Configuration
        @Environment(\.isEnabled) private var isEnabled
        @State private var hovering = false

        var body: some View {
            configuration.label
                .padding(.horizontal, 11)
                .frame(height: 24)
                .background(Color.primary.opacity(configuration.isPressed ? 0.08 : hovering ? 0.045 : 0))
                .opacity(isEnabled ? 1 : 0.45)
                .contentShape(Rectangle())
                .onHover { hovering = $0 && isEnabled }
                .animation(.easeOut(duration: 0.12), value: hovering)
        }
    }
}

private extension View {
    func pointingHandCursor() -> some View {
        onHover { inside in
            (inside ? NSCursor.pointingHand : NSCursor.arrow).set()
        }
    }
}
