# List the recipes
default:
    @just --list

# Unit tests, lint and format check
test:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# The end-to-end test, which needs act and Docker
e2e:
    cargo test --test e2e -- --ignored
    cargo test --test shard_e2e -- --ignored
    cargo test --test retry_e2e -- --ignored

# Scan the whole git history for secrets, as the gitleaks job in ci.yml does
leaks:
    gitleaks git --redact --no-banner .
