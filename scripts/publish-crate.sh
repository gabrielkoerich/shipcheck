#!/usr/bin/env bash
# Publish first: a tag without a crate is worse than a crate without a tag, and a rerun then tags it.
# Permanent: a version can be yanked, never deleted, and it publishes the source
set -e
cargo publish --locked --allow-dirty
