import SwiftUI

enum Theme {
    static let background = Color("AccentColor")
    static let ink = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark
            ? UIColor(red: 0.93, green: 0.92, blue: 0.90, alpha: 1)
            : UIColor(red: 0.09, green: 0.09, blue: 0.10, alpha: 1)
    })
    static let muted = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark
            ? UIColor(white: 0.62, alpha: 1)
            : UIColor(white: 0.42, alpha: 1)
    })
    static let card = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark
            ? UIColor(red: 0.12, green: 0.12, blue: 0.13, alpha: 1)
            : UIColor.white
    })
    static let hairline = Color(UIColor { traits in
        traits.userInterfaceStyle == .dark
            ? UIColor(white: 1, alpha: 0.10)
            : UIColor(white: 0, alpha: 0.08)
    })
}

struct SignInView: View {
    @Bindable var session: Session
    @State private var code = ""
    @FocusState private var codeFocused: Bool
    @State private var showCode = false

    var body: some View {
        ZStack {
            Theme.background.ignoresSafeArea()
            VStack(spacing: 0) {
                Spacer()
                RudderMark()
                    .frame(width: 76, height: 76)
                    .padding(.bottom, 28)
                Text("Rudder")
                    .font(.system(size: 34, weight: .semibold, design: .default))
                    .foregroundStyle(Theme.ink)
                Text("Your agents, wherever you are.")
                    .font(.system(size: 17))
                    .foregroundStyle(Theme.muted)
                    .padding(.top, 8)
                Spacer()
                VStack(spacing: 12) {
                    Button {
                        session.signIn(provider: "google")
                    } label: {
                        HStack(spacing: 10) {
                            GoogleG().frame(width: 18, height: 18)
                            Text("Continue with Google")
                                .font(.system(size: 17, weight: .semibold))
                        }
                        .frame(maxWidth: .infinity, minHeight: 54)
                        .foregroundStyle(Theme.ink)
                        .background(Theme.card, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).stroke(Theme.hairline))
                    }
                    .buttonStyle(.plain)
                    .disabled(session.signingIn)
                    .accessibilityLabel("Continue with Google")

                    Button("Use GitHub instead") {
                        session.signIn(provider: "github")
                    }
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Theme.muted)
                    .frame(minHeight: 44)
                    .disabled(session.signingIn)

                    if showCode {
                        VStack(alignment: .leading, spacing: 8) {
                            Text("Code from `rudder cloud app` on your laptop")
                                .font(.system(size: 14))
                                .foregroundStyle(Theme.muted)
                            HStack(spacing: 10) {
                                TextField("ABCD-EFGH", text: $code)
                                    .font(.system(size: 20, weight: .semibold, design: .monospaced))
                                    .textInputAutocapitalization(.characters)
                                    .autocorrectionDisabled()
                                    .focused($codeFocused)
                                    .submitLabel(.go)
                                    .onSubmit { Task { await session.pair(code: code) } }
                                    .padding(.horizontal, 14)
                                    .frame(minHeight: 54)
                                    .background(Theme.card, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                                    .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).stroke(Theme.hairline))
                                    .accessibilityLabel("Pairing code")
                                Button {
                                    codeFocused = false
                                    Task { await session.pair(code: code) }
                                } label: {
                                    Text("Pair")
                                        .font(.system(size: 17, weight: .semibold))
                                        .frame(minWidth: 72, minHeight: 54)
                                        .foregroundStyle(Theme.background)
                                        .background(Theme.ink, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                                }
                                .buttonStyle(.plain)
                                .disabled(session.signingIn || code.trimmingCharacters(in: .whitespaces).count < 8)
                            }
                        }
                        .padding(.top, 6)
                    } else {
                        Button("Have a code from rudder cloud app?") {
                            showCode = true
                            codeFocused = true
                        }
                        .font(.system(size: 14))
                        .foregroundStyle(Theme.muted)
                        .frame(minHeight: 36)
                    }

                    if let error = session.errorMessage {
                        Text(error)
                            .font(.system(size: 14))
                            .foregroundStyle(.red)
                            .multilineTextAlignment(.center)
                    }
                }
                // A phone-sized column on iPad, not buttons a screen wide.
                .frame(maxWidth: 420)
                .padding(.horizontal, 24)
                .padding(.bottom, 28)
            }
            if session.signingIn {
                ProgressView().controlSize(.large)
            }
        }
    }
}

/// The Rudder logo (the Helm mark from the site and the app icon).
struct RudderMark: View {
    var body: some View {
        Image("Logo")
            .resizable()
            .interpolation(.high)
            .aspectRatio(contentMode: .fit)
            .accessibilityHidden(true)
    }
}

/// Google's "G", drawn so no image asset is needed.
struct GoogleG: View {
    var body: some View {
        Canvas { context, size in
            let rect = CGRect(origin: .zero, size: size).insetBy(dx: size.width * 0.06, dy: size.height * 0.06)
            let center = CGPoint(x: rect.midX, y: rect.midY)
            let radius = rect.width / 2
            let line = rect.width * 0.2
            func arc(_ from: Double, _ to: Double, _ color: Color) {
                var path = Path()
                path.addArc(center: center, radius: radius - line / 2, startAngle: .degrees(from), endAngle: .degrees(to), clockwise: false)
                context.stroke(path, with: .color(color), lineWidth: line)
            }
            arc(-40, 45, Color(red: 0.26, green: 0.52, blue: 0.96))
            arc(45, 135, Color(red: 0.20, green: 0.66, blue: 0.33))
            arc(135, 200, Color(red: 0.98, green: 0.74, blue: 0.02))
            arc(200, 320, Color(red: 0.92, green: 0.26, blue: 0.21))
            var bar = Path()
            bar.addRect(CGRect(x: center.x, y: center.y - line / 2, width: radius, height: line))
            context.fill(bar, with: .color(Color(red: 0.26, green: 0.52, blue: 0.96)))
        }
    }
}
