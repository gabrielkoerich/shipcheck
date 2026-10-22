# shipcheck

Your agent already ran the tests. shipcheck runs your repository's checks on your machine, keeps the result for that exact git tree, and posts the required check from its own GitHub App. By default it runs the commands it finds, such as `cargo test` or `npm test`, natively in an OS sandbox. On a machine set up like production, with the same tools, dependencies, OS and system libraries, that tests the code where it will run. Or it runs your GitHub Actions workflows in containers with act, the mode closest to GitHub's CI. A PR whose tree was already tested gets its check in under a second.

Time to result on bean, a private Python repository, on an Apple silicon Mac with Docker Desktop against GitHub's hosted runners: after a push TBD vs TBD, after a commit with the hooks TBD vs TBD, a tree already tested TBD vs TBD. Method and numbers in [docs/BENCHMARKS.md](docs/BENCHMARKS.md). A single run can take longer than on GitHub. The gain is no runner queue, no Actions minutes on PR pushes, and no new run for a tree already tested.

Docs: https://shipcheck.run/docs/ (source in `site/content/docs`).

## Install

With Homebrew, which also installs act, git and the Docker client:

```sh
brew install gabrielkoerich/tap/shipcheck
```

Or with the install script:

```sh
curl -fsSL https://shipcheck.run/install.sh | sh
```

Or with cargo, Rust 1.89 or later:

```sh
cargo install shipcheck
```

Or from source:

```sh
cargo install --git https://github.com/gabrielkoerich/shipcheck shipcheck
```

| Needs | Why |
|---|---|
| git | shipcheck reads commits, trees and `main` from your clone |
| Docker | act runs each job in a container. Docker Desktop is recommended. OrbStack and colima are tested, with limits in the install docs |
| [act](https://github.com/nektos/act) 0.2.89 or later | Runs the workflows. `brew install act` |
| A copy-on-write filesystem | APFS on macOS, btrfs or XFS on Linux, so a warm checkout costs no disk. On other filesystems shipcheck falls back to a plain git worktree |

Then run `shipcheck init` in your repository. See [the quickstart](https://shipcheck.run/docs/quickstart/).

## What works without the GitHub App

Status: early. The one-click hosted GitHub App is not live yet ([#2](https://github.com/gabrielkoerich/shipcheck/issues/2)). Without it, these work on any repository:

- `shipcheck run`: runs the workflows locally on the working copy and gives the verdict as an exit code. `shipcheck commit` then commits exactly that tree, so its check needs no new run.
- The result cache: a tree that already passed answers at once.
- The git hooks: start runs in the background on commit or push.
- `shipcheck status` and `shipcheck logs`: show the stored result and logs of a commit.

The `shipcheck` check on a PR (`shipcheck post`, `shipcheck daemon`) needs a GitHub App and its private key on the machine that runs shipcheck. Today that means your own App, with `SHIPCHECK_APP_ID` and `SHIPCHECK_APP_KEY_FILE` set. A one-click hosted App is coming.

## Layout

| Folder | Holds |
|---|---|
| `src/` | The `shipcheck` CLI |
| `site/` | The website and user docs, built with [Zola](https://www.getzola.org): `zola --root site serve` |
| `docs/` | Design notes: why it is built this way, what a check proves, and what a job can reach |

## License

MIT
