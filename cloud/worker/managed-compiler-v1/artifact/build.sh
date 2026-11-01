#!/bin/sh
# Root-run local/explicit builder only. Does not publish or touch managed homes.
set -eu
if [ "$#" -ne 2 ]; then
  echo 'usage: build.sh <explicit-local-tag> <fresh-/workspace-image-id-file>' >&2
  exit 64
fi
compiler_tag=$1
compiler_iid=$2
case "$compiler_tag" in ''|-*) echo 'explicit local image tag required' >&2; exit 64;; esac
case "$compiler_iid" in /workspace/*) ;; *) echo 'image-id file must be under /workspace' >&2; exit 64;; esac
case "$compiler_iid" in */../*|*/./*|*//*) echo 'image-id path must be normalized' >&2; exit 64;; esac
test ! -e "$compiler_iid"
test ! -L "$compiler_iid"
compiler_context=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec docker buildx build --platform linux/amd64 --load \
  --file "$compiler_context/Dockerfile" --target compiler-artifact \
  --tag "$compiler_tag" --iidfile "$compiler_iid" "$compiler_context"
