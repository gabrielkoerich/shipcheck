+++
title = "Agents"
description = "How a coding agent learns to run shipcheck instead of the tests and git commit, with any agent."
weight = 30
+++

An agent should run `shipcheck run` instead of the project's tests, and `shipcheck commit -m "<subject>"` instead of `git commit`. Then the run it waits for is the PR's check, and every commit is a tree that already passed. The agent works on a branch, since `shipcheck commit` and `shipcheck post` refuse on the default branch. shipcheck works the same with any agent, or with a person. It writes no agent config, hooks or permissions, since it does not know which agent you use or how you set it up.

## Instructions

`shipcheck init` writes a short block between `<!-- shipcheck:start v3 -->` and `<!-- shipcheck:end -->` markers into the files agents read. Commit the files it changes. Running it again replaces only the text between the markers.

| File | When |
|---|---|
| `AGENTS.md` | Always. Most agents read it |
| `CLAUDE.md`, `.claude/CLAUDE.md` | When it exists and has no `@AGENTS.md` line |
| `GEMINI.md` | When it exists |
| `.github/copilot-instructions.md` | When it exists |

If your agent reads another file, point it at `AGENTS.md` in that agent's own settings.

[SKILL.md](https://shipcheck.run/SKILL.md) says how to run shipcheck, read a result and fix a failed check. Put it wherever your agent loads skills, if it does.

## Git hooks

`shipcheck install` installs the `pre-push` and `post-commit` git hooks for the clone. They start a run in the background, so an agent that runs `git commit` instead of `shipcheck commit` still gets its check, after one more run. They never block git.

## Your agent's sandbox

shipcheck keeps its results in `SHIPCHECK_HOME`, a folder your user can write. Nothing in shipcheck stops an agent from writing a fake result there. If your agent has a sandbox, keep it on and keep that folder outside its writable paths. See [limits](@/docs/limits.md).

## Upgrading from an older version

Older versions wrote hooks, permission rules and sandbox settings into `.claude/settings.local.json`, `.codex/hooks.json`, `.codex/rules/shipcheck.rules`, `.cursor/hooks.json`, `.gemini/settings.json` and `.github/hooks/shipcheck.json`. Run `shipcheck install` in each clone. It removes only the entries shipcheck wrote, keeps your own settings, and deletes a file only when nothing of yours is left in it. A file your team tracks in git is left alone with a note.

## Check it

`shipcheck doctor` says whether each instruction file has the current block.
