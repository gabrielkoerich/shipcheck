#!/usr/bin/env bash
# Publish first, a tag without a crate is worse than a crate without a tag. A version crates.io
# already has is skipped, so a run cancelled after the publish has its rerun go on to the tag
set -e
v=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
code=$(curl -sS -o /dev/null -w '%{http_code}' -A shipcheck-release "https://crates.io/api/v1/crates/shipcheck/$v")
case "$code" in
  200) echo "shipcheck $v is already on crates.io, skipping the publish" ;;
  404) cargo publish --locked --allow-dirty ;;
  *) echo "::error::crates.io answered $code for shipcheck $v"; exit 1 ;;
esac
