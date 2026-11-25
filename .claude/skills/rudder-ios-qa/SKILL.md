---
name: rudder-ios-qa
description: Click through the real Rudder iPhone app on an Offstage Mac's simulator, signed in to a real account, and bring back a video, screenshots and the accessibility tree of every screen. Use when asked to test, QA, click through, record or screenshot the iPhone app, to check how a change looks in the native app, or before a TestFlight build.
---

# iPhone click-through on Offstage

The app runs in an iPhone simulator on an Offstage Mac. A UI test taps
through `ios/UITests/steps.json`, the simulator's screen is recorded, and the
output comes back to this machine. One run takes about twenty minutes.

## Run it

```sh
scripts/ios-qa.sh /workspace/rudder-scratch/<task>/ios-qa        # main as pushed
scripts/ios-qa.sh /workspace/rudder-scratch/<task>/ios-qa <ref>  # another pushed ref
```

It tests what is pushed to GitHub, not the working copy: push first. The app
loads the live web app from the control plane, so a web change shows up once
Cloud is deployed, with no new native build.

The output directory holds:

- `clickthrough.mp4`: the whole pass, raw. It runs about 13 minutes and
  300 MB because the test waits between taps. Before sending it to the user,
  speed it up and shrink it (about 2 MB), then send that with SendUserFile:
  `ffmpeg -i clickthrough.mp4 -vf "setpts=PTS/4,scale=-2:1280,fps=30" -an -c:v libx264 -crf 26 -pix_fmt yuv420p -movflags +faststart short.mp4`
  (no ffmpeg on PATH here: `find ~/.cache -name 'ffmpeg-linux-x86_64*'`).
- `shots/NN-name.png` and `shots/NN-name.txt`: each "shot" step's screenshot
  and accessibility tree. Read the PNGs yourself and report what looks wrong.
- `shots/log.txt`: one line per step. `MISS` means no element had that label.
- `build.log`, `test.log`: xcodebuild's output.

The script exits non-zero when the build fails or a step missed. The output is
still there to read.

## Change what it clicks

Edit `ios/UITests/steps.json`, push, run again. Steps:

| Step | Does |
|---|---|
| `{"waitFor": "Skip", "timeout": 60}` | waits for an element whose label holds the text |
| `{"tap": "Settings"}` | taps the first element with that label (exact, else containing it) |
| `{"tapAt": [0.5, 0.9]}` | taps a point, as fractions of the screen |
| `{"type": "text"}` | types into the focused field |
| `{"swipe": "up"}` | swipes up, down, left or right |
| `{"wait": 2}` | seconds |
| `{"shot": "home"}` | screenshot and tree |

Labels are what VoiceOver reads: a button's text or its `aria-label` in
`cloud/app/`. When a tap misses, open the nearest shot's `.txt` and take the
label from there.

The pass is signed in to a real account. Look, do not act: never add a step
that sends a message, starts or deletes an agent, changes a setting or signs
out. Typing a draft and leaving it unsent is fine.

## How it is wired

- `ios/UITests/ClickThrough.swift` is the test; `ios/scripts/qa.sh` runs on
  the Mac (build, run, collect); `.offstage/ios-qa.mjs` drives the Offstage
  sandbox and mints a ten-minute pair code just before the run stage, so the
  account's token never reaches the Mac.
- `.github/workflows/ios-qa.yml` runs the driver with the repository's
  `OFFSTAGE_API_KEY` and `RUDDER_QA_TOKEN` secrets. Its Linux runner only
  calls the Offstage API. The repository is public, so the workflow encrypts
  the output to a one-time key that `scripts/ios-qa.sh` makes; the public log
  shows counts only. Keep it that way: never print labels, trees or
  screenshots to the log, and never upload the output unencrypted.
- `RUDDER_QA_TOKEN` is the token in `~/.rudder-qa-token`. If the run stage
  fails with "pair/start answered 401", make a new token (the phone-visual-qa
  memory has the steps) and `gh secret set RUDDER_QA_TOKEN < ~/.rudder-qa-token`.
- With an Offstage key on this machine the driver also runs directly:
  `OFFSTAGE_API_KEY=... RUDDER_QA_TOKEN="$(cat ~/.rudder-qa-token)" node .offstage/ios-qa.mjs`
  (install the SDK into `.offstage` first, as the workflow does).

For the web app at phone width without a Mac, `scripts/phone-qa.mjs` is
faster; this skill is for the native shell and real WebKit.
