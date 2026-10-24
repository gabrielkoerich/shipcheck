# Security

## Report a vulnerability

Use GitHub's [private vulnerability reporting](https://github.com/gabrielkoerich/shipcheck/security/advisories/new). Do not open a public issue. You get an answer within a few days.

## What shipcheck can reach

shipcheck runs a branch's code on your machine and treats every job as untrusted code.

- act mode runs each job in a Docker container with its own checkout, no Docker socket, no host environment or GitHub token, and no network by default.
- Host mode runs each check as your user in an OS sandbox, `sandbox-exec` on macOS or bubblewrap on Linux. A check writes only its own clone, a new home and the repository's tool caches, reads the system and your toolchains but not the rest of your home, and has your network. Use it only for your own and your agents' work.
- The daemon runs only PRs that you, or a login `daemon_authors` on `main` lists, opened and pushed, and never a fork's.
- The shipcheck App has Checks: write, Pull requests: write, Metadata: read and Contents: read. The daemon uses Contents: read only to read a branch's activity, which names who pushed a PR's head. No shipcheck code reads your code through it, and it changes no code.
- The hosted service keeps a SHA-256 hash of each machine token with your GitHub user id, login and machine name. It keeps no copy of the checks or comments it posts.

The details are at https://shipcheck.run/docs/security/.
