+++
title = "Security"
description = "What a job can reach on your machine, what the App can do on GitHub, what the service stores, and how to report a vulnerability."
weight = 65
+++

shipcheck runs a branch's code on your machine. Whoever writes that code, a teammate, a dependency or an agent, decides what runs there. So shipcheck treats every job as untrusted code.

## What a job can reach

| | act mode | Host mode |
|---|---|---|
| Where it runs | A Docker container, never on the machine itself. `-self-hosted` is refused | Natively, as your user, in `sandbox-exec` on macOS or bubblewrap on Linux |
| Files it writes | Its own checkout and the repository's cache volumes | Its own clone, a new empty `HOME` and `TMPDIR`, and the repository's tool caches |
| Files it reads | Its checkout and volumes | The system and your toolchains, such as `~/.cargo/bin` or `~/.nvm/versions`. Nothing else in your home. Credential files such as `~/.ssh`, `~/.npmrc`, `~/.netrc` and shipcheck's own token and keys stay hidden |
| Docker | No socket | No socket. It cannot connect to a Unix socket outside its own folders |
| Environment | No host environment and no GitHub token | `PATH`, `USER`, `LANG` and a few more, the names in `host.env`, and `CI=true`. `host.env` refuses names that carry credentials |
| Git | A `.git` of its own with only the tested commit's history. git-lfs and submodules never run on the host | Its own clone |
| Secrets | Only the values the owner put in shipcheck's secrets file for that repository | None |
| Network | None by default. `network = "internet"` gives the internet and the local network, but not services on the machine's `127.0.0.1` | The machine's network: the internet, the LAN and local services |

act mode also refuses Docker options that reach the host, such as `--privileged`, and refuses reusable workflows. It refuses Docker Desktop before 4.44.3 and runc versions with known container escapes.

Host mode gives up the container, so use it only for your own and your agents' work. The daemon never checks a fork's PR in host mode. [Modes](@/docs/config.md#modes) has the full list of what a host check gets.

Hooks run only your own commits and pushes. The daemon runs a PR only when you, or a login `daemon_authors` on `main` lists, opened it and pushed its head, and never a fork's ([when a PR changes on GitHub](@/docs/how-it-works.md#when-a-pr-changes-on-github)).

## What the App can do on GitHub

The [shipcheck App](https://github.com/apps/shipcheck-run) asks for three permissions:

| Permission | For |
|---|---|
| Checks: write | Posting the job checks and the `shipcheck` summary |
| Pull requests: write | Finding the PRs of a commit and keeping one [report comment](@/docs/how-it-works.md#the-report-comment) on a PR |
| Metadata: read | Reading who pushed a PR's head, for the daemon |

It has no access to your code. A job never gets the App's token or any GitHub token.

## What the hosted service stores

`shipcheck post` sends the check text and the report comment to shipcheck's service, which posts them with the App and keeps no copy. The service checks that your GitHub user has write access to the repository first.

It stores, for each signed-in machine, a SHA-256 hash of the machine token, your GitHub user id and login, the machine name, and when the token was made and when it expires. It never stores the token itself, and it revokes the GitHub token from the sign-in once it knows who you are. A token stops working after 90 days without a post. It also counts failed sign-ins per IP address and locks an address out for 15 minutes after three.

Run records, results and logs stay on your machine. shipcheck sends no telemetry.

[Your own App](@/docs/install.md#your-own-app) posts from your machine with your App's key, with no service at all.

## Keys on your machine

On macOS, shipcheck keeps the machine token, the App key and the secrets files in your login keychain, where an agent running as you gets a prompt instead of the key. On Linux they are files of mode `0600`, which any program you run can read ([where shipcheck keeps its keys](@/docs/install.md#where-shipcheck-keeps-its-keys)).

## Report a vulnerability

Use GitHub's [private vulnerability reporting](https://github.com/gabrielkoerich/shipcheck/security/advisories/new). Do not open a public issue. You get an answer within a few days.
