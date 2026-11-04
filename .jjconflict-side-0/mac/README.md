# Rudder for macOS

A native shell around the Rudder app (`/app` on the control plane), the macOS
twin of `../ios`. It adds what a browser tab cannot do:

- Sign in with Google or GitHub through `ASWebAuthenticationSession`
  (`/app/login?native=1` ends at `rudder://auth?token=rdr_...`). The token is a
  generic password in the login keychain (service `ai.exla.rudder`). Sign Out
  (app menu, or the page's `signOut`) deletes it.
- A menu bar item that lists the agents that need you, polled every 30s, with a
  notification when a new one appears and the count on the Dock icon.
- Notifications the page asks for (`notify`), which open their page when clicked.
- **Copy an agent to this Mac**: runs your local `rudder cloud pull <runId> --json`
  in your clone of the repo, then offers the dashboard, Terminal, Finder or Xcode.
  It is a copy: the cloud agent keeps running.
- Mac menus: Cmd+R reload, Cmd+[ / Cmd+] back and forward, Cmd+, opens
  `/app/settings`. The last page is reopened on launch.

Bundle id `ai.exla.rudder.mac`, macOS 14+, SwiftUI app lifecycle, Developer ID
distribution (not the App Store): no sandbox, hardened runtime with no exceptions.

## Layout

```
mac/
├── project.yml              XcodeGen spec (app + RudderCoreTests)
├── App/
│   ├── RudderMacApp.swift   @main, menus, AppDelegate, sign-in screen, menu bar extra
│   ├── AppModel.swift       window/navigation, needs-you polling, notifications
│   ├── AppWebView.swift     WKWebView + the `rudder` message bridge
│   ├── Session.swift        sign-in, pairing codes, Keychain
│   ├── LocalAgents.swift    finding local tools, `rudder cloud pull`, checkout mapping
│   ├── Info.plist, Rudder.entitlements, Assets.xcassets (icon from ../assets/rudder.icns)
├── Core/RudderCore.swift    pure logic: URLs, rudder:// parsing, needs-you counting, CLI output
├── Tests/RudderCoreTests.swift
└── scripts/build.sh         xcodegen, tests, archive, notarize, DMG, optional GitHub release
```

## The bridge (for the web app)

At document start the app sets:

```js
window.RUDDER_NATIVE = Object.freeze({
  platform: "mac", token: "rdr_...", version: "1.0.0",
  notifications: true,   // `notify` posts a macOS notification
  local: true,           // the `rudder` CLI was installed here last time the app looked
  openLocal: true,       // this build answers openLocal / localStatus / openPath
  attachments: true,     // <input type=file> opens the macOS open panel
});
```

`picker`, `login`, `voice` and `digest` are absent, so the page uses its plain
web paths for those. `local` is a cached value from the previous check; the
current answer arrives once per page load as `window.rudderLocalStatus(...)`.

Messages, sent with `window.webkit.messageHandlers.rudder.postMessage(msg)`
(only pages under `/app` or `/api` of the control plane are listened to):

| `type` | fields | answer |
|---|---|---|
| `signIn`, `signOut` | | back to the native sign-in screen |
| `openExternal` | `url` | default browser (http, https, mailto) |
| `badge` | `count` | Dock badge |
| `notify` | `title`, `body`, `path` (an `/app/...` path) | notification; a click opens `path` |
| `localStatus` | `id?` | `window.rudderLocalStatus({id, rudder, claude, codex, rudderVersion?})` |
| `openLocal` | `id`, `runId`, `repo` (`owner/name`) | `window.rudderLocal({id, runId, ok, path?, title?, checkout?, error?, cancelled?})` |
| `openPath` | `path`, `app` (`dashboard`, `terminal`, `finder`, `xcode`) | `window.rudderOpenedPath({path, app, ok})` |

`openLocal` in detail:

1. `rudder` is found on the PATH your login shell prints (run once per launch as
   `$SHELL -l -i -c`, so Homebrew, npm, nvm and volta installs are found) plus the
   usual install directories. Missing: `ok:false` with the npm install line.
2. The checkout of `repo` comes from a remembered mapping (UserDefaults
   `localRepoFolders`). The first time, an open panel asks for the folder; it
   must contain `.git` or `.jj`, and its `origin` must be the project's GitHub
   repo (the CLI requires it), else the app asks for another. Cancel gives
   `cancelled:true`.
3. `rudder cloud pull <runId> --json` runs in that folder with
   `RUDDER_CLOUD_TOKEN` / `RUDDER_CLOUD_URL` set to this app's sign-in (so it
   works even if `rudder login` was never run here) and auto-update off. The
   CLI prints `{path, runId, title}`; on failure it exits non-zero and the last
   stderr line is the `error`. It retries a waking machine once itself; the app
   stops it after 3 minutes.
4. The pull is a COPY: the cloud agent keeps running. The copy appears paused
   in that checkout's `rudder` dashboard. After answering the page, an alert
   offers Open Rudder in Terminal (a `.command` file that runs `rudder` in the
   checkout, so no Apple Events permission), Open Workspace in Terminal, Open in
   Finder, and Open in Xcode when the workspace has an `.xcworkspace` / `.xcodeproj`.
   The page can do the same later with `openPath` (`app: "dashboard"` etc.).

`openPath` only opens workspaces this app pulled (the last 50 are remembered).

## Build and run (on a Mac)

Needs Xcode 16+ and XcodeGen.

```bash
brew install xcodegen
cd mac
xcodegen generate
open Rudder.xcodeproj        # Run the "Rudder" scheme
xcodebuild test -project Rudder.xcodeproj -scheme RudderCoreTests -destination 'platform=macOS'
```

`project.yml` uses team `3C4383262W` (the iOS app's). Change `DEVELOPMENT_TEAM`
if the Developer ID certificate lives elsewhere. Point at another control plane
with `defaults write ai.exla.rudder.mac RudderServerURL http://localhost:8787`
or the `RUDDER_CLOUD_URL` environment variable in the scheme.

## Sign, notarize, DMG

One-time setup on the release Mac:

1. A **Developer ID Application** certificate in the login keychain
   (Xcode > Settings > Accounts > Manage Certificates > +).
2. Notary credentials, stored once:
   ```bash
   xcrun notarytool store-credentials rudder-notary \
     --apple-id you@exla.ai --team-id 3C4383262W --password <app-specific password>
   ```
   (or an App Store Connect API key: `RUDDER_NOTARY_KEY`, `_KEY_ID`, `_ISSUER`).

Then:

```bash
RUDDER_NOTARY_PROFILE=rudder-notary mac/scripts/build.sh
```

It generates the project, runs RudderCoreTests, archives a universal
(arm64 + x86_64) Release build signed with Developer ID, hardened runtime and a
secure timestamp, checks the signature, notarizes and staples the app, builds
`mac/build/Rudder.dmg` (app + Applications link), signs, notarizes and staples
the DMG, runs `spctl` on both, and prints the SHA-256.
`RUDDER_SKIP_NOTARIZE=1` gives a signed, unnotarized build for local testing.

Bump `MARKETING_VERSION` / `CURRENT_PROJECT_VERSION` in `project.yml` per release.

## Hosting the DMG

`https://rudder.exla.ai/download/mac` already redirects (site/vercel.json) to
`https://github.com/exla-ai/rudder/releases/latest/download/Rudder.dmg`, so the
DMG must be an asset named exactly `Rudder.dmg` on the repo's **latest** GitHub
release:

```bash
RUDDER_NOTARY_PROFILE=rudder-notary RUDDER_RELEASE_TAG=mac-v1.0.0 mac/scripts/build.sh
```

creates release `mac-v1.0.0` with the DMG and marks it latest. If any other
GitHub release is created later (and marked latest), the download link breaks
until the DMG is attached to it too (`gh release upload <tag> mac/build/Rudder.dmg`),
so either keep GitHub releases for the Mac app only, or point the redirect at a
fixed tag or a bucket instead.

## Verify on a Mac

1. Launch; sign in with Google. The app opens on `/app`; quit and relaunch: still
   signed in, same page.
2. Cmd+R, Cmd+[, Cmd+], Cmd+, (settings). A GitHub link opens in the browser.
3. Menu bar: the Helm wheel icon; with an agent waiting, its count, the agent listed;
   clicking it opens that agent. Make a new agent need you: a notification
   appears within 30s; clicking it (with the window closed) opens the window on it.
4. In Safari's Web Inspector (Debug build: Develop > your Mac > Rudder):
   `webkit.messageHandlers.rudder.postMessage({type:"localStatus"})` and watch
   `window.rudderLocalStatus`; `postMessage({type:"notify",title:"t",body:"b",path:"/app/settings"})`.
5. Copy: `postMessage({type:"openLocal",id:"1",runId:"<a run id>",repo:"<owner/name>"})`,
   choose the clone, wait for `window.rudderLocal`, try each open button (Open Rudder in Terminal should show the copy paused).
6. Close the window: the menu bar item stays; the Dock icon reopens the window.
7. `spctl --assess --type execute -v mac/build/Rudder.app` says "Notarized Developer ID".

## Known gaps

- The 30s poll uses `GET /api/app/state?passive=1` with `X-Rudder-Passive: 1`.
  Today the control plane counts every state read as activity (`touchHome`) and
  wakes a sleeping home machine on it, so until the server skips both for a
  passive read, an open Mac app keeps the home awake and never idle.
- Slack/Linear connects that send the page to an OAuth url open in the default
  browser, so the callback lands in the browser, not this window.
- No auto-update (Sparkle) yet: users download new DMGs.
