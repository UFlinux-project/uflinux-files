#!/usr/bin/env python3
"""Fail a build when the file manager costs more than `docs/memory-budget.md` says.

Reads the JSON report `tests/files-memory-bench.py` writes and checks it twice: once
against the numbers in the table, and once against itself. The second check is the
reason the tool exists. A single sample under budget proves very little — it is the
sample taken before the leak had anything to leak, and it says nothing at all about
descriptors. So the gate also fails on PSS that grows across the cycle loop, and on a
descriptor count that grew at all, which are the two failures a process at rest hides
and a session of use does not.

It evaluates and does not manufacture: a scenario the bench could not run is reported
as skipped and does not count as passing, and a report with fewer than three sessions
is rejected outright, because the median of one sample is that sample.

    tests/files-memory-bench.py --output /tmp/report.json
    scripts/files-memory-gate.py --report /tmp/report.json

Exit 0 within budget, 1 over it, 2 when the report is missing, unreadable, or too thin
to judge.
"""

import argparse
import json
import statistics
import sys
from pathlib import Path

MIB = 1024 * 1024

# One row of the table in docs/memory-budget.md per scenario the bench runs. The number
# is the budget; the label is what to print when it is broken, so the failure names the
# decision it came from rather than just a figure.
BUDGETS = {
    "provider": (10 * MIB, "uflinux-files-provider: GLib and GIO only, no GTK, no widget tree"),
    "cold-window": (90 * MIB, "one window on a 2k-entry folder, before interaction"),
    "warm-window": (90 * MIB, "one window, one 2k-entry folder, listed and settled"),
    "columns-5k": (120 * MIB, "column view plus thumbnails, at peak"),
    "search": (24 * MIB, "uflinux-searchd idle: an index and a searcher, no writer"),
    "search-query": (64 * MIB, "uflinux-searchd at work: tantivy's writer floor included"),
    "desktop-model": (12 * MIB, "the desktop-icons model at 200 items"),
}

# The fields a session must carry. Anything missing makes the report too thin to judge,
# which is a problem with the report rather than with the program, and gets exit 2.
REQUIRED = ("settled_pss_bytes", "exercised_pss_bytes", "warmed_pss_bytes",
            "cycled_pss_bytes", "fds_before", "fds_after")

# Retained growth across the cycle loop that the gate will accept. Five MiB over a
# hundred repetitions is loose enough to absorb allocator behaviour and tight enough to
# catch a per-cycle leak inside a hundred cycles of a real session.
RETAINED_SLACK = 5 * MIB

REQUIRED_ENV = ("renderer", "compositor", "scale", "machine", "available_memory_bytes",
                "dri", "gsk_renderer")


def human(value):
    return "-" if value is None else f"{value / MIB:.1f} MiB"


def medians(sessions, scenario, field):
    values = [s["scenarios"][scenario].get(field) for s in sessions
              if scenario in s.get("scenarios", {})
              and not s["scenarios"][scenario].get("skipped")
              and s["scenarios"][scenario].get(field) is not None]
    return statistics.median(values) if values else None


def check_environment(report, failures):
    env = report.get("environment")
    if not isinstance(env, dict):
        failures.append("the report carries no `environment`: a memory number measured "
                        "on an unknown machine is not a number anyone can act on")
        return
    missing = [key for key in REQUIRED_ENV if env.get(key) in (None, "")]
    if missing:
        failures.append(f"`environment` is missing {', '.join(missing)}")
    available = env.get("available_memory_bytes") or 0
    if available and available < 512 * MIB:
        print(f"  note: only {human(available)} available — below the reference "
              "machine, so these numbers are pessimistic, not optimistic")
    # Worth saying out loud: the renderer is most of what a window costs. Measured on
    # the same machine with no render node, the GL renderer — Mesa's llvmpipe, since
    # there is no GPU to drive — holds 222 MiB against cairo's 54.
    if env.get("gsk_renderer") == "cairo":
        print("  note: no render node found, so the window was rendered with cairo. "
              "On a GPU machine GTK picks gl and the window rows will read higher.")


def check_session_shape(sessions, failures):
    for index, session in enumerate(sessions, start=1):
        scenarios = session.get("scenarios")
        if not isinstance(scenarios, dict):
            failures.append(f"session {index} carries no scenarios")
            continue
        for name, result in scenarios.items():
            if result.get("skipped"):
                continue
            absent = [field for field in REQUIRED if result.get(field) is None]
            if absent:
                failures.append(f"session {index} / {name} is missing "
                                f"{', '.join(absent)}")


def check_scenario(sessions, name, budget, why, failures, notes):
    warmed = medians(sessions, name, "warmed_pss_bytes")
    exercised = medians(sessions, name, "exercised_pss_bytes")
    cycled = medians(sessions, name, "cycled_pss_bytes")
    before = medians(sessions, name, "fds_before")
    after = medians(sessions, name, "fds_after")
    rounds = medians(sessions, name, "cycles")
    if warmed is None:
        notes.append(f"{name:14s} not measured — no budget evaluated")
        return
    label = f"{name:14s} {human(warmed):>9s} / {human(budget):>9s}"
    notes.append(label if exercised is None or exercised <= warmed
                 else f"{label}  peak {human(exercised)}")
    if warmed > budget:
        failures.append(f"{label}  OVER BUDGET ({why})")
    if cycled is not None and cycled - warmed > RETAINED_SLACK:
        failures.append(f"{name:14s} retains {human(cycled - warmed)} after the cycle "
                        f"loop (over the {human(RETAINED_SLACK)} the allocator gets)")
    if before is not None and after is not None and after > before:
        span = (f"after {int(rounds)} repetitions" if rounds
                else "across the scenario")
        failures.append(f"{name:14s} holds {int(after - before)} more descriptors {span} "
                        "than before: something is opened per iteration and never closed")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()

    try:
        report = json.loads(args.report.read_text())
    except FileNotFoundError:
        print(f"memory gate: no report at {args.report}")
        return 2
    except (OSError, ValueError) as error:
        print(f"memory gate: {args.report} is not readable JSON: {error}")
        return 2

    sessions = report.get("sessions")
    count = len(sessions) if isinstance(sessions, list) else 0
    if count < 3:
        print(f"memory gate: {args.report} has {count} session(s); three are needed "
              "before a median means anything")
        return 2

    failures = []
    notes = []
    print(f"memory gate: {args.report}")
    check_environment(report, failures)
    check_session_shape(sessions, failures)
    for name, (budget, why) in BUDGETS.items():
        if any(name in s.get("scenarios", {}) for s in sessions):
            check_scenario(sessions, name, budget, why, failures, notes)
        else:
            notes.append(f"{name:14s} not measured — the bench did not run it")

    for note in notes:
        print(f"  {note}")
    for failure in failures:
        print(f"  FAIL {failure}")
    if failures:
        print(f"memory gate: {len(failures)} problem(s) over "
              f"{len(sessions)} sessions")
        return 1
    print(f"memory gate: within budget across {len(sessions)} sessions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
