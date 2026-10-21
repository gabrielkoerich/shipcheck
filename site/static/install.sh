#!/bin/sh
# Installs the latest shipcheck release: curl -fsSL https://shipcheck.run/install.sh | sh
set -eu
# Everything runs from main, called on the last line, so a download cut short runs nothing

base="https://github.com/gabrielkoerich/shipcheck/releases/latest/download"
dir="${SHIPCHECK_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    echo "shipcheck install: $*" >&2
    exit 1
}

main() {
    case "$(uname -s)" in
        Darwin)
            os=apple-darwin
            sum="shasum -a 256"
            ;;
        Linux)
            os=unknown-linux-gnu
            sum="sha256sum"
            ;;
        *)
            fail "unsupported OS $(uname -s), build from source with cargo install shipcheck"
            ;;
    esac

    case "$(uname -m)" in
        arm64 | aarch64)
            arch=aarch64
            ;;
        x86_64 | amd64)
            arch=x86_64
            ;;
        *)
            fail "unsupported architecture $(uname -m), build from source with cargo install shipcheck"
            ;;
    esac

    archive="shipcheck-$arch-$os.tar.gz"
    tmp="$(mktemp -d)"
    [ -n "$tmp" ] && [ -d "$tmp" ] || fail "could not create a temporary directory"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    echo "Downloading $archive"
    curl -fsSL -o "$tmp/$archive" "$base/$archive" || fail "could not download $base/$archive"
    curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "could not download $base/SHA256SUMS"

    expected="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
    [ -n "$expected" ] || fail "$archive is not in SHA256SUMS"
    actual="$(cd "$tmp" && $sum "$archive" | awk '{ print $1 }')"
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $archive, expected $expected, got $actual"

    tar -xzf "$tmp/$archive" -C "$tmp"
    mkdir -p "$dir"
    # Install through a temporary name and mv, so a running shipcheck keeps its old binary
    cp "$tmp/shipcheck" "$dir/.shipcheck.new"
    chmod 755 "$dir/.shipcheck.new"
    mv -f "$dir/.shipcheck.new" "$dir/shipcheck"
    echo "Installed shipcheck to $dir/shipcheck"

    case ":$PATH:" in
        *":$dir:"*) ;;
        *) echo "Note: $dir is not on your PATH. Add it to your shell profile." ;;
    esac

    echo "shipcheck needs act 0.2.89 or later and Docker to run workflows: https://shipcheck.run/docs/install/"
}

main "$@"
