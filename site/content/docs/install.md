+++
title = "Install"
description = "What shipcheck needs on the machine, and how to install it."
weight = 10
+++

What a repository needs depends on its [mode](@/docs/config.md#modes):

- Host mode needs no Docker. On a Mac it needs only shipcheck. On Linux it needs shipcheck and bubblewrap.
- act mode needs Docker and act.

Every repository also needs the [shipcheck App](#post-the-checks) to post its checks on PRs.

## Set up a Mac

Tested on macOS with Docker Desktop on Apple silicon. Host mode uses macOS's own `sandbox-exec`.

1. Install shipcheck with [Homebrew](https://brew.sh). It also installs act, git and the Docker client:

   ```sh
   brew install gabrielkoerich/tap/shipcheck
   ```

2. For act mode, install and start a Docker engine. Host mode needs none:

   | Engine | Install | Tested |
   |---|---|---|
   | Docker Desktop 4.44.3 or later | [docker.com](https://www.docker.com/products/docker-desktop/), then open it once | Yes, recommended |
   | OrbStack | [orbstack.dev](https://orbstack.dev) | Yes. It sometimes showed a job the files of the previous run at the same path, under `/var/folders`. shipcheck now fails such a job instead of passing it |
   | colima | `brew install colima && colima start` | Yes, with the repository and `SHIPCHECK_HOME` under your home directory, the only one colima shares by default. Elsewhere a job sees an empty checkout and fails |

   The three ran the same trials at about the same speed. OrbStack and colima also switch Docker's default context to their own when they start.

   shipcheck refuses Docker Desktop before 4.44.3, which let any container reach the Docker engine.

3. Check it:

   ```sh
   shipcheck doctor
   ```

   It checks git, the OS sandbox or act and the Docker engine, the disk and the repository, marks each line `ok` or `fix`, and prints the command that fixes each `fix`. It exits 0 when a run can work.

macOS disks use APFS, so each job's checkout is a copy-on-write clone and costs no disk. On Apple silicon, jobs run arm64 images. A job whose tools ship no arm64 build can get an amd64 image of its own with `act.jobs` in [`shipcheck.toml`](@/docs/config.md). Turn on Rosetta in Docker Desktop's settings so amd64 images run faster.

## Set up Linux

Tested on Debian 13 with Docker Engine 29 on x86_64.

For host mode, install bubblewrap, which runs each check in a sandbox: `sudo apt install bubblewrap` or `sudo dnf install bubblewrap`.

Ubuntu 23.10 and later let only programs with an AppArmor profile create user namespaces, and bubblewrap needs them. Allow bwrap alone with the profile apparmor ships, and leave the restriction on for everything else:

```sh
sudo apt install apparmor-profiles
sudo cp /usr/share/apparmor/extra-profiles/bwrap-userns-restrict /etc/apparmor.d/
sudo apparmor_parser -r /etc/apparmor.d/bwrap-userns-restrict
```

`shipcheck doctor` runs bwrap once. When it fails and `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` is 1, it prints these commands, or only the last one when the profile is in `/etc/apparmor.d` but not loaded. Without a working bubblewrap, host mode refuses to run.

1. For act mode, install [Docker Engine](https://docs.docker.com/engine/install/) from Docker's own package repository, which brings a current runc. shipcheck refuses runc before 1.2.8 or 1.3.3, which have known container escapes.

2. Let your user run Docker without sudo, then log out and in:

   ```sh
   sudo usermod -aG docker "$USER"
   ```

   Membership of the `docker` group is as strong as root on that machine. shipcheck uses the Docker socket to start jobs, and never gives the socket to a job.

3. Install shipcheck. The install script needs no other tool:

   ```sh
   curl -fsSL https://shipcheck.run/install.sh | sh
   ```

   Homebrew on Linux works too, and also installs act and git: `brew install gabrielkoerich/tap/shipcheck`. Without Homebrew, install git from your distribution and act from its [releases](https://github.com/nektos/act/releases).

4. Check it:

   ```sh
   shipcheck doctor
   ```

   It prints the command that fixes each line it marks `fix`, as on a Mac.

On ext4 there is no copy-on-write, so each job's checkout is a plain git worktree: a full copy of the files and a slower start. btrfs, or XFS with reflink, gives copy-on-write clones as on a Mac. Jobs run as `runner`, uid 1001, so shipcheck gives the checkout to that uid before each job and back to your user after it.

## Other ways to install

With the install script, on macOS or Linux, arm64 or x86_64:

```sh
curl -fsSL https://shipcheck.run/install.sh | sh
```

It downloads the binary from the latest [GitHub release](https://github.com/gabrielkoerich/shipcheck/releases), checks it against the release's `SHA256SUMS`, and installs it to `~/.local/bin`. Set `SHIPCHECK_INSTALL_DIR` to install it somewhere else.

With cargo, from crates.io, with Rust 1.89 or later:

```sh
cargo install shipcheck
```

From source:

```sh
cargo install --git https://github.com/gabrielkoerich/shipcheck shipcheck
```

Check it:

```sh
shipcheck --help
```

## Post the checks

shipcheck posts the result of your local runs as GitHub checks. It needs Checks: write to add the checks to commits, and Pull requests: write to keep one comment on a PR, only when a job writes a report to `$GITHUB_STEP_SUMMARY`. It reads the repository's pull requests to find what to check. It never reads or changes your code.

Set it up once per repository and once per machine:

1. Install the [shipcheck App](https://github.com/apps/shipcheck-run) on the repository. `shipcheck init` prints this link.
2. Sign in each machine that runs shipcheck. `shipcheck install` and `shipcheck init` do it when the machine cannot post yet and they run in a terminal. They print a code to enter at github.com/login/device. `shipcheck login` does the same on its own.

After that, `shipcheck post` sends the result to shipcheck's service, and the service posts the checks.

- You need write access to the repository.
- The service receives the check's text and the report comment, never your code.
- The text holds the end of each job's log ([the checks on the PR](@/docs/how-it-works.md#the-checks-on-the-pr)).
- The machine token stops working after 90 days without a post. Run `shipcheck login` again then.
- `shipcheck logout` signs the machine out.

`shipcheck doctor` says whether posting works, and which identity posts: `SHIPCHECK_TOKEN`, the login token or your own App.

### Unattended machines

A VM, a CI runner or an agent box has no person to enter the code. Make a token for it on a machine where you can open a browser:

```sh
shipcheck token --name agent-vm
```

It prints a code to enter at github.com/login/device, as `login` does, then prints the token on stdout. It does not touch this machine's own sign-in. Put the token in the other machine's environment as `SHIPCHECK_TOKEN`, or in the agent's secret store. It lasts 90 days after its last post, and `shipcheck logout` on that machine revokes it.

If a machine with a token is compromised, run `shipcheck logout --all` on any signed-in machine. It revokes every token of your GitHub user at once, this machine's too, so sign the others in again after. Each GitHub user can hold 50 live tokens, and `logout --all` also clears that limit.

## Your own App

A team can post from an App of its own instead. This is optional.

1. Register a GitHub App with Checks: write and Pull requests: write. Without Pull requests: write the checks still post, and the [report comment](@/docs/how-it-works.md#the-report-comment) does not.
2. Install it on the repository.
3. Set `SHIPCHECK_APP_ID`, and put its private key at `app.pem` in `SHIPCHECK_HOME`. On macOS shipcheck moves it into the login keychain, see [below](#where-shipcheck-keeps-its-keys). `SHIPCHECK_APP_KEY_FILE` points at a key file you keep elsewhere instead.

`shipcheck post` then uses that key directly, with no service. A key on the machine wins over `shipcheck login`, unless `SHIPCHECK_TOKEN` is set. `shipcheck daemon` uses the same key, or the `shipcheck login` token. With a key, set `login` in `daemon.toml` to your GitHub login, so the daemon knows which PRs are yours to run.

## What works without the GitHub App

These work on any repository, with no App:

- `shipcheck run` runs the checks locally and gives the verdict as an exit code.
- The result cache answers at once for a tree that already passed.
- The git hooks start runs in the background on commit or push.
- `shipcheck status` and `shipcheck logs` show the stored result and logs of a commit.

## Where shipcheck keeps its keys

shipcheck keeps three secrets: the machine token from `shipcheck login`, the App key `app.pem`, and the secrets act jobs get, one file per repository at `secrets/<owner>/<repo>.env` in `SHIPCHECK_HOME`, in act's `NAME=value` format. `shipcheck token` prints a token for another machine and keeps no copy.

On macOS, shipcheck moves each of them into an item of your login keychain the first time it reads the file, moves the file to the Trash and prints one line saying so. Only shipcheck reads an item without asking, so an agent running as you gets a keychain prompt instead of the key. To change a secret, write the file again.

The keychain knows shipcheck by its code signature. Release builds, from Homebrew or the install script, are signed with one Developer ID and notarized by Apple, so an upgrade keeps access. A build from `cargo install` or from source gets a new signature each time: macOS then asks once per item whether the new shipcheck may read it. Run `shipcheck doctor` at the Mac and choose Always Allow. Choose it only for shipcheck, never for `security` or another program.

On Linux they stay files of mode `0600`, which any program you run can read. The Linux keyring has no per-program access, so it would not keep an agent out.
