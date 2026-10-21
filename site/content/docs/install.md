+++
title = "Install"
description = "What shipcheck needs on the machine, and how to install it."
weight = 10
+++

## Set up a Mac

Tested on macOS with Docker Desktop on Apple silicon.

1. Install shipcheck with [Homebrew](https://brew.sh). It also installs act, git and the Docker client:

   ```sh
   brew install gabrielkoerich/tap/shipcheck
   ```

2. Install and start a Docker engine:

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

   It checks git, act, the Docker engine, the disk and the repository, and prints the command that fixes each line marked `fix`. It exits 0 when a run can work.

macOS disks use APFS, so each job's checkout is a copy-on-write clone and costs no disk. On Apple silicon, jobs run arm64 images. A job whose tools ship no arm64 build can get an amd64 image of its own with `act.jobs` in [`shipcheck.toml`](@/docs/config.md). Turn on Rosetta in Docker Desktop's settings so amd64 images run faster.

## Set up Linux

Tested on Debian 13 with Docker Engine 29 on x86_64.

1. Install [Docker Engine](https://docs.docker.com/engine/install/) from Docker's own package repository, which brings a current runc. shipcheck refuses runc before 1.2.8 or 1.3.3, which have known container escapes.

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

   It checks git, act, the Docker engine, the disk and the repository, and prints the command that fixes each line marked `fix`. It exits 0 when a run can work.

On ext4 there is no copy-on-write, so each job's checkout is a plain git worktree: a full copy of the files and a slower start. btrfs, or XFS with reflink, gives copy-on-write clones as on a Mac. Jobs run as root in their containers, so the files they write belong to root. shipcheck hands them back to your user after each job.

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

## What works without the GitHub App

Without the App, these work on any repository:

| Works | What it does |
|---|---|
| `shipcheck run` | Runs the workflows locally and gives the verdict as an exit code |
| The result cache | A tree that already passed answers at once |
| The git hooks | Start runs in the background on commit or push |
| `shipcheck status` and `shipcheck logs` | Show the stored result and logs of a commit |

The `shipcheck` check on a PR needs a GitHub App installed on the repository. Today that is an App of your own: register it with `checks: write` and `pull_requests: read`, install it on the repository, then set `SHIPCHECK_APP_ID` and `SHIPCHECK_APP_KEY_FILE`, and `shipcheck post` uses that key directly.

A one-click shipcheck App is coming. Once it is live, sign each machine in once:

```sh
shipcheck login
```

It prints a code to enter at github.com/login/device. After that, `shipcheck post` sends the result to shipcheck's service, which posts the check if you have write access to the repository. The service receives the check's text, never your code, and a log tail only with `shipcheck post --with-logs`. `shipcheck logout` signs the machine out.

Next: [the quickstart](@/docs/quickstart.md).
