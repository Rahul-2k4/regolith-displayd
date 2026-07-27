#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
expected_manifest="$repo_dir/Cargo.toml"

build_plan=$(make -f "$repo_dir/Makefile" -n build)
clean_plan=$(make -f "$repo_dir/Makefile" -n clean)

printf '%s\n' "$build_plan" | grep -F "cargo build --release --manifest-path \"$expected_manifest\""
printf '%s\n' "$clean_plan" | grep -F "rm -rf debian/tmp_files/.cargo"
printf '%s\n' "$clean_plan" | grep -F "cargo clean --manifest-path \"$expected_manifest\""
