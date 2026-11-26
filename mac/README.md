# Rudder for macOS

A native shell around the Rudder app (`/app` on the control plane), the macOS
twin of `../ios`. It adds what a browser tab cannot do:

- Sign in with Google or GitHub in the default browser, through the device
  login `rudder login` uses: `POST /api/cli/login` (with `client: "mac"`),
  open `/cli/oauth/<provider>/start?device_code=...`, poll
  `/api/cli/login/poll` until it returns an `rdr_` token. The token is a
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
- Updates itself with [Sparkle](https://sparkle-project.org) 2: a daily check,
  downloaded in the background and installed when Rudder quits, and
  Rudder > Check for Updates… (see "Updates" below).

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
│   ├── About.swift          Rudder > About Rudder: the app's own About window
│   ├── Updates.swift        Sparkle: the updater and Rudder > Check for Updates…
│   ├── Info.plist, Rudder.entitlements, Assets.xcassets (icon from ../assets/rudder.icns)
├── Core/RudderCore.swift    pure logic: URLs, rudder:// parsing, needs-you counting, CLI output
├── Tests/RudderCoreTests.swift
└── scripts/
    ├── build.sh             xcodegen, tests, unsigned Release archive, then package.sh
    ├── package.sh           sign (Sparkle's helpers too), notarize, DMG, release.env
    ├── publish.sh           stages the GitHub release, runs .github/workflows/mac-publish.yml
    ├── appcast.mjs          the Sparkle appcast: write (EdDSA-signed) and verify
    ├── make-dmg.sh          the DMG window (dmgbuild)
    └── ci.sh                the same steps on an offstage Mac (release.yml, mac.yml)
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
web paths for those. `local` is the value recorded by the last local tool check.
Opening a local copy requests the current status from the shell. Checking again
reads the login shell's PATH afresh.

Messages, sent with `window.webkit.messageHandlers.rudder.postMessage(msg)`
(only pages under `/app` or `/api` of the control plane are listened to):

| `type` | fields | answer |
|---|---|---|
| `signIn`, `signOut` | | back to the native sign-in screen |
| `openExternal` | `url` | default browser (http, https, mailto) |
| `badge` | `count` | Dock badge |
| `notify` | `title`, `body`, `path` (an `/app/...` path) | notification; a click opens `path` |
| `localStatus` | `id?`, `refreshPATH?` | `window.rudderLocalStatus({id, rudder, claude, codex})` |
| `openLocal` | `id`, `runId`, `repo` (`owner/name`) | `window.rudderLocal({id, runId, ok, path?, title?, checkout?, error?, cancelled?})` |
| `openPath` | `path`, `app` (`dashboard`, `terminal`, `finder`, `xcode`) | `window.rudderOpenedPath({path, app, ok})` |
| `appearance` | `value` (`system`, `light`, `dark`) | none; the whole app follows it and keeps it for the next launch |

The other way, the app opens a page (a notification, the menu bar, Cmd+,)
by calling `window.rudderOpenPath({path})` when an app page is loaded, so
the page moves within itself instead of reloading.

`openLocal` in detail:

1. `rudder` is found on the PATH your login shell prints (read on demand as
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

`project.yml` uses team `3C4383262W`, where the Developer ID certificate lives (the
iPhone app is on Exla Corp.'s team, `P76SY8RFGL`). Change `DEVELOPMENT_TEAM`
if the certificate moves. Point at another control plane
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

It generates the project, runs RudderCoreTests and archives an unsigned Apple
Silicon (arm64) Release build. `mac/scripts/package.sh` then signs it with
Developer ID, the hardened runtime and a secure timestamp, inside out:
Sparkle's helpers (`Installer.xpc`, `Downloader.xpc` keeping its entitlements,
`Autoupdate`, `Updater.app`), `Sparkle.framework`, then the app with
`App/Rudder.entitlements`, never `--deep`. It checks every one of them for team
`3C4383262W` and the runtime flag, notarizes and staples the app, builds
`mac/build/Rudder.dmg` with `mac/scripts/make-dmg.sh`, signs, notarizes and
staples the DMG, runs `spctl` on both, writes `mac/build/release.env`
(`VERSION`, `BUILD`, `MINIMUM_SYSTEM`) and prints the SHA-256.
`RUDDER_SKIP_NOTARIZE=1` gives a signed, unnotarized build for local testing.

When Xcode cannot build on the signing Mac, build the unsigned Release archive
on another Mac (an Offstage Mac:
`xcodebuild archive ... CODE_SIGNING_ALLOWED=NO`, as build.sh does), copy its
`Rudder.app` over (`ditto -c -k --keepParent`), and run only
`mac/scripts/package.sh path/to/Rudder.app` on the signing Mac. Never sign the
app alone with a bare `codesign`: `Sparkle.framework` and its helpers need this
team's signature first, or the hardened runtime refuses to load the framework.

The CI route (`mac/scripts/ci.sh`) signs through Xcode's archive and Developer
ID export instead, which re-signs Sparkle's helpers itself.

### DMG window

`make-dmg.sh <Rudder.app> <out.dmg>` lays out the Finder window with
[dmgbuild](https://pypi.org/project/dmgbuild/) 1.6.7 (pinned, in a throwaway
venv; needs Python 3.10+, so Homebrew's `python3`, not Xcode's 3.9). Settings are
in `mac/dmg/settings.py`: 660x420, 128pt icons, Rudder at (170, 196) and
Applications at (490, 196), LZFSE-compressed, the app icon as the volume icon.
The Offstage candidate build uses the same script.

Applications is a Finder alias to `/Applications` that carries the folder's
icon as its own custom icon (`mac/dmg/applications.js`, run by make-dmg.sh with
`osascript`), not a symlink. Finder on macOS 26 (seen on 26.3) draws every
symlink and plain alias as an empty dashed square; it draws a custom icon as
is. Dropping Rudder on the alias still copies it into `/Applications`.

The art is light on purpose: Finder keeps icon labels dark on a background
picture. `mac/dmg/background.py` writes `background.svg` (paper, faint chart
contours, a wake from Rudder into ripples around Applications, with a gap in the
rings for the label). After changing it, render both PNGs; dmgbuild joins them
into one Retina TIFF:

```bash
python3 mac/dmg/background.py
for s in 1 2; do
  chrome --headless=new --hide-scrollbars --force-device-scale-factor=$s \
    --window-size=660,420 --virtual-time-budget=8000 \
    --screenshot="mac/dmg/background$([ $s = 2 ] && echo @2x).png" \
    "file://$PWD/mac/dmg/background.svg"
done
```

The Mac app carries Rudder's version: `npm version` runs `scripts/version.mjs`, which sets `MARKETING_VERSION` and the derived build number in `project.yml` (and the Cloud worker image pin). Never bump them by hand.

## Releasing: the DMG and the appcast

Both live on the repo's **latest** GitHub release, `mac-v<version>-<build>`:

- `Rudder.dmg`: `https://rudder.exla.ai/download/mac` redirects (site/vercel.json)
  to `https://github.com/exla-ai/rudder/releases/latest/download/Rudder.dmg`.
- `appcast.xml`: the Sparkle feed, `SUFeedURL` in `App/Info.plist`,
  `https://github.com/exla-ai/rudder/releases/latest/download/appcast.xml`.
  GitHub answers with redirects to the asset, which Sparkle follows (https all
  the way). It has one item: that release's version and build, minimum macOS
  14.0, and an enclosure pointing at that release's own
  `releases/download/mac-v<version>-<build>/Rudder.dmg` with its length and
  EdDSA signature, so it stays right however "latest" moves.

Every Rudder version ships the Mac app (`.github/workflows/release.yml`, the
rudder-ship skill). Pushing the version commit to `release-check/X.Y.Z` tests
the app on an Offstage Mac, signs and notarizes the app and DMG, signs the
Sparkle feed on the Linux runner and stages the three files in the draft
release `vX.Y.Z`. The `vX.Y.Z` tag at the same commit checks them against
their hashes and `App/Info.plist`, waits for Cloud to serve the matching worker
image, publishes `mac-v<version>-<build>` as the latest release and checks
both public URLs.

To publish a manually built DMG, publish both files from any machine with `gh` signed in to an account that can push
to the repo, once `mac/build/Rudder.dmg` and `mac/build/release.env` are there
(copied from the Mac that packaged them) and the commit they were built from is
pushed. No key is needed there:

```bash
mac/scripts/publish.sh mac/build <commit>
```

`publish.sh` uploads `Rudder.dmg` and `release.env` to a draft release
`mac-v<version>-<build>` that targets `<commit>`, runs
`.github/workflows/mac-publish.yml` on it (`gh workflow run`) and waits for it
(`gh run watch`). For a manual release, that job handles Sparkle signing:

1. It downloads both files from the draft and checks that the tag matches
   `release.env`.
2. `appcast.mjs write` signs the downloaded DMG into `appcast.xml` with the
   `SPARKLE_ED_PRIVATE_KEY` secret, and refuses a key that is not the one for
   `SUPublicEDKey`.
3. `appcast.mjs verify` checks `appcast.xml` against `SUPublicEDKey` in
   `mac/App/Info.plist` at `<commit>`, the key the app ships: the signature
   over the DMG, the version, build, minimum macOS, URL and length.
4. It uploads `appcast.xml`, removes `release.env` and publishes the release
   as latest, which creates its tag at `<commit>`.

A release without `appcast.xml` stops every installed app's updates until the
next one, and a version or build in the appcast that differs from the DMG's
would make the apps update in a loop: always publish with `release.env` from
the build. Never replace `Rudder.dmg` on a published release either: the
appcast's length and signature are for the bytes it signed. A new DMG needs a
new Rudder version; `publish.sh` refuses a tag that has a release or draft
already. If the job fails, the draft stays: fix the cause and run
`gh workflow run mac-publish.yml --ref main -f tag=mac-v<version>-<build>`,
or `gh release delete` the draft to start over.

Rudder's npm releases create no GitHub release, so the Mac release stays
latest. If any other GitHub release is created later and marked latest, both
links break until `Rudder.dmg` and `appcast.xml` are attached to it too.

## Updates

Sparkle 2 (`packages` in `project.yml`, pinned to an exact release at least two
weeks old) runs from `App/Updates.swift`: an `SPUStandardUpdaterController`
started once launch finishes, and Rudder > Check for Updates…. `Info.plist`
sets `SUFeedURL`, `SUPublicEDKey`, `SUEnableAutomaticChecks` (check daily
without asking) and `SUAutomaticallyUpdate` (download in the background,
install when Rudder quits; Rudder keeps running with its window closed, so a
downloaded update is offered in Sparkle's window at a later check). The app is
not sandboxed, so Sparkle's XPC services stay unused (`SUEnableInstallerLauncherService`
and `SUEnableDownloaderService` are off), and no entitlement is needed.

Updates are signed with an Ed25519 key in Sparkle's format. `SUPublicEDKey` is
base64 of the raw 32-byte public key. The private key is base64 of the 32-byte
seed, what Sparkle's `generate_keys -x` exports. It is the
`SPARKLE_ED_PRIVATE_KEY` repo secret, read only by the GitHub Actions signing steps;
no machine needs a key file. Keep a backup somewhere safe, never in a Rudder
machine's home directory (a machine update recreates it): losing the key means
no installed app accepts another update until users download a new DMG by
hand. `generate_keys -f <file>` imports it into a Mac's keychain if Sparkle's
own tools are ever wanted; they are not needed.

The first Sparkle build (1.2.0) cannot reach users of 1.1.0 or older, who have
no updater: they download it once from `/download/mac`.

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
8. Rudder > Check for Updates… answers in Sparkle's window: up to date, or the
   newer release offered and installed with a relaunch.

## Known gaps

- The 30s poll uses `GET /api/app/state?passive=1` with `X-Rudder-Passive: 1`.
  Today the control plane counts every state read as activity (`touchHome`) and
  wakes a sleeping home machine on it, so until the server skips both for a
  passive read, an open Mac app keeps the home awake and never idle.
- Slack connects that send the page to an OAuth url open in the default
  browser, so the callback lands in the browser, not this window.
- A Cloud deploy reaches an open window only after Cmd+R or a relaunch: the
  web app has no version the shell could compare against.
