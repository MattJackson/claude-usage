#!/usr/bin/env python3
"""Render a demo-state fixture template into a real state.json for hero
screenshot capture.

Why this exists: usagio's countdown.rs only treats a usage window as
"locked" (and swaps the menu row's percentage for a countdown) when that
window's reset timestamp is BOTH >= 99.5% used AND still in the future
relative to wall-clock time when the menu renders. A static ISO8601
timestamp baked into a fixture file would go stale the moment it's in the
past, silently turning a "locked" screenshot fixture into a plain "healthy"
one. So the locked fixtures (fixtures/weekly-locked.json,
fixtures/session-locked.json, fixtures/mixed.json) use relative tokens like
"NOW+2d14h" instead of a fixed date; this script resolves those tokens to
an absolute UTC timestamp computed at render time, a few seconds before
usagio actually reads the file.

Token grammar: NOW+ followed by any combination of <N>d, <N>h, <N>m (each
optional, in that order), e.g. "NOW+2d14h", "NOW+3h15m", "NOW+45m". Any
plain ISO8601 string with no NOW+ token is passed through unchanged.

Usage: render_fixture.py <template.json> <output.json>
"""
from __future__ import annotations

import argparse
import datetime
import re
import sys

TOKEN_RE = re.compile(r"NOW\+((?:\d+d)?(?:\d+h)?(?:\d+m)?)")
PART_RE = re.compile(r"(\d+)([dhm])")


def resolve(match: "re.Match[str]") -> str:
    spec = match.group(1)
    if not spec:
        raise ValueError(f"empty NOW+ duration in token: {match.group(0)!r}")
    days = hours = minutes = 0
    for amount, unit in PART_RE.findall(spec):
        n = int(amount)
        if unit == "d":
            days = n
        elif unit == "h":
            hours = n
        elif unit == "m":
            minutes = n
    delta = datetime.timedelta(days=days, hours=hours, minutes=minutes)
    when = datetime.datetime.now(datetime.timezone.utc) + delta
    return when.strftime("%Y-%m-%dT%H:%M:%SZ")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("template", help="path to fixtures/<variant>.json")
    parser.add_argument("output", help="path to write the rendered state.json to")
    args = parser.parse_args(argv)

    with open(args.template, "r", encoding="utf-8") as fh:
        text = fh.read()

    rendered = TOKEN_RE.sub(resolve, text)

    # Fail loudly rather than writing something usagio can't load: confirm
    # the result is still valid JSON and no stray "NOW+" token survived
    # (e.g. a typo like "NOW+2dd" that TOKEN_RE didn't fully consume).
    import json

    json.loads(rendered)
    if "NOW+" in rendered:
        raise SystemExit(
            f"error: unresolved NOW+ token remains in rendered output "
            f"(check the duration grammar in {args.template})"
        )

    with open(args.output, "w", encoding="utf-8") as fh:
        fh.write(rendered)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
