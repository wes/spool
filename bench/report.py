#!/usr/bin/env python3
"""One table: every client measured on this machine, side by side.

    python3 bench/report.py ~/spool-bench/results/<machine>
    python3 bench/report.py ~/spool-bench/results/<machine> --markdown
    python3 bench/report.py --calibration ~/spool-bench/results/<machine>/calibration.jsonl

Reads the `<client>.jsonl` files `run.sh` writes and takes, for each client,
the most recent session only — an older session was taken against a different
build or a different setting and is history, not a second opinion.

Each cell is `first / settle` in milliseconds, medians. `first` is something
changed; `settle` is it stopped changing. Quoting only `first` lets a client
that paints a spinner quickly look fast.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from pathlib import Path

ACTIONS = ["open-message", "next-message", "scroll", "search", "launch", "quit"]
ORDER = ["spool", "applemail", "mailmate", "thunderbird", "canary", "airmail", "neomutt"]
NAMES = {
    "spool": "Spool",
    "applemail": "Apple Mail",
    "mailmate": "MailMate",
    "thunderbird": "Thunderbird",
    "canary": "Canary",
    "airmail": "Airmail",
    "neomutt": "neomutt",
}


def read_lines(path: Path) -> list[dict]:
    out = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            # A run that died halfway costs its own line, not the file.
            continue
    return out


def latest_session(records: list[dict]) -> tuple[dict | None, list[dict]]:
    start = None
    for i, r in enumerate(records):
        if r.get("event") == "session":
            start = i
    if start is None:
        return None, records
    return records[start], records[start + 1 :]


def sessions(records: list[dict]) -> list[tuple[dict, list[dict]]]:
    out: list[tuple[dict, list[dict]]] = []
    for r in records:
        if r.get("event") == "session":
            out.append((r, []))
        elif out:
            out[-1][1].append(r)
    return out


def current(records: list[dict]) -> tuple[dict | None, list[dict]]:
    """The latest session, plus anything later sessions of the same version
    measured that it did not: `run.sh launch` adds launch and quit as a session
    of their own, and should not hide the rest."""
    all_sessions = sessions(records)
    if not all_sessions:
        return None, []
    head, _ = all_sessions[-1]
    merged: dict[str, dict] = {}
    statics: dict | None = None
    for session, rows in all_sessions:
        if session.get("version") != head.get("version"):
            continue
        for r in rows:
            if r.get("event") == "summary":
                merged[r.get("label")] = r
            elif r.get("event") == "static":
                statics = r
    return head, list(merged.values()) + ([statics] if statics else [])


def fmt(v) -> str:
    if v is None:
        return "—"
    return f"{v:,.0f}"


def cell(summary: dict | None) -> str:
    if not summary:
        return "—"
    first, settle = summary.get("first_ms_median"), summary.get("settle_ms_median")
    captured, runs = summary.get("captured", 0), summary.get("runs", 0)
    text = f"{fmt(first)} / {fmt(settle)}"
    # A run that saw no change timed out. A median over the ones that did is
    # still a number, but not one to quote without saying how many were lost.
    if captured < runs:
        text += f" ({captured}/{runs})"
    return text


def table(rows: list[list[str]], markdown: bool) -> str:
    if markdown:
        lines = ["| " + " | ".join(rows[0]) + " |", "|" + "---|" * len(rows[0])]
        lines += ["| " + " | ".join(r) + " |" for r in rows[1:]]
        return "\n".join(lines)
    widths = [max(len(r[i]) for r in rows) for i in range(len(rows[0]))]
    out = []
    for n, r in enumerate(rows):
        out.append("  ".join(c.ljust(widths[i]) if i == 0 else c.rjust(widths[i]) for i, c in enumerate(r)))
        if n == 0:
            out.append("  ".join("-" * w for w in widths))
    return "\n".join(out)


def calibration(path: Path, markdown: bool) -> str:
    records = read_lines(path)
    _, records = latest_session(records)
    rows = [["asked", "target fired", "instrument read", "overhead"]]
    for delay in (50, 200, 800):
        label = f"calibrate/{delay}"
        fired = [r["actual_ms"] for r in records if r.get("event") == "fired" and r.get("label") == label]
        read = [r["first_ms"] for r in records
                if r.get("event") == "run" and r.get("label") == label and r.get("first_ms") is not None]
        if not fired or not read:
            continue
        f, m = statistics.median(fired), statistics.median(read)
        rows.append([f"{delay} ms", f"{f:.2f} ms", f"{m:.2f} ms", f"+{m - f:.1f} ms"])
    if len(rows) == 1:
        return "no calibration runs found"
    return table(rows, markdown)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("results", type=Path, nargs="?")
    ap.add_argument("--markdown", action="store_true")
    ap.add_argument("--calibration", type=Path)
    args = ap.parse_args()

    if args.calibration:
        print(calibration(args.calibration, args.markdown))
        return 0
    if not args.results or not args.results.is_dir():
        ap.error("give the results directory for a machine")

    clients: dict[str, tuple[dict, list[dict]]] = {}
    for path in sorted(args.results.glob("*.jsonl")):
        if path.stem == "calibration":
            continue
        session, records = current(read_lines(path))
        if session:
            clients[path.stem] = (session, records)
    if not clients:
        print(f"nothing measured yet in {args.results}")
        return 1

    order = [c for c in ORDER if c in clients] + sorted(c for c in clients if c not in ORDER)
    head = ["ms, first / settle"] + [NAMES.get(c, c) for c in order]
    rows = [head]
    for action in ACTIONS:
        row = [action]
        for c in order:
            _, records = clients[c]
            summary = next((r for r in records if r.get("event") == "summary"
                            and r.get("label") == f"{c}/{action}"), None)
            row.append(cell(summary))
        rows.append(row)

    statics = {c: next((r for r in clients[c][1] if r.get("event") == "static"), {}) for c in order}
    def memory(st: dict) -> str:
        m = st.get("memory") or {}
        if m.get("total_kb"):
            return fmt(m["total_kb"] / 1024)
        # Sessions from before footprints were taken: RSS, which is not the
        # same quantity and is marked so.
        return f"({fmt(st['rss_kb'] / 1024)} rss)" if st.get("rss_kb") else "—"
    rows.append(["memory, MB (with helpers)"] + [memory(statics[c]) for c in order])
    rows.append(["idle CPU, % of a core"] + [
        f"{statics[c]['idle_cpu_pct']:.1f}" if statics[c].get("idle_cpu_pct") is not None else "—" for c in order])
    rows.append(["store on disk, MB"] + [
        fmt(statics[c].get("store_kb", 0) / 1024) if statics[c].get("store_kb") else "—" for c in order])

    any_session = clients[order[0]][0]
    print(f"{any_session.get('machine')} ({any_session.get('model')}), macOS {any_session.get('os')}, "
          f"display {any_session.get('display')}, window {any_session.get('window')}, "
          f"corpus {any_session.get('corpus', '50k')}")
    print(table(rows, args.markdown))
    print()
    for c in order:
        s = clients[c][0]
        print(f"  {NAMES.get(c, c)} {s.get('version') or ''}".rstrip())

    cal = args.results / "calibration.jsonl"
    if not cal.exists():
        # A second corpus is the same machine and the same instrument.
        machine = any_session.get("machine")
        if machine and (args.results.parent / machine / "calibration.jsonl").exists():
            cal = args.results.parent / machine / "calibration.jsonl"
    print()
    if cal.exists():
        print("Instrument overhead on this machine (identical for every client, so it cancels in a")
        print("comparison; anything within a frame or two of it is at the floor):")
        print(calibration(cal, args.markdown))
    else:
        print("Not calibrated on this machine: run `bench/run.sh calibrate` before quoting these.")
    print("Memory is physical footprint, as Activity Monitor shows it, including the WebKit processes")
    print("each client renders mail in. Apple Mail's store excludes the shared Envelope Index.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
