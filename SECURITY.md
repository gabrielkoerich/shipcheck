# Security

## Report a vulnerability

Use GitHub's [private vulnerability reporting](https://github.com/gabrielkoerich/shipcheck/security/advisories/new). Do not open a public issue. You get an answer within a few days.

## What shipcheck can reach

shipcheck runs a branch's code on your machine and treats every job as untrusted code.

- act mode runs each job in a Docker container with its own checkout, no Docker socket, no host environment or GitHub token, and no network by default.
- Host mode runs each check as your user in an OS sandbox, `sandbox-exec` on macOS or bubblewrap on Linux. A check writes only its own clone, a new home and the repository's tool caches, reads the system and your toolchains but not the rest of your home, and has your network. Use it only for your own and your agents' work.
- A job with `engine = "colima"` runs in a VM of its own, cloned from a base VM shipcheck set up once and deleted after the result. The VM reaches this machine's loopback and the internet whatever `network` says, so use it only for a repository you trust.
- A host check written as `[host.checks.<name>]` with `sandbox = false` on main runs outside the sandbox only after you run `shipcheck trust` in that repository on this machine. Then any branch's code runs in it with your full access to your home, its keys and tokens, so trust only a repository where you trust every author `daemon_authors` lists. Until then it runs in the sandbox, and doctor and every run say so.
- The daemon runs only PRs that you, or a login `daemon_authors` on `main` lists, opened and pushed, and never a fork's.
- On your machine shipcheck keeps the machine token from `shipcheck login`, and the act job secrets you give each repository. On macOS they are in the login keychain. On Linux they are files of mode 0600 under `~/.local/state/shipcheck`.
- Checks post only through the [shipcheck App](https://github.com/apps/shipcheck-run) and the hosted service. The App's private key lives only in the hosted service, never on your machine.
- The shipcheck App has Checks: write, Pull requests: write and Metadata: read. It cannot read or change your code.
- The hosted service keeps a SHA-256 hash of each machine token with your GitHub user id, login and machine name. It also keeps who pushed each PR head, from the App's webhook, for 7 days, counts of failed authentications per IP address, and run records, only for an account with a team plan whose `shipcheck.toml` sets `[team] records = true`. It keeps no copy of the checks or comments it posts.

The details are at https://shipcheck.run/docs/security/.
