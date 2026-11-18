import Foundation
import Observation
import Sparkle

/// The app updates itself with Sparkle. Info.plist sets the feed (`SUFeedURL`,
/// the appcast on the latest GitHub release), the key updates are signed with
/// (`SUPublicEDKey`), and checks once a day that download an update in the
/// background and install it when Rudder quits (`SUEnableAutomaticChecks`,
/// `SUAutomaticallyUpdate`). Rudder > Check for Updates… checks now in
/// Sparkle's own window.
@MainActor
@Observable
final class Updates {
    static let shared = Updates()

    /// False until the updater starts, and while a check or install is under way.
    private(set) var canCheck = false

    @ObservationIgnored private let controller: SPUStandardUpdaterController
    @ObservationIgnored private var observation: NSKeyValueObservation?

    private init() {
        controller = SPUStandardUpdaterController(startingUpdater: false, updaterDelegate: nil, userDriverDelegate: nil)
        // Sparkle changes the property on the main thread only.
        observation = controller.updater.observe(\.canCheckForUpdates, options: [.initial, .new]) { [weak self] updater, _ in
            MainActor.assumeIsolated {
                self?.canCheck = updater.canCheckForUpdates
            }
        }
    }

    /// Once launch has finished: the first scheduled check follows from here.
    func start() {
        controller.startUpdater()
    }

    func check() {
        controller.checkForUpdates(nil)
    }
}
