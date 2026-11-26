#!/usr/bin/env bash
# The iPhone click-through, or the App Store screenshots, from this machine:
# starts the "iOS click-through" workflow (an Offstage Mac does the work),
# waits, and opens the sealed output into the directory given.
# Usage: scripts/ios-qa.sh <out-dir> [git ref] [clickthrough|appstore]
set -euo pipefail
out="${1:?usage: scripts/ios-qa.sh <out-dir> [git ref] [clickthrough|appstore]}"
ref="${2:-main}"
pass="${3:-clickthrough}"
key="$(mktemp -d)"
trap 'rm -rf "$key"' EXIT
openssl genrsa -out "$key/private.pem" 3072 2>/dev/null
openssl rsa -in "$key/private.pem" -pubout -out "$key/public.pem" 2>/dev/null
started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
gh workflow run ios-qa.yml --ref "$ref" -f public_key="$(base64 -w0 "$key/public.pem")" -f pass="$pass"
run=""
for _ in $(seq 30); do
  sleep 4
  run=$(gh run list --workflow ios-qa.yml --event workflow_dispatch --created ">=$started" --json databaseId --jq '.[0].databaseId')
  [[ -n "$run" ]] && break
done
[[ -n "$run" ]] || { echo "the run did not appear"; exit 1; }
echo "run $run"
code=0
gh run watch "$run" --exit-status --interval 20 > /dev/null || code=$?
gh run view "$run" --log 2>/dev/null | grep -E "offstage: |screenshots|failed: " | sed 's/^.*Z //' || true
mkdir -p "$out"
gh run download "$run" --name ios-qa-sealed --dir "$key/sealed"
openssl pkeyutl -decrypt -inkey "$key/private.pem" -in "$key/sealed/pass.enc" -out "$key/pass"
openssl enc -d -aes-256-cbc -pbkdf2 -pass "file:$key/pass" -in "$key/sealed/out.tgz.enc" | tar -xzf - -C "$out" --strip-components=1
echo "output in $out"
exit "$code"
