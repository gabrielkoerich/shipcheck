+++
title = "Commands"
description = "The commands, their exit codes and their environment variables."
weight = 50
+++

| Command | Does |
|---|---|
| `shipcheck run [--sha <rev> \| --staged] [--no-fetch]` | Runs the checks in `shipcheck.toml`, host checks or workflows, and waits for the verdict. With no `--sha`, it tests the working copy: tracked edits and untracked files that are not gitignored, which it lists. `--staged` tests the index. A working copy with no change is `HEAD`. `--sha` tests that commit only. Fetches `origin/main` first unless `--no-fetch`. A tree that already passed, or a change outside `paths`, answers at once. For each failed job it prints the failed step and its exit code, the failing tests by name, the last 60 lines of the step and the path of the full log. A failed host check gets its tests and the last 60 lines of its output. `--json` prints one object instead, see [JSON](#json) |
| `shipcheck run --job <id> \| --workflow <file>` | A partial run, for the loop after a failure: runs only those jobs, or every job of those workflows, and the jobs they `needs`. Both flags repeat. A matrix job's id runs all its legs. In host mode `--job` names a `[host]` check and runs only it, and `--workflow` is an error. The result is never stored, so it is never the check and `shipcheck commit` and `status` ignore it. Exit codes as usual. Fix, run the failed job as often as needed, then one full `shipcheck run` and `shipcheck commit` |
| `shipcheck commit -m <message> \| -F <file> [--allow-failed] [--no-verify]` | Commits exactly the tree the last `shipcheck run` of this working copy tested, through `git commit`, so hooks and signing work as usual. Refuses when a file changed since the run, and when the run failed unless `--allow-failed`. `--no-verify` goes to `git commit`. The commit's check is then that run's result |
| `shipcheck doctor` | Checks git, act, the Docker engine and its versions, or in host mode the OS sandbox, copy-on-write, the count and size of shipcheck's Docker volumes and images, `origin/main` and its `shipcheck.toml`, the network setting, the hooks, whether `shipcheck post` can post, and where shipcheck's secrets are, reading each so that after an upgrade the keychain asks here. Marks each line `ok` or `fix`, a green check or a red cross in a terminal, with the command that fixes it. Exits 0 when a run can work, 1 otherwise |
| `shipcheck status [<rev> \| --staged] [--json]` | Prints the stored result for the tree `run` would test. With no revision it first prints whether the working copy's tree and the index's tree have a result, so you know before a commit whether it reuses one. With no result it exits 2, and with `--json` it prints `{"tree": "<id>", "result": null}`. Without `--base`, a tree that has no result under `main`'s `shipcheck.toml` shows its last run's, such as one with `--base <branch>`, and says which base it read |
| `shipcheck logs [<rev> \| --staged] [--base <ref>]` | Prints the full logs of that tree, one per check or workflow, then the last partial run's, with control characters escaped. Without `--base` it finds the last run of the tree as `status` does |
| `shipcheck history [--json] [--limit <n>]` | Prints the last runs of this repository on this machine, 20 by default: when, commit, mode, result, time, whether a stored pass answered, and what started the run. `--json` prints the records as one array. See [Run records](#run-records) |
| `shipcheck post [<rev>] [--home-ok]` | Posts the stored result of the commit's tree on GitHub as one completed check per job, `shipcheck/<job>`, and a `shipcheck` summary. A job id two workflows share is `shipcheck/<workflow>/<job>`, the workflow without `.yml`. The job checks are the ones to require. A job `main` has that the result lacks posts as a failure. Refuses when `main` has no `shipcheck.toml`. Each job check carries its log tail as `logs` in `shipcheck.toml` says, every job by default and always a failed one, and anyone who can read the repository sees it. Each check shows how long its job took. Refuses when `SHIPCHECK_HOME`, `HOME` or `XDG_STATE_HOME` moves the results away from the default, since an agent could have written them there. Pass `--home-ok` if you set it yourself. The daemon takes `--home-ok` too |
| `shipcheck login` | Signs this machine in to shipcheck's service with GitHub, so `shipcheck post` works without an App key. Posting needs the shipcheck App installed on the repository and write access for you. Prints a code to enter at github.com/login/device, and in a terminal Enter opens that page in the browser. The machine token stops working after 90 days without a post, then run it again. On macOS it goes in the login keychain, see [where shipcheck keeps its keys](@/docs/install.md#where-shipcheck-keeps-its-keys) |
| `shipcheck logout [--all]` | Revokes this machine's token and deletes it. With `SHIPCHECK_TOKEN` set, revokes that token instead and keeps the stored login token. `--all` revokes every token of your GitHub user, on every machine, and prints how many. When this machine's token is no longer valid, `--all` revokes nothing and fails, so sign in again first |
| `shipcheck token [--name <machine>]` | Makes a machine token for another machine to use as `SHIPCHECK_TOKEN`. Prints a code to enter at github.com/login/device on stderr, as `login` does, then only the token on stdout. Leaves this machine's sign-in alone. `--name` labels it, `token` by default. Each user can hold 50 live tokens |
| `shipcheck init [--yes]` | For the repository owner: writes `shipcheck.toml` with a host check for each workflow job it can turn into one, writes the agent instructions into `AGENTS.md` and the other files agents read, and runs `install`. On a terminal it also offers to install the App, run hosted Actions only on `main` and require the job checks, asking before each step. `--yes` takes each default without asking. Without a terminal or `--yes` it asks nothing and skips those three steps. It leaves an existing `shipcheck.toml` as it is. See [the quickstart](@/docs/quickstart.md) |
| `shipcheck install` | For each clone: installs the pre-push and post-commit hooks, removes the agent hooks an older version wrote, and starts the daemon service for this repository. Running it again changes nothing, and an error with the service is one line that never fails the install. In a terminal, it signs the machine in when it cannot post yet. Without one it prints how to. See [Agents](@/docs/agents.md) |
| `shipcheck daemon [--home-ok \| --uninstall]` | `init` and `install` start it as a background service, a launchd agent on macOS or a systemd user unit on Linux, and add the repository to `daemon.toml` in `SHIPCHECK_HOME`. Every `interval_seconds` (10 by default) it checks the open PRs into the configured branch. A head with no completed `shipcheck` check gets its stored result at once, or, with no result, a run that posts in progress and then the result, as a push does. It runs only PRs you, or a login `daemon_authors` lists, both opened and pushed the head of, never a head that contains one it refused, and never a fork's. It posts with `shipcheck login` or an App key, and waits until one is there. `--uninstall` stops and removes the service |
| `shipcheck clean [--all]` | Removes what shipcheck keeps for this repository: its Docker cache volumes, `actions/cache` entries, warm checkouts and run slots. `--all` does it for every repository, also removes stored results, logs and run records, and removes the daemon service. It never removes a Docker image. A volume that a container uses, running or stopped, stays. Everything rebuilds on the next run, which is slower once. Refuses while a run is going |

The git hooks call `shipcheck on-push` and `shipcheck on-commit`. Those start a background run when `on` asks for one, never fail, and never make git wait. On a machine that can post, a push's run posts the checks. You do not call them yourself.

`--base <ref>` reads `shipcheck.toml` and the workflows from another ref than `origin/main`, and turns off the fetch. It is for trying shipcheck on a repository before the file is on `main`.

## JSON

`--json` prints one object on stdout and the progress on stderr. Exit codes stay the same.

```json
{
  "tree": "4b825dc6...", "sha": "9f2c1e0a...", "partial": false, "passed": false, "skipped": null, "ci_changes": [],
  "workflows": [{
    "file": "ci.yml", "passed": false, "exit": 1, "attempts": 1, "log": ".../logs/<key>/ci.yml.log",
    "jobs": [{
      "id": "test", "status": "failure", "attempts": 1, "shards": null,
      "failed_step": "Run pytest", "exit": 1, "note": null,
      "failing_tests": ["tests/test_math.py::test_add"], "failing_tests_total": 1,
      "tail": ["...", "FAILED tests/test_math.py::test_add - assert 3 == 4"],
      "log": ".../logs/<key>/ci.yml/test.log", "attempt_logs": []
    }]
  }]
}
```

Test names come from pytest, cargo test, nextest, go test, jest, vitest, rspec and phpunit output, at most 30. `sha` is `working copy` or `index` when the run tested one of those. `partial` is true for a `--job` or `--workflow` run. In host mode, one workflow named `host` holds a job per check, with no `failed_step` or `exit`, and `tail` is the end of the check's output. `failing_tests_total` counts them all. `tail` holds at most 60 lines, with control characters escaped. `note` explains a failure that is not the code's, such as a stale checkout or a job with no image on this host.

## Run records

Each run adds one line to `records/<owner>/<repo>.jsonl` in `SHIPCHECK_HOME`: when it ran, the commit and tree, the branch, what started it, the machine name and OS, the shipcheck and tool versions, whether a stored pass answered, and for each job its status, attempts, times and failing test names. The daemon also adds the PR number and author, whether it posted the checks and the comment, when a PR it checked gets a new head, and when one is merged and by whom. A record holds no code, file contents, log output, environment values or secrets.

Records stay on your machine. shipcheck sends them nowhere. A paid plan may later offer dashboards built from them, and records would be uploaded only if you turn that on yourself. `shipcheck history` reads them locally.

Records are not deleted with old results and logs. A file past 50 MB moves to `<repo>.<n>.jsonl`. To delete them, remove the `records` folder, or run `shipcheck clean --all`, which also removes results, logs and warm checkouts.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Every check passed, or nothing under `paths` changed |
| 1 | A check failed, or `doctor` found something to fix. The output names the failed step and tests, `shipcheck logs` prints every log |
| 2 | shipcheck could not run, for example `shipcheck.toml` is invalid, the OS sandbox is missing in host mode, or act is not installed in act mode. The message says what to fix. For `status`, the tree has no result yet |

## Environment

| Variable | Default | Meaning |
|---|---|---|
| `SHIPCHECK_HOME` | `~/Library/Application Support/shipcheck` on macOS, `$XDG_STATE_HOME/shipcheck` or `~/.local/state/shipcheck` on Linux | Where results, logs and warm checkouts live. Background runs log to `logs/background.log` in it. At the start of each run, results untouched for 14 days, logs untouched for 90 days and warm checkouts no run used for 7 days are deleted. At its end, the Docker volumes of this home that no run used for 14 days are removed, unless a container uses them, and so are the runner images an older shipcheck pinned. shipcheck never removes a volume it did not make, or an image a workflow names |
| `SHIPCHECK_TIMINGS` | unset | Set, `run` prints how long each phase of each job took to stderr, such as the clone, act's start and the cleanup |
| `SHIPCHECK_MAX_RUNS` | a quarter of the CPUs, at least 1 | How many runs go at once on this machine. Others wait for a slot |
| `SHIPCHECK_API` | `https://api.shipcheck.run` | The service `login`, `logout`, `token` and `post` talk to |
| `SHIPCHECK_APP_ID` | unset | The ID of [your own GitHub App](@/docs/install.md#your-own-app), for `post` and the daemon |
| `SHIPCHECK_APP_KEY_FILE` | `app.pem` in `SHIPCHECK_HOME`, kept in the login keychain on macOS | A private key file of that App that you keep yourself, read as a file. Prefer `app.pem`, which macOS keeps in the keychain |
| `SHIPCHECK_NO_DAEMON` | unset | `1` makes `init` and `install` leave the daemon service out on this machine |
| `SHIPCHECK_SKIP` | unset | `1` on a commit or push starts no background run, see [skips](@/docs/config.md) |
| `SHIPCHECK_TOKEN` | the token from `shipcheck login` | A machine token for `post`, for an unattended machine such as a VM or an agent box. Make one with `shipcheck token`. When set, `post` uses the service even if an App key is on the machine, and `logout` revokes it |
