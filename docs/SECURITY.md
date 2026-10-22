# Security

shipcheck runs a branch's workflow and test code on a developer's or maintainer's own machine. Whoever controls that code controls what runs there: a contributor with write access, a compromised dependency, a retargeted action tag, or a prompt-injected agent. A maintainer's Mac is a self-hosted runner that also holds SSH keys, browser sessions and every other project, and GitHub's own advice is that self-hosted runners should almost never run untrusted code. So every job is treated as untrusted, and a job must never reach the host. PROOF.md covers what a result proves. This file covers what a job can touch.

## What a job can reach

| Resource | A job gets | How |
|---|---|---|
| Host processes and files | Its checkout and its cache volumes, nothing else | A Docker container. `-self-hosted` platforms, which act runs on the host, are refused |
| Docker | No socket | `--container-daemon-socket -`. With the socket a job could mount any host path |
| Docker options from the workflow | Only `--cpus`, `--memory`, `--shm-size`, `--env`, health checks, `--user`, `--workdir`, `--entrypoint`, `--hostname`, `--ulimit`, `--init` | act passes `container.options` and service `options` past every flag shipcheck sets. Anything else, `volumes`, and any `${{ }}` in these settings are refused before the run, since act would expand it into options shipcheck never saw |
| Host environment and tokens | Nothing | act gets an allowlisted environment and `-s GITHUB_TOKEN=`, since act fills an unset `GITHUB_TOKEN` from the host |
| The repository's `.git` | A new repository with only the tested commit's history and the tags in it, no other branches, stash, remotes, hooks or credentials | Built from the job's worktree before the job starts, with git pointed at the worktree's git dir on the host |
| What the job leaves in its checkout | Nothing that runs or is read on the host | The job's config can name commands, such as `core.fsmonitor`, that any git command in the checkout starts. After the job shipcheck removes every container of the job, then puts the worktree's `.git` file back before it warms the template from the checkout or empties it |
| The terminal | Job output with control characters escaped | `shipcheck logs` and the lines `shipcheck run` prints |
| act's configuration | Only what shipcheck passes | `.actrc`, `.env`, `.secrets`, `.vars` and `.input` are removed from the checkout, and the env, var and input files are `/dev/null` |
| Network | Nothing, by default | `network = "none"` gives `--network none`, and jobs with `services:` are refused, since act gives them a network of its own. `network = "internet"` gives each act process a Docker bridge network of its own, never the host's, see below |
| Artifacts | act's artifact server on a random port, on `127.0.0.1` reached through `host.docker.internal` on a Mac, and on the run network's gateway on Linux | Not the LAN or every interface, where its hardcoded signing key would let anyone read or replace artifacts. Not the default bridge's gateway, which every container on the machine reaches |
| Secrets | Only what the owner put in `secrets/<owner>/<repo>.env` | `--secret-file`, never from the commit. Every branch's run gets them, as on GitHub for branches of the same repository |
| GitHub | No token | The check is posted by the daemon with the App's token, never inside a job |
| Host git on the branch's tree | Never runs git-lfs, submodules or fsmonitor | Every git process shipcheck starts, git-cow's included, gets `filter.lfs.*` off, `submodule.recurse=false` and `core.fsmonitor=false` through `GIT_CONFIG_*`. git-lfs read the tree's `.lfsconfig` (CVE-2024-53263 sent credentials to its URL) and wrote through the tree's symlinks (CVE-2025-26625). A submodule checkout ran a hook (CVE-2025-48384) |
| Reusable workflows | Refused | act runs a called workflow's jobs without shipcheck's Docker option checks, reads a local one from the branch and clones a remote one on the host. A branch could have called a workflow with `--privileged --pid=host`, as in Gitea runner CVE-2026-73802 |
| The check | Log tails only with `logs = "failed"` on main or `post --with-logs`, never for a fork's PR | Values from the secrets file are replaced with `***` before posting, beyond act's own masking |

Which commits run at all: hooks run only the developer's own commits and pushes, and the daemon runs only PRs from branches in the same repository, never from forks.

## Tested controls

Each control has a test that fails if it stops working. The e2e cases are sections of `runs_caches_and_ignores_the_working_copy` in `tests/e2e.rs`, which CI runs on Linux and the author runs on a Mac with Docker Desktop.

| Control | Test |
|---|---|
| A job's git config, hooks, filters, symlinks and hardlinks run nothing on the host and change no host file, through warming, emptying or the Linux hand-back | `run::tests::what_a_job_writes_in_its_checkout_runs_nothing_on_the_host`, and the e2e isolation case, whose job also leaves a process rewriting `.git` |
| shipcheck's own git commands on a checkout never read its `.git` | `run::tests::git_on_a_job_checkout_never_reads_its_git` |
| Only the allowed Docker options pass, in every quoting, `${{ }}` and matrix form | `runner::act::tests::allows_only_the_listed_docker_options_in_every_form`, `runner::act::tests::refuses_docker_options_that_reach_the_host` |
| Engines with known escapes are refused | `runner::act::tests::refuses_engines_with_known_escapes` |
| No Docker socket, host token or host environment | e2e isolation case |
| A `.git` of its own, without unpushed work, stash, private tags or credentials, and the host's `.git` untouched | e2e isolation case |
| With `network = "none"`: no route, no DNS, and no host gateway, `host.docker.internal`, Docker API port, Docker Desktop proxy or listener on the host's `127.0.0.1`, also from a `docker://` step | e2e isolation case |
| With `network = "internet"`: the internet, but not a listener on the host's `127.0.0.1`. Jobs run on the run's own network, never `host`, which is removed after the run, and `shipcheck clean` removes one a crashed run left. The artifact server listens on loopback on a Mac and on the gateway on Linux | e2e artifacts case, `runner::act::tests::the_artifact_server_listens_on_the_run_network_gateway_or_loopback` |
| Outputs reach the next job as literal strings | `runner::act::tests::outputs_reach_the_next_job_as_literal_strings`, `runner::act::tests::refuses_needs_in_an_env_it_cannot_move`, e2e isolation jobs `out` and `use` |
| Secrets reach only their own repository's jobs, and log tails for the check carry none | e2e isolation case, `check::tests::failing_body_has_the_log_tail_when_asked`, `run::tests::secrets_longest_first_with_each_line` |
| A branch's run leaves nothing in a later run of main but the state listed under Open | e2e state case |
| A job past its timeout leaves no container or process | e2e hang case |
| Control characters in job output never reach the terminal raw | `run::tests::escapes_control_characters_for_the_terminal`, e2e `shipcheck logs` |
| Dotfiles in the checkout do not change how act runs | e2e isolation case, and the `.actrc` of the feature branch |

## Host mode

`runner = "host"` gives up the container. Each check is a command from `shipcheck.toml` on `main`, run with `sh -c` as the developer's user, on the host, with the host's network, inside an OS sandbox shipcheck sets up for each command: `sandbox-exec` on macOS, bubblewrap (`bwrap`) on Linux. Without one, host mode refuses to run and `shipcheck doctor` names the fix. The sandbox exists because `shipcheck install` lets agents run `shipcheck run` outside their own sandbox, so without it host mode would hand a sandboxed agent's branch code the user's rights. `versions` commands run in the same sandbox, with the developer's checkout readable.

| Resource | A check gets | How |
|---|---|---|
| Writes | Its clone, a new home, and the repository's tool caches under `SHIPCHECK_HOME/hostcache/<owner>/<repo>/`: `base/` for runs of the base's tree, `branches/` for every other tree | macOS: `(deny file-write*)` but those paths. Linux: the root read-only, those paths bound writable, new empty `/tmp`, `/run` and `/var/run` that vanish with the check |
| `HOME`, `TMPDIR` | A new directory, mode 0700 with a random name in the system's temp dir, removed after the command | `tempfile` |
| Reads of the user's home | Only toolchain paths, such as `~/.cargo/bin`, `~/.rustup/toolchains`, `~/.nvm/versions` and `~/.local/share/uv/python` (`TOOLCHAINS` in `src/runner/host.rs`). Credential files, such as `~/.cargo/credentials.toml`, `~/.netrc`, `~/.npmrc` and `~/.ssh`, and shipcheck's `token`, `app.pem`, `secrets/` and `SHIPCHECK_APP_KEY_FILE` are denied again | macOS: `(deny file-read-data)` on the home, then the list. Linux: a read-only empty tmpfs over the home, then the list bound read-only. A fixed list, since the caller sets `PATH` and `RUSTUP_HOME` |
| Other reads | The whole system outside the home | Toolchains under `/usr`, `/opt/homebrew` and `/usr/local` |
| Unix sockets | Only in its writable paths, and macOS's DNS resolver | macOS: `(deny network-outbound (remote unix-socket))` but those. Linux: the host's socket dirs are hidden by the empty `/tmp`, `/run` and home. Docker's socket, an agent's and `ssh-agent` are out of reach |
| macOS services | Only user lookups, notifications, logging, temp dirs, the clock, DNS and proxy settings and `trustd` for TLS | `(deny mach-lookup)` but `MACH_SERVICES`, and `(deny appleevent-send)`. So `open`, `osascript`, `launchctl` or a copy of them cannot reach launchd, Launch Services, another app or the keychain, which would start a process outside the sandbox |
| Executables | Anything it can read, its clone, home and caches included | Not restricted: `cargo test`, `go test` and `npx` run binaries they just built or fetched there. The capability rules above are the wall |
| Environment | `PATH`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `SHELL`, `TERM`, the names in `[host] env`, `CI=true`, and `HOME`, `TMPDIR`, `RUSTUP_HOME`, `CARGO_HOME`, `npm_config_cache`, `UV_CACHE_DIR`, `XDG_CACHE_HOME`, `GOPATH` and `GOCACHE` set by shipcheck | `env_clear`, then that allowlist. `[host] env` refuses `SSH_AUTH_SOCK`, `GIT_*`, `SHIPCHECK_*`, `AWS_*`, `GOOGLE_*`, `AZURE_*`, names holding `TOKEN`, `SECRET`, `PASSWORD` or `CREDENTIAL`, and names ending in `_KEY` |
| The repository's `.git` | A new repository with only the tested commit's history, as for a job | Same as act mode |
| Terminal | None | `setsid` before exec, stdin `/dev/null`, `/dev/tty` not writable, and bubblewrap's `--new-session`, so a check cannot push keystrokes into the developer's terminal |
| Processes it starts | Killed with its session's process group at the end or at the timeout | `killpg(SIGKILL)`. On Linux a pid namespace also kills everything when the check ends. On macOS a process that calls `setsid` again leaves the group and survives, still inside the sandbox |
| Network | The host's: the internet, the LAN and TCP services on `127.0.0.1` | None. Package managers need it |

Residual risks: a check can send anything it can read to the internet, which excludes the home and the denied files but includes system files and the clone. It can reach TCP services on the machine and the LAN, such as a database on `127.0.0.1` or Docker's API if it listens on TCP. On Linux it can reach abstract Unix sockets, which have no path to hide, such as an X11 or D-Bus session bus that uses one. A branch's run can write into the `branches/` tool cache that later runs of other branches read, as act mode's volumes can (Open). A copy of the system's `open` is denied by capability, but a new kind of macOS service that starts processes and needs no Mach lookup would need a rule.

Which commits run: the hooks run only the developer's own commits. The daemon refuses host mode for any PR whose head is not in the same repository, including from the cache, before it posts anything.

Tested: `runner::host::tests::the_profile_lets_a_check_write_only_in_its_jail`, `runner::host::tests::bwrap_hides_the_hosts_sockets_and_home_and_binds_the_jail`, `runner::host::tests::a_check_writes_only_in_its_jail` (writes outside, the home, credentials, a Unix socket outside, `/dev/tty`), `runner::host::tests::a_check_starts_no_process_outside_the_sandbox` (`open`, a copied `open`, `osascript` to Terminal, a copied `launchctl submit`, `launchctl bootstrap`), `runner::host::tests::passes_only_the_allowlist_and_the_owners_names`, `runner::host::tests::refuses_names_that_carry_credentials`, `runner::host::tests::a_timeout_kills_the_whole_process_group`, `runner::host::tests::a_finished_check_leaves_nothing_running`, `config::tests::checks_host_mode`, and `tests/host_e2e.rs`, which runs without Docker and checks that a write to the real home and outside the clone lands nowhere. CI installs bubblewrap and the AppArmor profile that lets only bwrap create user namespaces, as the install docs say.

## `network = "internet"`

The owner turns it on in `shipcheck.toml` on `main` when the workflows need downloads. Each act process gets a Docker bridge network of its own, `shipcheck-<home>-net-<tag>`, created before the run with the label `shipcheck` and removed after it. A run that crashed leaves one, which the next run in that slot, `reclaim` and `shipcheck clean` remove. act's own default is `--network host`, which on a Linux machine reached every port the host listens on, its `127.0.0.1` services and, on a Tailscale machine, the tailnet. Rejected: Docker's default bridge, whose gateway every container on the machine reaches, so the artifact server there would serve the user's other containers.

What a job on the run network still reaches:

- The internet.
- The host at the network's gateway address, on every port the host binds to `0.0.0.0` or to that address. On Linux the artifact server listens there, on a random port, for the run only. Docker's isolation rules stop traffic between bridges, but not traffic to the host's own addresses, so a container on another bridge can reach that gateway address too.
- The LAN and the tailnet through the host's routing, and services the host binds to its LAN address.
- On a Mac, Docker Desktop's VM network, which reaches the Mac's loopback through `host.docker.internal`, as the e2e artifacts case relies on, and Docker Desktop's proxy at `192.168.65.1:3128`. The research measured this: every mode except `--internal` reaches the Mac's loopback services through `host.docker.internal`, the LAN and the router.
- Jobs with `services:` run on the network act creates for them, also a bridge.

Planned fix, tested in parts by the research:

1. A Docker network per run, created by shipcheck, also under the name act uses for jobs with services.
2. On Docker Desktop and colima, firewall rules in Docker's VM, set through a privileged `nsenter` container, that drop traffic from that network to private addresses, `host.docker.internal` and the gateway, and allow the internet. One rule lets the artifact server's single port through. shipcheck checks the rules before every run and refuses to run without them, since they vanish when the VM restarts.
3. Elsewhere, an `--internal` network with a filtering proxy container, such as Smokescreen, and `HTTP_PROXY` in jobs.
4. Later, a Linux VM that shipcheck owns as the Docker host, with host loopback off.

Rejected for now: one VM per job (Kata, Firecracker), since Docker Desktop on Apple silicon has no `/dev/kvm`, and Apple's `container`, since act needs a Docker API.

## The hosted service

The service at `api.shipcheck.run` (ARCHITECTURE.md) is the only place outside the author's machines that holds the App's private key.

| It holds | Where |
|---|---|
| The App's private key, App ID, client id and client secret | Worker secrets on Cloudflare |
| For each signed-in machine: the SHA-256 of its token, the GitHub user id and login, the machine name, when it signed in and when the token expires, 90 days after its last use | D1 |

It never stores the GitHub user token, which it sends once to GitHub to learn whether our App issued it and to whom, a machine token in clear, a check body, code or logs. A log tail passes through only when the user posts with `--with-logs`.

| It trusts | So |
|---|---|
| GitHub's answer, under the App's client secret, about the token the CLI sends | A machine is bound to the user who completed the device flow on our App. A personal access token or another App's token that leaked is refused |
| GitHub's permission answer for that user on the repository | Only users with write, maintain or admin get a check posted, the same people who can push to the branch |
| The check body from a signed-in machine | It cannot prove the run happened (PROOF.md). It refuses any field or check name `src/check.rs` does not write, so a machine token can only post the `shipcheck` check on the commit it names |

Each installation token it mints covers one repository with `checks: write` and `metadata: read`, and lives only for the request. A stolen machine token posts checks as its user until it goes 90 days unused, or `shipcheck logout` or a row delete revokes it. A stolen token in steady use never expires: that is the cost of agents never needing a person to sign in again. `shipcheck login` makes a new one. There is no rate limit yet beyond Cloudflare's.

## Open

| Risk | Planned fix |
|---|---|
| An agent can rewrite its own clone: `git remote set-url origin` or `git update-ref refs/remotes/origin/main`. A `shipcheck run` it starts then reads `shipcheck.toml` from that clone's idea of main, so it can pick `network = "internet"` for a run that leaves its sandbox. It cannot forge a check: `post` and the daemon fetch main from GitHub first, and the config is in the key, so such a result never answers for a real commit. The hook denies `--base` and `--no-fetch` for agents, so a run fetches main first | A separate `_shipcheck` user with its own clone and a pinned remote URL, so nothing the agent writes reaches the config of a run |
| `network = "internet"` reaches services the host binds to `0.0.0.0` or the gateway, the LAN and the tailnet. Containers on other bridges can reach the artifact server's port on the gateway address during a run. Docker Desktop also runs an HTTP proxy in its VM at `192.168.65.1:3128` and `192.168.65.7:3128` that fetches the Mac's `127.0.0.1` and the LAN for any container | Above, with the firewall covering all of `192.168.65.0/24` and limiting the artifact server's port to the run network's subnet, and a probe container before each `internet` run that checks every private target fails |
| Container isolation depends on the runc and Docker Desktop versions (runc escapes of 2024 and 2025, CVE-2025-9074 before Docker Desktop 4.44.3) | Built: before each run, shipcheck reads `docker version` and refuses Docker Desktop before 4.44.3 and runc before 1.2.8 or 1.3.3. An engine whose version it cannot read, such as one on crun, still runs. Open: checking runtimes other than runc |
| State shared between runs is outside the cache key. A branch's run leaves two things in later runs, of `main` too: the gitignored directories of its checkout without a hidden name, such as `build/` or `node_modules/`, which warm the template of the same workflow and job, and every file in the cache volumes of `act.cache_paths`, which hold the tool homes and package caches under `/root/.shipcheck`, Go modules, Dart, NuGet and yarn berry packages, and the volumes of `act.slot_paths` of the same slot and job: apt lists, the Maven, Gradle and sbt homes and the yarn v1 and nub stores. A package cache holds downloaded files that later installs trust, and Gradle's home also holds init scripts that every later build of that slot runs. setup-uv's wheels stay in `/root/.cache/uv`, since shipcheck turns off its prune. The job's `.gitignore` decides what counts as ignored. Its tracked and untracked files, hidden directories, `.git` and container files outside the volumes do not carry over (e2e state case). This is the Cacheract pattern from GitHub's own caches | Decide between warming only from commits on the base, which makes branch runs cold unless `main` was run, and per-branch volumes and templates, which cost disk |
| Everything under `SHIPCHECK_HOME` is writable by the developer's user, so an agent running as that user can write a passing result. The agent hooks deny edits under the home and shell commands that name it, `SHIPCHECK_HOME=` or `api.shipcheck.run`, and Claude Code gets deny rules for the home and its `token`, `app.pem` and `secrets/`. That catches habits only: a variable, base64 or a script the agent runs gets past it. Codex's default sandbox keeps the home outside its writable roots, so there the home is protected unless the user turns the sandbox off | Below |
| An agent can set `SHIPCHECK_HOME`, `HOME` or `XDG_STATE_HOME` to a folder it wrote and post from it | Built: `post` and the daemon refuse a home other than the default unless a person passes `--home-ok`. The default comes from the user's passwd entry (`getpwuid_r`), so no variable moves it, and a Linux user with `XDG_STATE_HOME` set must pass the flag too. The agent hooks deny `--home-ok` and those variables. A flag is a weak gate, since an agent that gets past the hooks can type it. Planned: confirm on an interactive terminal instead |
| A cloned repository can ship `AGENTS.md`, `CLAUDE.md`, `.claude`, `.codex` or another file `init` and `install` write as a symlink to `~/.zshrc` or `~/.ssh` | Built: they refuse a path that is a symlink, sits under one or resolves outside the clone, and open with `O_NOFOLLOW` |
| Forged results on macOS | A MAC key in the macOS Keychain over every result, and the machine token in the Keychain too, so only the signed shipcheck binary can write a result that `post` accepts. Cost: a Keychain prompt for each unsigned build |
| Forged results anywhere | The real fix: a separate `_shipcheck` OS user with its own Docker engine and home, results signed with a machine key registered with the service, and the CLI talking to it over a socket. Costs: a second user, a second Docker VM, an install with sudo, and a socket protocol |
| git-cow's `warm` finds the checkout's repository through its `.git` and takes no git options. shipcheck removes every container of the job and puts the worktree's `.git` file back first, so nothing can rewrite it, but act and git-cow still read the checkout's `.git` on the host. act reads it with go-git, which runs no commands | A git-cow option for an explicit git dir and `-c core.fsmonitor=false -c core.hooksPath=/dev/null` on every command, as shipcheck's own git calls use |
| The secrets file is chosen by `owner/repo` from the origin URL, whatever the host, so two remotes with the same owner and name share it | Key the file by host too |
| act and shipcheck mask only exact secret values. A job that prints a secret encoded, reversed or split puts it in a posted log tail | As on GitHub, post log tails only from `main` or when the owner asks |
| act evaluates a job's `env` a second time in each step, so a `${{ }}` inside a value from a need would run as an expression. shipcheck moves such values to each step's `env`. Values from other contexts in a job's `env`, such as a commit message in `github.event`, are still evaluated twice | Report it to act, or move every job `env` value with an expression to the steps |
| act's artifact server skips its run ID check, so jobs of two concurrent runs could read each other's artifacts | A token check in front of it, until act fixes it |
| Docker Desktop runs every container in one VM with file sharing to the Mac. A container escape reaches the VM | The VM that shipcheck owns, above |

## History

- 2026-10-20: a full review found the Docker socket mounted into every job, the host `GITHUB_TOKEN` passed to jobs, act's LAN cache server, symlink writes outside the checkout, a YAML alias bomb against the daemon, workflow Docker options and volumes that undo the isolation, and the whole host `.git` copied into jobs. All are fixed. Trials of public repositories had run before the fixes, so the author revoked the GitHub CLI authorization and an old token found in a repository's remote URL.
- 2026-10-21: regression tests for every control found three holes, all fixed. Warming ran git in the checkout through the job's own `.git`, so a job-written `core.fsmonitor` ran on the host. A need's output named outside `${{ }}`, or put in a job's `env`, was read by act as an expression. So was a `${{ }}` inside a quoted output value in a bare `if`, so values now go in as `fromJSON` of a JSON string with every character but letters, digits and spaces escaped. `shipcheck logs` printed job output raw to the terminal.
