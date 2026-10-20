import Foundation
import UserNotifications

/// The morning digest's 9:01 notification.
///
/// The control plane writes the digest at 9:01 in the phone's time zone (the
/// web app sends it on every state poll); this is the local, repeating
/// reminder that says it is there. Tapping it opens `/app/digest`. A local
/// notification needs no push certificate, so it works on every build that
/// has this file. The web app turns it on with a `digestReminder` message and
/// hears back through `window.rudderDigestReminder({enabled, denied})`.
@MainActor
final class DigestReminder: NSObject, UNUserNotificationCenterDelegate {
    static let shared = DigestReminder()
    static let identifier = "rudder.digest"

    /// Set by the app at launch; a tap on the notification opens the digest there.
    weak var session: Session?

    func install() {
        UNUserNotificationCenter.current().delegate = self
    }

    /// Turns the daily reminder on or off. Returns whether it is now scheduled,
    /// and whether iOS refused (notifications off in Settings).
    func set(enabled: Bool) async -> (enabled: Bool, denied: Bool) {
        let center = UNUserNotificationCenter.current()
        center.removePendingNotificationRequests(withIdentifiers: [Self.identifier])
        guard enabled else { return (false, false) }
        var settings = await center.notificationSettings()
        if settings.authorizationStatus == .notDetermined || settings.alertSetting != .enabled {
            _ = try? await center.requestAuthorization(options: [.alert, .sound, .badge])
            settings = await center.notificationSettings()
        }
        guard settings.alertSetting == .enabled else { return (false, true) }
        let content = UNMutableNotificationContent()
        content.title = "Your Rudder digest"
        content.body = "What your agents did since yesterday morning, by project."
        content.sound = .default
        content.threadIdentifier = Self.identifier
        var at = DateComponents()
        at.hour = 9
        at.minute = 1
        let request = UNNotificationRequest(
            identifier: Self.identifier,
            content: content,
            trigger: UNCalendarNotificationTrigger(dateMatching: at, repeats: true)
        )
        do {
            try await center.add(request)
            return (true, false)
        } catch {
            return (false, false)
        }
    }

    func isScheduled() async -> Bool {
        let pending = await UNUserNotificationCenter.current().pendingNotificationRequests()
        return pending.contains { $0.identifier == Self.identifier }
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        guard response.notification.request.identifier == Self.identifier else { return }
        await MainActor.run { self.session?.openDigest() }
    }
}
