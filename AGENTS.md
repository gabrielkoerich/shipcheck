# Contributing to shipcheck

Rules for anyone changing shipcheck, people and agents alike. [docs/DESIGN.md](docs/DESIGN.md) says why it is built this way.

## Invariants

Every change must keep these. Each one closes a cheat listed in `docs/PROOF.md`.

- A result comes only from a run shipcheck started. Nothing accepts a result, a log or a verdict from a caller.
- `shipcheck.toml` and `.github/workflows/` are read from `origin/main`, never from the PR tree or the working copy.
- Every job gets a new copy-on-write clone checked out at the SHA, at its slot's fixed path for that job. Jobs never share a checkout, and never run in the agent's worktree.
- A job never reaches the host: no Docker socket, no host token or environment, a `.git` of its own, no network unless `network = "internet"` on main, never `-self-hosted`. The isolation case of the end-to-end test proves it, and must keep passing.
- A workflow passes only on exit code 0 from an `act` process shipcheck started, with every started job ending in `success`. The cache is written only from such a run.
- The cache key holds the tree, the config from `main`, the workflows as they run, the act and shipcheck versions and the images (PROOF.md). Never key by commit SHA.
- A `paths` skip is never cached, and any doubt about the changed files means run.
- Hooks never block or fail git. Runs they start go to the background, one per branch, and a newer commit cancels the older one.
- The daemon never runs a fork's PR, and only handles PRs into its configured branch.
- The `shipcheck` check is posted only with the App's installation token, and log tails go into it only on request.
- Anything unexpected is a failure: a missing job, a timeout, a parse error, an unsupported platform.

## Code

- Rust, one crate with one binary, `shipcheck`. The daemon is `shipcheck daemon`. Keep the dependency list in `docs/ARCHITECTURE.md` current.
- User docs live in `site/content/docs`. Update them in the same commit as a change users can see.
- `SKILL.md` at the root tells agents how to use shipcheck. Keep it true to the CLI. The site serves it, a `.md` copy of every docs page, `llms.txt` and `llms-full.txt`, all written by `site/scripts/build-agent-files.py` after `zola build`.
- `Formula/shipcheck.rb` is the Homebrew template. The release workflow fills in versions and checksums and pushes it to `gabrielkoerich/homebrew-tap`.
- Call `act` and `git` as processes. Do not embed or reimplement them.
- The cache key needs a test with fixed inputs and a fixed expected digest.
- Test `act` behaviour against a real workflow before claiming it works.

