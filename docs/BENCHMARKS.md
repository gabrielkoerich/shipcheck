# Benchmarks

Time to result for one commit, shipcheck against GitHub's hosted runners, both started at the same moment.

## Method

- Machine: Apple silicon Mac, 14 CPUs, Docker Desktop with 8 GB for its VM.
- Repository: bean, the author's private Python repository. Its `ci.yml` runs one job: apt packages, setup-python, setup-uv, `uv sync`, basedpyright and pytest with about 2,000 tests.
- Each round commits a one-line comment to a `.py` file, pushes the branch so GitHub's CI starts, and starts `shipcheck run` at the same moment. shipcheck's time is from its start to its result. GitHub's is from the run's creation to its completion, from `gh run list`.
- Cold is the first run on the machine. Warm is a later run of a new commit, with shipcheck's warm checkout and cache volumes in place.
- A tree already tested, after a rebase or a squash, answers from shipcheck's cache without a run.

## Results

| Round | shipcheck | GitHub |
|---|---|---|
| Cold, 2026-10-21 | 716 s, attempt 1 failed and the retry passed | 367 s |
| Warm, 2026-10-21 | 468 s | 316 s |
| Tree already tested | under 1 s | a full run |

A single run is slower than GitHub's on this workflow today. Where it loses: `apt-get` downloads the package index on every run, setup-uv empties uv's cache at the end of every job, and pytest reads its virtualenv through the macOS bind mount. Work on all three is in progress.
