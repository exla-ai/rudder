---
name: rudder-burst
description: Use on a Rudder Cloud home (when `rudder-burst` is on PATH) before running a heavy command such as a release build, a full test suite, a large cargo/npm/pnpm/go/bazel build, or anything that pegs the CPU for minutes. Runs the command on a dedicated 16 CPU builder machine from the home's burst build pool instead of on the shared home, with the same exit code and streamed output; without a pool it runs the command on the home. Not for quick commands, interactive programs, or work that needs services running on the home.
---

# Heavy builds on the burst build pool

The home machine is shared by every agent on the account. A release build or a
full test suite there slows all of them down and can run the machine out of
memory. `rudder-burst` runs the command on a builder machine instead: a
dedicated machine (16 CPU, 32 GB by default) with its own warm caches, leased
for the length of the command.

Only use it when `command -v rudder-burst` succeeds. Elsewhere (a laptop, a
plain VM), run the command normally.

## Run a command

From anywhere inside the repository:

```sh
rudder-burst cargo build --release
rudder-burst npm test
rudder-burst 'cargo test --workspace && npm run build'
```

It uploads the repository (only what changed since the last run; `.gitignore`d
paths such as `target/` and `node_modules/` stay home and are never deleted on
the builder, so its build outputs stay warm), runs the command in the same
directory relative to the repo root, streams stdout and stderr, and exits with
the command's exit code. Ctrl-C stops the remote command.

## Bring outputs home

Nothing comes back unless asked for. Use `--pull` with a path relative to the
repo root, as often as needed:

```sh
rudder-burst --pull target/release/rudder-native cargo build --release
rudder-burst --pull dist/ npm run build
```

A trailing slash copies a directory's contents into the same directory at home.

## Options worth knowing

- `--env NAME=VALUE`: environment for the command (the home's environment is
  not copied; secrets the build needs must be passed explicitly).
- `--git`: also upload `.git` (left out by default) for builds that run git.
- `--minutes N`: lease length, renewed while the command runs (default 10).
- `rudder-burst status`: the home's builders and who holds them.
- `rudder-burst clean`: delete this repo's build directory on a builder, for a
  corrupted cache.

## When there is no builder

If the home has no build pool (it is off, or the home runs on its own host
outside Fly) or every builder stays busy past `--wait` (default 10 minutes),
`rudder-burst` says so on stderr ("running here instead") and runs the command
on the home with the same output and exit code. There is no need to retry or
to run it again yourself.
- A cold start (first build in a while) takes a minute while the machine
  starts; the first build ever on a builder also installs Rust and Go.
