import SwiftUI

/// The full-screen "Talk to Rudder" view, opened by the web app's mic button
/// (the `voice` message). It starts a conversation when it appears and ends
/// the ones it started when it goes; a conversation CarPlay started keeps
/// running, so picking up the phone at a light does not cut the car off.
@MainActor
struct VoiceView: View {
    let onClose: @MainActor () -> Void
    @State private var closing = false

    private var assistant: VoiceAssistant { .shared }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            VStack(spacing: 0) {
                HStack {
                    Spacer()
                    Button(action: close) {
                        Image(systemName: "xmark")
                            .font(.system(size: 17, weight: .semibold))
                            .foregroundStyle(.white.opacity(0.85))
                            .frame(width: 44, height: 44)
                            .background(.white.opacity(0.12), in: Circle())
                    }
                    .accessibilityLabel("Close")
                }
                .padding(.horizontal, 20)
                .padding(.top, 8)

                Spacer()

                VoiceOrb(state: assistant.state, level: assistant.level)
                    .frame(width: 220, height: 220)
                    .accessibilityHidden(true)

                Text(statusText)
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(.white.opacity(0.6))
                    .padding(.top, 28)
                    .accessibilityAddTraits(.updatesFrequently)

                captions
                    .padding(.top, 24)
                    .padding(.horizontal, 28)

                Spacer()

                footer
                    .padding(.horizontal, 28)
                    .padding(.bottom, 24)
            }
        }
        .preferredColorScheme(.dark)
        .onAppear {
            if !assistant.isActive {
                assistant.start(surface: .phone)
            }
        }
        .onChange(of: assistant.state) { old, new in
            // The conversation ended on its own (Rudder said goodbye, or it
            // went quiet): close, unless it ended with an error to show.
            if new == .idle && old.isActive {
                close()
            }
        }
        .onDisappear {
            if assistant.surface == .phone {
                assistant.stop()
            }
        }
    }

    @ViewBuilder
    private var captions: some View {
        VStack(spacing: 14) {
            if !assistant.userCaption.isEmpty {
                Text(assistant.userCaption)
                    .font(.system(size: 16))
                    .foregroundStyle(.white.opacity(0.55))
                    .lineLimit(3)
            }
            if !assistant.assistantCaption.isEmpty {
                Text(assistant.assistantCaption)
                    .font(.system(size: 20, weight: .medium))
                    .foregroundStyle(.white)
                    .lineLimit(6)
            }
        }
        .multilineTextAlignment(.center)
        .frame(maxWidth: 520)
        .animation(.easeOut(duration: 0.2), value: assistant.userCaption)
    }

    @ViewBuilder
    private var footer: some View {
        if case .error = assistant.state {
            Button {
                assistant.start(surface: .phone)
            } label: {
                Text("Try again")
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(.black)
                    .frame(maxWidth: .infinity, minHeight: 52)
                    .background(.white, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            }
        } else if let notice = assistant.notice {
            Text(notice)
                .font(.system(size: 13))
                .foregroundStyle(.white.opacity(0.4))
                .multilineTextAlignment(.center)
                .lineLimit(2)
        } else {
            Text("Ask what your agents are doing, or tell one what to do next.")
                .font(.system(size: 13))
                .foregroundStyle(.white.opacity(0.4))
                .multilineTextAlignment(.center)
        }
    }

    private var statusText: String {
        switch assistant.state {
        case .idle: ""
        case .connecting: "Connecting"
        case .listening: "Listening"
        case .thinking: "Thinking"
        case .speaking: "Rudder"
        case .error(let message): message
        }
    }

    private func close() {
        guard !closing else { return }
        closing = true
        if assistant.surface == .phone {
            assistant.stop()
        }
        onClose()
    }
}

/// A soft orb that breathes while idle, swells with the voice it hears or
/// speaks, and turns while Rudder is thinking.
private struct VoiceOrb: View {
    let state: VoiceState
    let level: Float

    var body: some View {
        TimelineView(.animation) { timeline in
            let time = timeline.date.timeIntervalSinceReferenceDate
            let breathe = 1 + 0.035 * sin(time * breathSpeed)
            let swell = 1 + 0.3 * CGFloat(min(max(level, 0), 1))
            let scale = CGFloat(breathe) * swell
            ZStack {
                Circle()
                    .fill(RadialGradient(colors: [glow.opacity(0.55), .clear], center: .center, startRadius: 10, endRadius: 140))
                    .scaleEffect(scale * 1.35)
                    .blur(radius: 20)
                Circle()
                    .fill(AngularGradient(colors: colors, center: .center, angle: .degrees(time * spinSpeed)))
                    .overlay(Circle().fill(RadialGradient(colors: [.white.opacity(0.35), .clear], center: .init(x: 0.35, y: 0.3), startRadius: 0, endRadius: 90)))
                    .scaleEffect(scale * 0.72)
                    .opacity(state.isActive ? 1 : 0.4)
            }
        }
        .animation(.easeOut(duration: 0.12), value: level)
    }

    private var colors: [Color] {
        switch state {
        case .speaking:
            [Color(red: 0.29, green: 0.48, blue: 0.85), Color(red: 0.36, green: 0.78, blue: 0.86), Color(red: 0.29, green: 0.48, blue: 0.85)]
        case .thinking, .connecting:
            [Color(red: 0.45, green: 0.38, blue: 0.86), Color(red: 0.29, green: 0.48, blue: 0.85), Color(red: 0.45, green: 0.38, blue: 0.86)]
        case .error:
            [Color(red: 0.75, green: 0.32, blue: 0.30), Color(red: 0.55, green: 0.25, blue: 0.28), Color(red: 0.75, green: 0.32, blue: 0.30)]
        case .listening, .idle:
            [Color(red: 0.29, green: 0.48, blue: 0.85), Color(red: 0.40, green: 0.60, blue: 0.92), Color(red: 0.29, green: 0.48, blue: 0.85)]
        }
    }

    private var glow: Color {
        if case .error = state { return Color(red: 0.75, green: 0.32, blue: 0.30) }
        return Color(red: 0.36, green: 0.58, blue: 0.95)
    }

    private var breathSpeed: Double {
        state == .thinking || state == .connecting ? 3.2 : 1.6
    }

    private var spinSpeed: Double {
        switch state {
        case .thinking, .connecting: 160
        case .speaking: 60
        default: 20
        }
    }
}
