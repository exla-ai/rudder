#!/bin/sh
# Root-run local/explicit builder only. Does not publish or touch managed homes.
set -eu
if [ "$#" -ne 2 ]; then
  echo 'usage: build.sh <explicit-local-tag> <fresh-/workspace-image-id-file>' >&2
  exit 64
fi
compiler_c1_tag=$1
compiler_c1_iid=$2
case "$compiler_c1_tag" in ''|-*) echo 'explicit local image tag required' >&2; exit 64;; esac
case "$compiler_c1_iid" in /workspace/*) ;; *) echo 'image-id file must be under /workspace' >&2; exit 64;; esac
case "$compiler_c1_iid" in */../*|*/./*|*//*) echo 'image-id path must be normalized' >&2; exit 64;; esac
test ! -e "$compiler_c1_iid"
test ! -L "$compiler_c1_iid"
compiler_c1_context=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec docker buildx build --platform linux/amd64 --load \
  --file "$compiler_c1_context/Dockerfile" --target compiler-artifact \
  --tag "$compiler_c1_tag" --iidfile "$compiler_c1_iid" "$compiler_c1_context"
