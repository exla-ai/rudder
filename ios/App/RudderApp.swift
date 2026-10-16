import SwiftUI

@main
struct RudderApp: App {
    @State private var session = Session()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView(session: session)
                .onOpenURL { url in session.handle(url: url) }
                .onChange(of: scenePhase) { _, phase in
                    if phase == .active {
                        session.didBecomeActive()
                    }
                }
        }
    }
}

/// Signed out: the native sign-in screen. Signed in: the Rudder app in a web view.
struct RootView: View {
    @Bindable var session: Session

    var body: some View {
        Group {
            if let token = session.token {
                AppWebView(session: session, token: token)
                    .ignoresSafeArea()
                    .background(Theme.background.ignoresSafeArea())
            } else {
                SignInView(session: session)
            }
        }
        .animation(.easeInOut(duration: 0.25), value: session.token == nil)
    }
}
