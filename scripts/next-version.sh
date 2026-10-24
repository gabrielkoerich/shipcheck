#!/usr/bin/env bash
# Prints last_tag, skip and version as key=value lines, to $GITHUB_OUTPUT when set
set -e
out="${GITHUB_OUTPUT:-/dev/stdout}"

LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "")
echo "last_tag=$LAST_TAG" >> "$out"

if [ -n "$LAST_TAG" ]; then
  COMMIT_COUNT=$(git rev-list "$LAST_TAG"..HEAD --count)
  COMMITS=$(git log "$LAST_TAG"..HEAD --pretty=format:"%s")
else
  COMMIT_COUNT=$(git rev-list HEAD --count)
  COMMITS=$(git log --pretty=format:"%s")
fi

if [ "$COMMIT_COUNT" -eq 0 ]; then
  echo "No new commits since $LAST_TAG, skipping release." >&2
  echo "skip=true" >> "$out"
  exit 0
fi

# Skip release if ALL commits are chore/docs-only
NON_RELEASE=$(echo "$COMMITS" | grep -cvE '^(chore|docs)(\(.*\))?:' || true)
if [ "$NON_RELEASE" -eq 0 ]; then
  echo "All $COMMIT_COUNT commits are chore/docs, skipping release." >&2
  echo "skip=true" >> "$out"
  exit 0
fi

echo "$COMMIT_COUNT new commits since ${LAST_TAG:-beginning} ($NON_RELEASE release-worthy)" >&2
echo "skip=false" >> "$out"

# BREAKING CHANGE: or breaking: is major, feat: is minor, everything else is patch
BUMP="patch"
if echo "$COMMITS" | grep -qE '^feat(\(.*\))?:'; then
  BUMP="minor"
fi
if echo "$COMMITS" | grep -qE '^BREAKING CHANGE:|^breaking(\(.*\))?:'; then
  BUMP="major"
fi

# Before the first tag the base is the version Cargo.toml already carries
CURRENT="${LAST_TAG#v}"
if [ -z "$CURRENT" ]; then
  CURRENT=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
fi
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
case "$BUMP" in
  major) MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  minor) MINOR=$((MINOR + 1)); PATCH=0 ;;
  patch) PATCH=$((PATCH + 1)) ;;
esac
NEW_VERSION="v${MAJOR}.${MINOR}.${PATCH}"

# A run that tagged and then failed leaves a tag that `describe` may not see from here
while git rev-parse "$NEW_VERSION" >/dev/null 2>&1; do
  CURRENT="${NEW_VERSION#v}"
  IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
  NEW_VERSION="v${MAJOR}.${MINOR}.$((PATCH + 1))"
  echo "tag was taken, trying $NEW_VERSION" >&2
done

echo "version=$NEW_VERSION" >> "$out"
echo "Version: $NEW_VERSION (${BUMP} bump)" >&2
