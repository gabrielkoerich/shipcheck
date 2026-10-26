# shipcheck

[![CI](https://github.com/gabrielkoerich/shipcheck/actions/workflows/ci.yml/badge.svg)](https://github.com/gabrielkoerich/shipcheck/actions/workflows/ci.yml) [![Release](https://img.shields.io/github/v/release/gabrielkoerich/shipcheck)](https://github.com/gabrielkoerich/shipcheck/releases/latest) [![Homebrew](https://img.shields.io/badge/homebrew-gabrielkoerich%2Ftap-orange)](https://github.com/gabrielkoerich/homebrew-tap) [![crates.io](https://img.shields.io/crates/v/shipcheck)](https://crates.io/crates/shipcheck) [![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Your agent already ran the tests. shipcheck runs your repository's checks on your machine, keeps the result for that exact git tree, and posts the required checks, one per job, from its GitHub App. A PR whose tree was already tested gets its checks in under a second.

It has two modes:

- Host mode, which `shipcheck init` writes when it finds your checks, runs the commands of your workflow jobs, such as `cargo test` or `npm test`, natively in an OS sandbox. On a machine set up like production, with the same tools, dependencies, OS and system libraries, that tests the code where it will run.
- act mode, the default without `runner`, runs your GitHub Actions workflows in containers with act. It is the mode closest to GitHub's CI.

`runner` is the mode of every job, and a `[jobs]` table moves one job to the other mode in the same run, such as a `ci.yml/e2e` job that needs Docker under act beside the host checks.

Measured on an idle Apple silicon Mac against GitHub's hosted runners, same commits: a large Rust app in act took 105 s warm against 115 to 124 s, and a tree already tested answers in under a second. A first run that starts cold can take longer than GitHub: the same app took 238 s against 116 s. [Speed](https://shipcheck.run/docs/speed/) has the settings that make runs fast.

Docs: https://shipcheck.run/docs/.

## Install

With Homebrew, which also installs act, git and the Docker client:

```sh
brew install gabrielkoerich/tap/shipcheck
```

Or with the install script:

```sh
curl -fsSL https://shipcheck.run/install.sh | sh
```

Or with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), which downloads the same signed release binary, no Rust needed:

```sh
cargo binstall shipcheck
```

Or build it from source with cargo, Rust 1.89 or later: from crates.io, from this repository, or from a clone. A build gets a new signature each time, so macOS asks once per keychain item after it:

```sh
cargo install shipcheck
cargo install --git https://github.com/gabrielkoerich/shipcheck shipcheck
git clone https://github.com/gabrielkoerich/shipcheck && cargo install --path shipcheck
```

| Needs | When | Why |
|---|---|---|
| git | Always | shipcheck reads commits, trees and `main` from your clone |
| An OS sandbox | Host mode | macOS has `sandbox-exec`. On Linux, install bubblewrap |
| Docker | act mode | act runs each job in a container. Docker Desktop is recommended. OrbStack and colima are tested, with limits in the install docs |
| [act](https://github.com/nektos/act) 0.2.86 or later | act mode | Runs the workflows. `brew install act` |
| A copy-on-write filesystem | Recommended | APFS on macOS, btrfs or XFS on Linux, so a warm checkout costs no disk. On other filesystems shipcheck falls back to a plain git worktree |

## Set up a repository

1. Install the binary, above.
2. Run `shipcheck init` in a clone of the repository, on `main`. An agent or a script runs `shipcheck init --yes`. Without a terminal and without `--yes`, `init` only prints its plan.
3. Commit the `shipcheck.toml` it writes and merge it to `main`.
4. Install the [shipcheck App](https://github.com/apps/shipcheck-run) on the repository. `init` prints this link.

[The quickstart](https://shipcheck.run/docs/quickstart/) says what each step does. Each other clone needs `shipcheck install` once. In a terminal, `init` and `install` sign the machine in when it cannot post yet. A machine with no person at it gets a token from `shipcheck token` instead ([unattended machines](https://shipcheck.run/docs/install/#unattended-machines)).

shipcheck then posts one check per job, such as `shipcheck/test`, as Actions does. Those are the checks branch protection requires. A `shipcheck` check beside them sums up the run. A team can post from [its own App](https://shipcheck.run/docs/install/#your-own-app) instead.

Without the App, `shipcheck run`, the result cache, the git hooks, `shipcheck status` and `shipcheck logs` still work on any repository.

What a green check proves and what a job can reach: [proof](https://shipcheck.run/docs/proof/), [security](https://shipcheck.run/docs/security/).

## Layout

| Folder | Holds |
|---|---|
| `src/` | The `shipcheck` CLI and daemon |
| `tests/` | Integration and end-to-end tests |
| `Formula/` | The Homebrew formula template, filled in by the `release` job of `ci.yml` |

## License

MIT
