#!/usr/bin/env bash
# Writes formula.rb from Formula/shipcheck.rb for VERSION, with the archives in artifacts/
set -e
VERSION="$1"
base="https://github.com/${GITHUB_REPOSITORY}/releases/download/${VERSION}"
subs=(-e "s|^  version \"0.0.0\"|  version \"${VERSION#v}\"|")

# Each platform gets the archive built for it, with the checksum of that archive
for pair in \
  "MACOS_ARM:aarch64-apple-darwin" \
  "MACOS_INTEL:x86_64-apple-darwin" \
  "LINUX_ARM:aarch64-unknown-linux-gnu" \
  "LINUX_INTEL:x86_64-unknown-linux-gnu"; do
  key="${pair%%:*}"; target="${pair#*:}"
  file="artifacts/shipcheck-${target}.tar.gz"
  [ -f "$file" ] || { echo "::error::missing $file"; exit 1; }
  sum=$(sha256sum "$file" | awk '{print $1}')
  subs+=(-e "s|URL_${key}\"|${base}/shipcheck-${target}.tar.gz\"|" -e "s|SHA_${key}\"|${sum}\"|")
done
sed "${subs[@]}" Formula/shipcheck.rb > formula.rb

if grep -q 'URL_\|SHA_' formula.rb; then
  echo "::error::a placeholder was left behind"
  exit 1
fi
