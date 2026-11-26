#!/usr/bin/env python3
"""Changelog fragments: one small file per change instead of edits to
CHANGELOG.md, so pull requests in parallel never conflict there.

A fragment is `changelog.d/<slug>.<kind>.md`, where `kind` is one of the Keep
a Changelog sections (added, changed, deprecated, removed, fixed, security)
and the content is one or more Markdown bullets (`- ...`, continuation lines
indented by two spaces).

    scripts/changelog.py check                 # validate the fragments (CI)
    scripts/changelog.py preview               # Unreleased with the fragments merged in
    scripts/changelog.py release 0.7.0 [DATE]  # move Unreleased + fragments into a version section

`release` writes `## [X.Y.Z] - DATE` (DATE defaults to today, UTC) from the
`## [Unreleased]` section and every fragment, leaves an empty Unreleased
section, and deletes the fragments. Add the release's summary paragraph under
the new heading afterwards (CONTRIBUTING.md, "Releasing").
"""

from __future__ import annotations

import datetime
import os
import re
import sys
from pathlib import Path

KINDS: list[str] = ["added", "changed", "deprecated", "removed", "fixed", "security"]
HEADINGS: dict[str, str] = {kind: f"### {kind.capitalize()}" for kind in KINDS}
FRAGMENT_NAME = re.compile(r"^[a-z0-9][a-z0-9._-]*\.(" + "|".join(KINDS) + r")\.md$")
UNRELEASED = "## [Unreleased]"
IGNORED = {"README.md", ".gitkeep"}


class ChangelogError(Exception):
    """A fragment or the changelog is not in the expected shape."""


def fragments(directory: Path) -> list[tuple[str, Path]]:
    """Every fragment `(kind, path)`, in name order."""
    found: list[tuple[str, Path]] = []
    if not directory.is_dir():
        return found
    for path in sorted(directory.iterdir()):
        if path.name in IGNORED:
            continue
        match = FRAGMENT_NAME.match(path.name)
        if not match:
            raise ChangelogError(
                f"{path}: name it <slug>.<kind>.md with kind one of {', '.join(KINDS)}"
            )
        found.append((match.group(1), path))
    return found


def bullets(path: Path) -> str:
    """The fragment's bullets, checked: non-empty, starting with `- `."""
    text = path.read_text(encoding="utf-8").strip("\n")
    if not text.strip():
        raise ChangelogError(f"{path}: empty fragment")
    for number, line in enumerate(text.splitlines(), start=1):
        if line and not line.startswith(("- ", "  ")):
            raise ChangelogError(
                f"{path}:{number}: bullets only (`- ...`, continuation lines indented by two spaces)"
            )
    if not text.startswith("- "):
        raise ChangelogError(f"{path}: the first line must start a bullet (`- `)")
    return text


def split_unreleased(
    changelog: str,
) -> tuple[str, dict[str, list[str]], list[str], str]:
    """`(head, sections, prose, tail)`: what is before `## [Unreleased]`, its
    `### Kind` sections (bullet text per kind), any prose under the heading
    before the first section, and everything after it (older releases)."""
    start = changelog.find(UNRELEASED)
    if start < 0:
        raise ChangelogError(f"CHANGELOG.md has no `{UNRELEASED}` section")
    body_start = start + len(UNRELEASED)
    next_release = re.search(r"^## \[", changelog[body_start:], flags=re.MULTILINE)
    end = body_start + next_release.start() if next_release else len(changelog)
    head, body, tail = changelog[:start], changelog[body_start:end], changelog[end:]
    sections: dict[str, list[str]] = {}
    prose: list[str] = []
    current: str | None = None
    lines: list[str] = []

    def flush() -> None:
        text = "\n".join(lines).strip("\n")
        if current is None:
            if text.strip():
                prose.append(text)
        elif text.strip():
            sections.setdefault(current, []).append(text)

    for line in body.splitlines():
        heading = re.match(r"^### (\w+)\s*$", line)
        if heading:
            flush()
            kind = heading.group(1).lower()
            if kind not in KINDS:
                raise ChangelogError(
                    f"CHANGELOG.md: unknown section `{line}` under Unreleased"
                )
            current, lines = kind, []
        else:
            lines.append(line)
    flush()
    return head, sections, prose, tail


def merged(
    sections: dict[str, list[str]], found: list[tuple[str, Path]]
) -> dict[str, list[str]]:
    """The Unreleased sections with the fragments appended, per kind."""
    result = {kind: list(entries) for kind, entries in sections.items()}
    for kind, path in found:
        result.setdefault(kind, []).append(bullets(path))
    return result


def render(sections: dict[str, list[str]], prose: list[str]) -> str:
    """Section bodies in Keep a Changelog order."""
    parts: list[str] = list(prose)
    for kind in KINDS:
        entries = sections.get(kind)
        if entries:
            parts.append(HEADINGS[kind] + "\n\n" + "\n".join(entries))
    return "\n\n".join(parts)


def main(argv: list[str], root: Path) -> int:
    directory = root / "changelog.d"
    changelog_path = root / "CHANGELOG.md"
    if not argv or argv[0] not in {"check", "preview", "release"}:
        print(__doc__, file=sys.stderr)
        return 2
    command = argv[0]
    try:
        found = fragments(directory)
        for _, path in found:
            bullets(path)
        changelog = changelog_path.read_text(encoding="utf-8")
        head, sections, prose, tail = split_unreleased(changelog)
        if command == "check":
            print(f"changelog: {len(found)} fragment(s) ok")
            return 0
        body = render(merged(sections, found), prose)
        if command == "preview":
            print(f"{UNRELEASED}\n\n{body}".rstrip() + "\n")
            return 0
        if len(argv) < 2 or not re.fullmatch(
            r"\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", argv[1]
        ):
            raise ChangelogError("release needs a version, e.g. `release 0.7.0`")
        if not body.strip():
            raise ChangelogError(
                "nothing to release: Unreleased is empty and there is no fragment"
            )
        version = argv[1]
        date = (
            argv[2]
            if len(argv) > 2
            else datetime.datetime.now(datetime.UTC).date().isoformat()
        )
        if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", date):
            raise ChangelogError(f"date `{date}` is not YYYY-MM-DD")
        released = f"{head}{UNRELEASED}\n\n## [{version}] - {date}\n\n{body}\n\n{tail.lstrip()}"
        changelog_path.write_text(released.rstrip("\n") + "\n", encoding="utf-8")
        for _, path in found:
            path.unlink()
        print(f"changelog: released {version} with {len(found)} fragment(s)")
        return 0
    except ChangelogError as error:
        print(f"changelog: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    # CHANGELOG_ROOT: another checkout (the tests use a fixture).
    sys.exit(
        main(
            sys.argv[1:],
            Path(
                os.environ.get("CHANGELOG_ROOT")
                or Path(__file__).resolve().parent.parent
            ),
        )
    )
