---
name: rudder-ios-ship
description: Publish the Rudder iPhone app on the App Store and ship updates to it. Upload a build from an Offstage Mac to App Store Connect and TestFlight, set the listing text, screenshots and App Review details from ios/appstore, submit for review, release, and read a rejection. Use when asked to publish, submit, upload or release the iPhone or iOS app, to make a TestFlight build, to change the App Store listing or screenshots, to add an iPhone capability (push, associated domains, app groups, CarPlay), or when App Review answers.
---

# The iPhone app on the App Store

Everything runs in the **iPhone App Store** workflow
(`.github/workflows/ios-appstore.yml`). Its Linux runner builds nothing:
an Offstage Mac archives and uploads (`.offstage/ios-appstore.mjs`,
`ios/scripts/appstore.sh`), and `ios/scripts/appstore.mjs` talks to the App
Store Connect API. Publishing, submitting and releasing are outward-facing:
do them only when the user asked for that step.

| Step | Does |
|---|---|
| `upload` | runs `identity`, archives the commit's app on an Offstage Mac, uploads it, waits until TestFlight has it (internal testers) |
| `identity` | registers the bundle id if missing, checks it is the project's team, enables the capabilities the entitlements need, checks the app record exists |
| `prepare` | does what `identity` does to the bundle id, creates or renames the App Store version, sets text, categories and App Review details from `ios/appstore`, attaches the build, uploads screenshots. Submits nothing |
| `submit` | sends the prepared version to App Review. Needs `confirm` set to the version |
| `release` | puts an approved version live (`releaseType` is `MANUAL`). Needs `confirm` |
| `status` | versions, builds and review submissions, or that the app record does not exist yet and how to create it |

```sh
gh workflow run ios-appstore.yml --ref v5.1.0 -f step=upload
gh workflow run ios-appstore.yml --ref v5.1.0 -f step=submit -f confirm=5.1.0
gh run watch "$(gh run list --workflow ios-appstore.yml --limit 1 --json databaseId --jq '.[0].databaseId')" --exit-status
```

Every step has a local dry run that prints the requests and needs no key:
`node ios/scripts/appstore.mjs prepare --dry-run`. `node ios/scripts/appstore.mjs check`
checks `ios/appstore` against Apple's limits (lengths, em dashes, screenshot
sizes, alpha).

## Team and bundle id

The iPhone app belongs to Exla Corp.'s Apple team, `P76SY8RFGL`, as
`ai.exla.rudder`. Both are set once, in `ios/project.yml`
(`DEVELOPMENT_TEAM`, the Rudder target's `PRODUCT_BUNDLE_IDENTIFIER`);
`ios/scripts/identity.sh` and `appstore.mjs` read them from there. The key
is that team's: `IOS_ASC_KEY_ID`, `IOS_ASC_ISSUER_ID`, `IOS_ASC_KEY_P8`. The
Mac app is on another team with its own `ASC_KEY_*`; never mix them.
`identity` refuses a key whose team does not own the bundle id.

Moving to another team or bundle id means a new app record: App Store
Connect cannot move a bundle id between teams except by an app transfer
(App Store Connect, App Information, Transfer App, done by both Account
Holders, only for an app that has been on the App Store).

## The old TestFlight app

Builds up to 12 went to TestFlight as `dev.viraat.rudder` on the user's
personal team (`3C4383262W`). The Exla app is a different app, not an
update to it:

- It has its own TestFlight groups; nothing moves from the old record.
- Testers install it fresh and sign in again. Nothing on the phone carries
  over (the Keychain token, notification permission, settings); accounts and
  agents live on the server and are all there after sign-in.
- Both apps claim the `rudder://` scheme, so with both installed a
  `rudder://pair` link may open the old one. Testers delete the old app.
- The old app gets no more builds; TestFlight builds expire 90 days after
  upload. Its record on the personal team is left alone unless the user asks
  to remove it.
- The TestFlight join link in `site/index.html`, `README.md` and
  `cloud/app/tutorial.js` (`PHONE_APP_URL`) is the old app's. Replace it with
  the App Store link once the app is live.

## Capabilities

The app has no entitlements today. To add one (push, associated domains,
app groups, Sign in with Apple), create `ios/App/Rudder.entitlements`, set
`CODE_SIGN_ENTITLEMENTS: App/Rudder.entitlements` on the Rudder target and
run `identity`: it enables the matching capability on the bundle id
(`ENTITLEMENT_CAPABILITIES` in `appstore.mjs`), and Xcode's cloud signing
puts it in the profile at upload. Side work per capability:

- Push (`aps-environment`): an APNs key (.p8) of team `P76SY8RFGL`, topic
  `ai.exla.rudder`, on the server sending pushes.
- Associated domains (`applinks:rudder.exla.ai`): serve
  `/.well-known/apple-app-site-association` naming `P76SY8RFGL.ai.exla.rudder`.
- App groups and keychain access groups: prefix them with the team id.
- CarPlay (`com.apple.developer.carplay-*`): Apple grants it to the team on
  request (developer.apple.com/contact/carplay, by the Account Holder);
  `identity` cannot enable it, and the upload fails until Apple has.

## Version

The iPhone app's version is Rudder's: `npm version` writes
`MARKETING_VERSION` and the build number (5.1.0 is 5001000) into
`ios/project.yml`. Never set them by hand. App Store Connect takes each build
number once, so one Rudder version gives at most one iPhone build: a second
upload needs a new Rudder release. Run every step with `--ref` at the release
tag (or `main` while `package.json` still names that version), so the
checkout's version is the build's. The iPhone app ships when the native shell
changed; web app changes reach it through Cloud with no new build.

## First publication

1. The user creates the app record (Apple's API cannot, for any key). On
   their own Mac: `ios/scripts/create-app.sh <their Apple ID>`, which runs
   fastlane produce and asks for their password and two-factor code there.
   Never sign in as the user from an agent. Or by hand in App Store Connect:
   Apps, +, New App; platform iOS, name Rudder, primary language English
   (U.S.), bundle id `ai.exla.rudder`, SKU `rudder-ios`, user access Full
   Access. `identity` fails with these fields until it exists.
2. The user, in App Store Connect once: App Privacy (answers below), age
   rating (every content question None or No, unrestricted web access No,
   giving 4+), Pricing and Availability (Free), and a TestFlight internal
   group with access to all builds.
3. Demo account: a Google account made for App Review with a Rudder account,
   a running cloud machine, two or three projects with agents that have a
   question and a plan waiting, and Claude Code signed in. Set
   `gh secret set RUDDER_DEMO_TOKEN` (its `rdr_` token) and
   `gh secret set APPSTORE_REVIEW < review.json` (JSON: `contactFirstName`,
   `contactLastName`, `contactPhone`, `contactEmail`, `demoAccountRequired`,
   `demoAccountName`, `demoAccountPassword`). Neither is ever printed.
4. Screenshots: `scripts/ios-qa.sh /workspace/rudder-scratch/<task>/shots main appstore`
   (the rudder-ios-qa skill). Look at every image: they are public. Copy the
   good ones to `ios/appstore/screenshots/<display type>/NN-name.jpg`; files
   upload in name order, 1 to 10 per set. Commit.
5. `upload`, then `prepare`, then the checks below, then `submit`.
6. After approval: `release` (or Release This Version in App Store Connect).

## Updates

New native code: a Rudder release, then `upload`, edit `whats-new.txt`,
`prepare`, checks, `submit`, `release`. Listing text or screenshots only: edit
`ios/appstore`, commit, `prepare` against the version being prepared. A live
version's listing changes only with a new version and build; promotional
text is the exception, and the user edits it on the live version in App
Store Connect (`prepare` only touches the version being prepared).

## Before submitting

- `check` passes and `status` shows the build `VALID` and the version
  `PREPARE_FOR_SUBMISSION`.
- The build was opened on a real iPhone from TestFlight: sign in, a chat,
  attach, Talk to Rudder.
- The demo account signs in on a fresh device and its machine is running.
  Keep it running until the review ends.
- The support and privacy URLs in `listing.json` load.
- `review-notes.txt` says how to sign in and where each native feature is.

## When App Review answers

Apple emails the account and the message is in App Store Connect, App Review
(not in the API: ask the user to paste it). The user replies there. The
first line names a guideline:

- 2.1 (could not sign in, crash, incomplete): fix the demo account or the bug.
- 4.2 (minimum functionality): reply with the native features from the notes.
- 4.8 (login services), 5.1.1(v) (account deletion), 5.1.2 (data sent to
  third-party AI): product changes; see the open risks in `docs/v4-handoff.md`.
- 2.5.4 (background audio): point to Talk to Rudder in the notes.

Metadata only (`METADATA_REJECTED`): fix `ios/appstore`, `prepare`, `submit`.
A binary change: new Rudder release, `upload`, `prepare`, `submit`; `submit`
reuses the submission the rejection left open.

## App Privacy answers

Match `ios/App/PrivacyInfo.xcprivacy`. No tracking. Linked to the user, for
app functionality: name, email address, user ID, photos or videos, audio
data, other user content. Linked, for analytics: product interaction, other
diagnostic data, user ID. Not linked, for analytics: device ID. Change the
manifest and these answers together.

## Secrets

`IOS_ASC_KEY_ID`, `IOS_ASC_ISSUER_ID`, `IOS_ASC_KEY_P8` (team `P76SY8RFGL`,
the raw .p8 text; Admin role: Xcode's cloud-managed distribution signing
needs it, so no certificate or profile is stored), `OFFSTAGE_API_KEY`,
`RUDDER_DEMO_TOKEN`, `APPSTORE_REVIEW`. Set them with
`gh secret set NAME < file`, never by pasting key material into a chat.
