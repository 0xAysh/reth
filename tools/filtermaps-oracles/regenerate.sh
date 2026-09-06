#!/usr/bin/env bash
# Run from any working directory. Requires an existing clean, pinned Geth checkout.
set -euo pipefail

if [[ $# == 2 ]]; then
  mode=all
  geth_arg=$1
  reth_arg=$2
elif [[ $# == 3 ]]; then
  mode=$1
  geth_arg=$2
  reth_arg=$3
else
  echo "usage: $0 [mapping|stream|all] <geth-checkout> <reth-checkout>" >&2
  exit 2
fi
case "$mode" in
  mapping|stream|all) ;;
  *) echo "unknown generation mode: $mode" >&2; exit 2 ;;
esac

source_dir=$(cd -- "$(dirname -- "$0")" && pwd)
geth=$(cd -- "$geth_arg" && pwd)
reth=$(cd -- "$reth_arg" && pwd)
pin=af7c0fd8ee09de71b1034dbe6d1112556b49b59f
[[ $(git -C "$geth" rev-parse HEAD) == "$pin" ]] || { echo "Geth must be at $pin" >&2; exit 1; }
[[ -z $(git -C "$geth" status --porcelain) ]] || { echo "Geth checkout must be clean" >&2; exit 1; }
export ORACLE_REV
ORACLE_REV=$(git -C "$source_dir" rev-parse HEAD)
[[ -z $(git -C "$source_dir" status --porcelain -- .) ]] || { echo "Commit the oracle folder before generating pinned output" >&2; exit 1; }
[[ -f "$reth/crates/filter-maps/Cargo.toml" ]] || { echo "Reth target must contain crates/filter-maps" >&2; exit 1; }

copied=()
cleanup() {
  if ((${#copied[@]})); then
    rm -f -- "${copied[@]}"
  fi
}
trap cleanup EXIT

run=''
if [[ $mode == mapping || $mode == all ]]; then
  mapping="$geth/core/filtermaps/gen_golden_test.go"
  [[ ! -e "$mapping" ]] || { echo "Refusing to overwrite $mapping" >&2; exit 1; }
  cp -- "$source_dir/mapping/gen_golden_test.go" "$mapping"
  copied+=("$mapping")
  export GOLDEN_OUT="$reth/crates/filter-maps/tests/it/golden/vectors.rs"
  mkdir -p -- "$(dirname -- "$GOLDEN_OUT")"
  run='TestGenGolden'
fi
if [[ $mode == stream || $mode == all ]]; then
  stream="$geth/core/filtermaps/gen_stream_test.go"
  [[ ! -e "$stream" ]] || { echo "Refusing to overwrite $stream" >&2; exit 1; }
  cp -- "$source_dir/stream/gen_stream_test.go" "$stream"
  copied+=("$stream")
  export STREAM_OUT="$reth/crates/filter-maps/tests/it/golden_stream/fixtures"
  mkdir -p -- "$STREAM_OUT"
  run=${run:+$run|}TestGenStream
fi

(cd -- "$geth" && go test ./core/filtermaps -run "^($run)$" -count=1 -v)
(cd -- "$reth" && cargo +nightly fmt -p reth-filter-maps)
