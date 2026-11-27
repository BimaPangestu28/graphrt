#!/usr/bin/env python3
"""Checks that the Kotlin and Swift examples of docs/mobile.md are the code the
SDK test targets compile and run.

In a test source, a region starts with `// snippet:start NAME` and ends with
`// snippet:end`. In the guide, a code block preceded by `<!-- snippet: NAME -->`
must equal that region (common indentation removed). Every region must be shown
and every marker must have a region, so neither side can drift.

Usage: python3 scripts/check-doc-snippets.py [--write]
  --write  copy the regions of the test sources into the guide's code blocks
"""

import pathlib
import re
import sys
import textwrap

ROOT = pathlib.Path(__file__).resolve().parent.parent
GUIDE = ROOT / "docs" / "mobile.md"
SOURCES = [
    *sorted((ROOT / "sdk" / "android" / "graphrt-jvm" / "src" / "test").rglob("*.kt")),
    *sorted((ROOT / "sdk" / "swift" / "Tests").rglob("*.swift")),
]

START = re.compile(r"^\s*// snippet:start (\S+)\s*$")
END = re.compile(r"^\s*// snippet:end\s*$")
MARKER = re.compile(r"^<!-- snippet: (\S+) -->\s*$")


def regions():
    found = {}
    for path in SOURCES:
        name, lines = None, []
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if (start := START.match(line)) is not None:
                if name is not None:
                    sys.exit(f"{path}:{number}: snippet `{start.group(1)}` starts inside `{name}`")
                name, lines = start.group(1), []
            elif END.match(line):
                if name is None:
                    sys.exit(f"{path}:{number}: snippet:end without a start")
                if name in found:
                    sys.exit(f"{path}:{number}: snippet `{name}` is defined twice")
                found[name] = textwrap.dedent("\n".join(lines)).strip("\n")
                name = None
            elif name is not None:
                lines.append(line)
        if name is not None:
            sys.exit(f"{path}: snippet `{name}` has no snippet:end")
    return found


def shown():
    blocks = {}
    lines = GUIDE.read_text(encoding="utf-8").splitlines()
    index = 0
    while index < len(lines):
        marker = MARKER.match(lines[index])
        index += 1
        if marker is None:
            continue
        name = marker.group(1)
        if index >= len(lines) or not lines[index].startswith("```"):
            sys.exit(f"{GUIDE.name}:{index}: `<!-- snippet: {name} -->` must be followed by a code block")
        index += 1
        body = []
        while index < len(lines) and not lines[index].startswith("```"):
            body.append(lines[index])
            index += 1
        index += 1
        blocks[name] = "\n".join(body).strip("\n")
    return blocks


def write(code):
    lines = GUIDE.read_text(encoding="utf-8").splitlines()
    out, index = [], 0
    while index < len(lines):
        line = lines[index]
        out.append(line)
        index += 1
        marker = MARKER.match(line)
        if marker is None or marker.group(1) not in code:
            continue
        if index < len(lines) and lines[index].startswith("```"):
            out.append(lines[index])
            index += 1
            while index < len(lines) and not lines[index].startswith("```"):
                index += 1
            out.extend(code[marker.group(1)].splitlines())
    GUIDE.write_text("\n".join(out) + "\n", encoding="utf-8")


def main():
    code = regions()
    if sys.argv[1:] == ["--write"]:
        write(code)
    elif sys.argv[1:]:
        sys.exit("usage: python3 scripts/check-doc-snippets.py [--write]")
    docs = shown()
    problems = []
    for name in sorted(set(code) - set(docs)):
        problems.append(f"snippet `{name}` is not shown in docs/mobile.md")
    for name in sorted(set(docs) - set(code)):
        problems.append(f"docs/mobile.md shows snippet `{name}`, which no test source defines")
    for name in sorted(set(code) & set(docs)):
        if code[name] != docs[name]:
            problems.append(f"snippet `{name}` in docs/mobile.md differs from the test source")
    if problems:
        print("\n".join(problems), file=sys.stderr)
        print("copy the regions of the test sources into the guide", file=sys.stderr)
        return 1
    print(f"{len(code)} doc snippets match the compiled sources")
    return 0


if __name__ == "__main__":
    sys.exit(main())
