"""shipcheck's pytest shard plugin, loaded with `-p shipcheck_shard` by the step shipcheck wraps.

Every shard collects the whole suite, keeps the files the greedy split gives it, and writes a report
the host checks: the digest of every collected node id, and each item it ran with its outcome
"""

import hashlib
import json
import os

import pytest

# Taken out at import, so a pytest that a test starts runs whole, with the user's own addopts
_SHARD = os.environ.pop("SHIPCHECK_SHARD", "")
_DIR = os.environ.pop("SHIPCHECK_SHARD_DIR", "")
_ADDOPTS = os.environ.pop("SHIPCHECK_ADDOPTS", None)

if _ADDOPTS:
    os.environ["PYTEST_ADDOPTS"] = _ADDOPTS
else:
    os.environ.pop("PYTEST_ADDOPTS", None)

_state = {"digest": "", "collected": 0, "mine": 0, "items": {}}


def split(items, index, count):
    """The items of shard `index`, every count-th in collection order, so slow tests next to each other in one file spread out"""
    return items[index::count]


def _parse():
    index, count = (int(p) for p in _SHARD.split("/"))

    if not 0 <= index < count:
        raise ValueError(_SHARD)

    return index, count


def pytest_configure(config):
    if config.pluginmanager.hasplugin("xdist") and config.getoption("numprocesses", None):
        raise pytest.UsageError("shipcheck: shard_tests cannot split a run that uses pytest-xdist")


@pytest.hookimpl(wrapper=True, trylast=True)
def pytest_collection_modifyitems(session, config, items):
    # After every other hook, so -m, -k and --deselect have already filtered
    result = yield
    index, count = _parse()
    ids = sorted(i.nodeid for i in items)
    _state["digest"] = hashlib.sha256("\n".join(ids).encode()).hexdigest()
    _state["collected"] = len(ids)
    mine = split(items, index, count)
    keep = {id(i) for i in mine}
    others = [i for i in items if id(i) not in keep]

    if others:
        config.hook.pytest_deselected(items=others)

    items[:] = mine
    _state["mine"] = len(mine)
    return result


def pytest_runtest_logreport(report):
    outcome = _state["items"].setdefault(report.nodeid, "passed")

    if report.failed:
        _state["items"][report.nodeid] = "failed"
    elif report.skipped and outcome == "passed":
        _state["items"][report.nodeid] = "skipped"


@pytest.hookimpl(trylast=True)
def pytest_sessionfinish(session, exitstatus):
    index, count = _parse()

    # An empty shard of a suite that has tests did its part
    if exitstatus == pytest.ExitCode.NO_TESTS_COLLECTED and _state["mine"] == 0 and _state["collected"] > 0:
        session.exitstatus = pytest.ExitCode.OK

    report = {
        "shard": index,
        "shards": count,
        "collected": _state["collected"],
        "digest": _state["digest"],
        "exit": int(session.exitstatus),
        "items": _state["items"],
    }

    with open(os.path.join(_DIR, f"{index}.json"), "w") as f:
        json.dump(report, f)

    failed = [n for n, o in _state["items"].items() if o == "failed"]

    with open(os.path.join(_DIR, f"{index}.summary"), "w") as f:
        f.write(f"{len(_state['items'])} items\n")
        f.writelines(f"{n}\n" for n in failed)
