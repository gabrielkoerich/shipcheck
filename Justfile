# List the recipes
default:
    @just --list

# Serve the site with live reload on port 1177, since 1111 is taken by the author's blog
serve port="1177":
    zola --root site serve --port {{ port }}

# Build the site into site/public with the agent files: .md copies, SKILL.md, llms.txt, llms-full.txt
build:
    zola --root site build
    python3 site/scripts/build-agent-files.py site/public

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

# Scan the whole git history for secrets, as the leaks workflow does
leaks:
    gitleaks git --redact --no-banner .

# Bring the public gabrielkoerich/shipcheck up to main through the split, as one normal commit
sync-public ref="main":
    scripts/sync-public.sh {{ ref }}
