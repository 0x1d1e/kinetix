#!/usr/bin/env python3
"""Mirror canonical architecture and glossary pages into the checked-in wiki."""

import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
REPO = "https://github.com/0x1d1e/kinetix/blob/main/"
PAGES = {
    "docs/ARCHITECTURE.md": "docs/wiki/Architecture.md",
    "CONTEXT.md": "docs/wiki/Glossary.md",
}
LINKS = {
    "../CONTEXT.md": "Glossary",
    "../AGENTS.md": REPO + "AGENTS.md",
    "../pi-warden.md": REPO + "pi-warden.md",
    "protocol-v1-compatibility.md": REPO + "docs/protocol-v1-compatibility.md",
    "pi-compatibility.md": REPO + "docs/pi-compatibility.md",
    "wiki/Plugins.md": "Plugins",
    "wiki/Routing-and-Fallback.md": "Routing-and-Fallback",
    "../SECURITY.md": REPO + "SECURITY.md",
    "../deploy/README.md": REPO + "deploy/README.md",
    "archive/README.md": REPO + "docs/archive/README.md",
}
HEADER = (
    "<!-- GENERATED from {source} by scripts/render-doc-pages.py.\n"
    "     Edit the canonical docs file instead. -->\n\n"
)


def render(source: str) -> str:
    text = (ROOT / source).read_text()

    def replace_link(match: re.Match) -> str:
        label, target = match.group(1), match.group(2)
        return f"[{label}]({LINKS.get(target, target)})"

    text = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", replace_link, text)
    return HEADER.format(source=source) + text


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    stale = []
    for source, destination in PAGES.items():
        output = ROOT / destination
        expected = render(source)
        if args.check:
            current = output.read_text() if output.exists() else ""
            if current != expected:
                stale.append(destination)
        else:
            output.write_text(expected)
            print(destination)

    if stale:
        for destination in stale:
            print(f"{destination} is stale; run scripts/render-doc-pages.py", file=sys.stderr)
        return 1
    if args.check:
        print("Architecture and Glossary wiki mirrors are up to date")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
