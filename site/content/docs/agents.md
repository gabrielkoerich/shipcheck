+++
title = "Agents"
description = "What shipcheck sets up so coding agents run shipcheck instead of the tests and git commit."
weight = 30
+++

An agent should run `shipcheck run` instead of the project's tests, and `shipcheck commit -m "<subject>"` instead of `git commit`. Then the run it waits for is the PR's check, and every commit is a tree that already passed. shipcheck sets this up in two layers.

## Instructions

`shipcheck init` writes a short block between `<!-- shipcheck:start v1 -->` and `<!-- shipcheck:end -->` markers. Commit the files it changes. Running it again replaces only the text between the markers.

| File | When |
|---|---|
| `AGENTS.md` | Always. Codex, Cursor, Copilot and others read it |
| `CLAUDE.md`, `.claude/CLAUDE.md` | When it exists and has no `@AGENTS.md` line, since Claude Code then reads only CLAUDE.md |
| `GEMINI.md` | When it exists. Otherwise `AGENTS.md` goes into `context.fileName` in `.gemini/settings.json`, when that file exists |
| `.github/copilot-instructions.md` | When it exists |
| `.aider.conf.yml` | When it exists: `read: AGENTS.md` |

`shipcheck install` also puts the shipcheck skill in `~/.claude/skills/shipcheck/` and `~/.agents/skills/shipcheck/`, for the agents installed on the machine.

## Hooks

`shipcheck install` writes hooks for each agent it finds, by its binary on `PATH` or its config directory. These files belong to your clone: shipcheck adds them to `.git/info/exclude`, and leaves a file alone if your team tracks it.

| Agent | File |
|---|---|
| Claude Code | `.claude/settings.local.json` |
| Codex | `.codex/hooks.json` and `.codex/rules/shipcheck.rules` |
| Cursor | `.cursor/hooks.json` |
| Gemini CLI | `.gemini/settings.json` |
| Copilot CLI | `.github/hooks/shipcheck.json` |

Codex and Gemini CLI ask you to trust a repository's hooks the first time. Accept them, or run `/hooks` in Codex.

Once `shipcheck.toml` is on `main`, the hooks:

- deny a test runner, such as `pytest`, `cargo test`, `npm test`, `go test`, `just test`, `make test`, `jest`, `vitest` or `act`, and tell the agent to run `shipcheck run`. Builds, linters and formatters run as usual.
- deny `git commit` and tell the agent to use `shipcheck commit`. A merge, revert, cherry-pick or rebase in progress may still finish with `git commit`.
- stop the agent from ending its turn, once, when it has uncommitted changes, or when its last commit has no passing result. The hook only reads results, it never starts a run.

In every repository they deny writing into shipcheck's own folder, so an agent cannot forge a result. This catches habits, not a determined script: see [limits](@/docs/limits.md).

The hooks fail open. If shipcheck is missing or errors, the agent goes on.

To run tests yourself in an agent session, start the agent with `SHIPCHECK_ALLOW_TESTS=1`.

## Check it

`shipcheck doctor` prints a line for each agent it finds: whether the instructions, the hooks and the skill are current, and what protects shipcheck's folder from that agent.
