#!/usr/bin/env bash
set -euo pipefail

bin=${1:?usage: check-binary.sh <binary> <reported output> <expected output> [build path...]}
out=${2:?usage: check-binary.sh <binary> <reported output> <expected output> [build path...]}
expected=${3:?usage: check-binary.sh <binary> <reported output> <expected output> [build path...]}
shift 3

if [ "$out" != "$expected" ]; then
  echo "::error::$bin reports '$out', expected '$expected'"
  exit 1
fi
echo "$bin reports '$out'"

paths=(/Users/ /home/runner/work/fastmock/ /home/runner/.cargo/ /home/runner/.rustup/)
for p in "$@"; do
  if [ -n "$p" ]; then
    paths+=("$p")
  fi
done
patterns=()
for p in "${paths[@]}"; do
  patterns+=(-e "$p")
done
found=$(strings -n 6 "$bin" | grep -F "${patterns[@]}" | head -n 20 || true)
if [ -n "$found" ]; then
  echo "::error::$bin contains local build paths"
  printf '%s\n' "$found"
  exit 1
fi
echo "$bin contains none of: ${paths[*]}"
