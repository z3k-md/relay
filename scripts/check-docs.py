#!/usr/bin/env python3
"""Keep the docs consistent with each other and with the code.

Checks:
  - relative Markdown links point at files that exist;
  - every decision number cited in code or docs (`D16`) has an entry in
    docs/DECISIONS.md;
  - every `DESIGN §N` / `DESIGN §N.M` reference names a section that exists
    (a bare `§N` counts too inside DESIGN.md and DECISIONS.md);
  - the index at the top of DECISIONS.md lists every entry;
  - every proposal in docs/proposals/ has a `**Status:**` line.

`--write` regenerates the DECISIONS.md index instead of failing on it.
Standard library only; run from anywhere in the repo.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DECISIONS = ROOT / "docs" / "DECISIONS.md"
DESIGN = ROOT / "docs" / "DESIGN.md"
INDEX_START = "<!-- index:start -->"
INDEX_END = "<!-- index:end -->"

LINK = re.compile(r"\]\(([^)\s]+)\)")
DNUM = re.compile(r"(?<![\w-])D(\d{1,3})(?![\w-])")
DNUM_RANGE = re.compile(r"(?<![\w-])D(\d{1,3})[–-]D?(\d{1,3})(?![\w-])")
DESIGN_REF = re.compile(r"DESIGN(?:\.md)?`?(?:\]\([^)]*\))?\s+§(\d+)(?:\.(\d+))?")
BARE_REF = re.compile(r"§(\d+)(?:\.(\d+))?")
HEADING = re.compile(r"^## D(\d+)\. (.+)$", re.M)
CODE_EXT = {".rs", ".ts", ".vue", ".md", ".yml", ".toml", ".sh", ".py"}


def tracked_files():
    out = subprocess.run(
        ["git", "ls-files"], cwd=ROOT, capture_output=True, text=True, check=True
    ).stdout
    return [ROOT / p for p in out.splitlines() if Path(p).suffix in CODE_EXT]


def decision_entries(text):
    return {int(n): title for n, title in HEADING.findall(text)}


def build_index(entries):
    lines = [INDEX_START, "", "| | Decision |", "| --- | --- |"]
    for n in sorted(entries):
        anchor = re.sub(r"[^\w\- ]", "", f"d{n}. {entries[n]}".lower()).replace(" ", "-")
        lines.append(f"| D{n} | [{entries[n]}](#{anchor}) |")
    lines += ["", INDEX_END]
    return "\n".join(lines)


def design_sections(text):
    """Top-level section number -> its body."""
    parts = re.split(r"^# (\d+)\. .*$", text, flags=re.M)
    return {int(parts[i]): parts[i + 1] for i in range(1, len(parts) - 1, 2)}


def subsection_exists(body, n, m):
    return re.search(rf"^(## {n}\.{m}\b|{m}\. )", body, re.M) is not None


def main():
    write = "--write" in sys.argv[1:]
    errors = []

    decisions_text = DECISIONS.read_text(encoding="utf-8")
    entries = decision_entries(decisions_text)
    sections = design_sections(DESIGN.read_text(encoding="utf-8"))

    # Index.
    index = build_index(entries)
    found = re.search(
        re.escape(INDEX_START) + r".*?" + re.escape(INDEX_END), decisions_text, re.S
    )
    if not found:
        errors.append(f"docs/DECISIONS.md: no index between {INDEX_START} and {INDEX_END}")
    elif found.group(0) != index:
        if write:
            decisions_text = decisions_text.replace(found.group(0), index)
            DECISIONS.write_text(decisions_text, encoding="utf-8")
            print("docs/DECISIONS.md: index rewritten")
        else:
            errors.append("docs/DECISIONS.md: index is out of date (run with --write)")

    for proposal in sorted((ROOT / "docs" / "proposals").glob("*.md")):
        if "**Status:**" not in proposal.read_text(encoding="utf-8")[:2000]:
            errors.append(f"{proposal.relative_to(ROOT)}: no **Status:** line near the top")

    for path in tracked_files():
        rel = path.relative_to(ROOT)
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError):
            continue
        is_md = path.suffix == ".md"
        for lineno, line in enumerate(text.splitlines(), 1):
            where = f"{rel}:{lineno}"
            if is_md:
                for target in LINK.findall(line):
                    if "://" in target or target.startswith(("#", "mailto:")):
                        continue
                    file_part = target.split("#", 1)[0]
                    if file_part and not (path.parent / file_part).exists():
                        errors.append(f"{where}: broken link {target}")
            if path == DECISIONS and line.startswith(("## D", "| D")):
                continue
            cited = {int(n) for n in DNUM.findall(line)}
            for a, b in DNUM_RANGE.findall(line):
                cited.update(range(int(a), int(b) + 1))
            for n in sorted(cited):
                if n not in entries:
                    errors.append(f"{where}: D{n} has no entry in docs/DECISIONS.md")
            refs = DESIGN_REF.findall(line)
            if path in (DESIGN, DECISIONS):
                # Bare § in these two files always means a DESIGN section.
                refs = BARE_REF.findall(line)
            for n, m in refs:
                n = int(n)
                if n not in sections:
                    errors.append(f"{where}: DESIGN §{n} does not exist")
                elif m and not subsection_exists(sections[n], n, m):
                    errors.append(f"{where}: DESIGN §{n}.{m} does not exist")

    for e in errors:
        print(e)
    if errors:
        print(f"\n{len(errors)} problem(s). See AGENTS.md, 'Keeping docs true'.")
        return 1
    print("docs ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
