+++
title = "Commands"
description = "The commands, their exit codes and their environment variables."
weight = 50
+++

| Command | Does |
|---|---|
| `shipcheck run [--sha <rev>] [--no-fetch]` | Runs the workflows in `shipcheck.toml` for a commit, `HEAD` by default, and waits for the verdict. Fetches `origin/main` first unless `--no-fetch`. A tree that already passed, or a change outside `paths`, answers at once |
| `shipcheck doctor` | Checks git, act, the Docker engine and its versions, copy-on-write, `origin/main` and its `shipcheck.toml`, the network setting, the hooks, and whether `shipcheck post` can post. Prints `ok` or `fix` with the command to run for each, and exits 0 only when a run can work |
| `shipcheck status [<rev>]` | Prints the stored result for the commit's tree |
| `shipcheck logs [<rev>]` | Prints the full workflow logs of the commit's tree, with control characters escaped |
| `shipcheck post [<rev>] [--with-logs]` | Posts the stored result of the commit's tree as the `shipcheck` check on GitHub. `--with-logs` adds the tail of each failed log, which anyone who can read the repository sees |
| `shipcheck login` | Signs this machine in to shipcheck's service with GitHub, so `shipcheck post` works without an App key. Posting needs the shipcheck App installed on the repository and write access for you. Prints a code to enter at github.com/login/device. The machine token lasts 90 days, run it again to renew |
| `shipcheck logout` | Revokes this machine's token and deletes it |
| `shipcheck init` | For the repository owner: writes `shipcheck.toml` and installs the hooks |
| `shipcheck install` | For each clone: installs the pre-push and post-commit hooks |
| `shipcheck clean [--all]` | Removes what shipcheck keeps for this repository: its Docker cache volumes, `actions/cache` entries, warm checkouts and run slots. `--all` does it for every repository and also removes stored results and logs. Everything rebuilds on the next run, which is slower once. Refuses while a run is going |

The hooks call `shipcheck on-push` and `shipcheck on-commit`. Those start a background run when `on` asks for one, never fail, and never make git wait. You do not call them yourself.

`--base <ref>` reads `shipcheck.toml` and the workflows from another ref than `origin/main`. It is for trying shipcheck on a repository before the file is on `main`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Every workflow passed, or nothing under `paths` changed |
| 1 | A workflow failed. `shipcheck logs` shows why |
| 2 | shipcheck could not run, for example `shipcheck.toml` is missing on `main` or act is not installed. The message says what to fix. For `status`, the tree has no result yet |

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `SHIPCHECK_HOME` | `~/Library/Application Support/shipcheck` on macOS, `$XDG_STATE_HOME/shipcheck` or `~/.local/state/shipcheck` on Linux | Where results, logs and warm checkouts live. Background runs log to `logs/background.log` in it. Results and logs untouched for 14 days are deleted at the start of each run |
| `SHIPCHECK_MAX_RUNS` | a quarter of the CPUs, at least 1 | How many runs go at once on this machine. Others wait for a slot |
| `SHIPCHECK_API` | `https://api.shipcheck.run` | The service `login`, `logout` and `post` talk to |
| `SHIPCHECK_TOKEN` | the `token` file from `shipcheck login` | A machine token for `post`, for CI or a machine without a home directory. When set, `post` uses the service even if an App key is on the machine |
