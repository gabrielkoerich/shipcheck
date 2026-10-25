# Contributing to shipcheck

Rules for anyone changing shipcheck, people and agents alike. shipcheck turns a local run into a PR's required checks, so each rule keeps one thing true: a check passes only when the work behind it ran and passed.

## Invariants

Every change must keep these.

- A result comes only from a run shipcheck started. Nothing accepts a result, a log or a verdict from a caller.
- `shipcheck.toml` and `.github/workflows/` are read from the base branch. Every command finds it in one order: this checkout's daemon entry, then the remote's default branch as `git ls-remote --symref origin HEAD` reports it, then `origin/main` with a warning. The hooks never ask the remote, so they skip the second step. init refuses when it gets no answer, and nothing posts from the `origin/main` fallback. They are never read from the PR tree, the working copy or a local ref such as `origin/HEAD`.
- Every job gets a new copy-on-write clone checked out at the SHA, at its slot's fixed path for that job. Jobs never share a checkout, and never run in the agent's worktree.
- A job never reaches the host: no Docker socket, no host token or environment, a `.git` of its own, no network unless `network = "internet"` on main, `-self-hosted` only inside a VM shipcheck created for that job. The isolation case of the end-to-end test proves it, and must keep passing. The `colima` engine's VM still reaches the host's loopback and the internet, so it is for trusted repositories only, and doctor says so.
- A workflow passes only on exit code 0 from an `act` process shipcheck started, with every started job ending in `success`. A host check passes only on exit code 0 from its command, which shipcheck started. The cache is written only from such a run.
- The cache key holds the tree, the config from `main`, the workflows as they run, the act and shipcheck versions and the images. Never key by commit SHA.
- A `paths` skip is never cached, and any doubt about the changed files means run.
- Hooks never block or fail git. Runs they start go to the background, one per branch, and a newer commit cancels the older one.
- The daemon never runs a fork's PR, and only handles PRs into its configured branch.
- The `shipcheck` check and the job checks beside it are posted only with the App's installation token. Log tails follow `logs` on main, `all` by default, a failed job always gets its tail, known secrets are masked, and a fork's PR never gets one.
- Anything unexpected is a failure: a missing job, a timeout, a parse error, an unsupported platform.

## Code

- Opinionated defaults. Setup is: install the binary, `shipcheck init`, merge `shipcheck.toml`, install the App. Every option has a default that works, and none is required. A change that adds a step, a question or a required option to that path needs a reason in its PR. Otherwise it gets a default.
- Rust, one crate with one binary, `shipcheck`. The daemon is `shipcheck daemon`.
- `SKILL.md` at the root tells agents how to use shipcheck. Keep it true to the CLI. https://shipcheck.run serves it.
- `Formula/shipcheck.rb` is the Homebrew template. Each merge to `main` is a release: the `release` job of `ci.yml` fills in versions and checksums and pushes the formula to `gabrielkoerich/homebrew-tap`.
- Call `act` and `git` as processes. Do not embed or reimplement them.
- The cache key needs a test with fixed inputs and a fixed expected digest.
- Test `act` behaviour against a real workflow before claiming it works.

