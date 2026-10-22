#!/usr/bin/env python3
"""Print a completion spec (completions/<command>.json) for a clap CLI from its `-h` output.

For CLIs that can't answer completion requests themselves (no cobra `__complete`, no clap
`CompleteEnv`): walks `<command> [subcommand...] -h`, reading clap's Commands, Options and
possible values. Only help is ever run. Regenerate after the CLI changes:

    python3 scripts/completion-spec-from-help.py -o completions/codex.json codex "OpenAI's coding agent"

With -o the file is replaced only when every help request succeeded.
"""

import argparse
import json
import os
import re
import subprocess
import sys

MAX_DEPTH = 4
ENTRY = re.compile(r"^(\S.*?)(?:\s{2,}(\S.*))?$")
BRACKETS = re.compile(r"\s*\[(aliases|possible values|default|env): ([^\]]*)\]")


def help_text(argv):
    """`argv -h`; exits when it fails, so a partial spec is never printed."""
    cmd = " ".join([*argv, "-h"])
    try:
        out = subprocess.run(
            [*argv, "-h"],
            capture_output=True,
            text=True,
            timeout=10,
            stdin=subprocess.DEVNULL,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        sys.exit(f"{cmd}: {e}")
    if out.returncode != 0:
        sys.exit(f"{cmd}: exit {out.returncode}\n{out.stderr.strip()}")
    return out.stdout


def sections(text):
    """{section: [(head, description)]} for clap's `Name:` sections.

    Entries start at column 2, or 6 for options without a short name; deeper lines continue
    the description (wrapped, or below the entry in clap's long layout).
    """
    result, current = {}, None
    for line in text.splitlines():
        stripped = line.strip()
        indent = len(line) - len(line.lstrip(" "))
        if stripped and indent == 0:
            current = result.setdefault(line[:-1], []) if line.endswith(":") else None
        elif current is None or not stripped:
            continue
        elif indent == 2 or (indent == 6 and stripped.startswith("-")):
            m = ENTRY.match(stripped)
            current.append((m.group(1), m.group(2) or ""))
        elif current:
            head, desc = current[-1]
            current[-1] = (head, f"{desc} {stripped}".strip())
    return result


def split_brackets(desc):
    """The description without clap's trailing [aliases: ...] etc., and those values."""
    extra = {k: [v.strip() for v in vals.split(",")] for k, vals in BRACKETS.findall(desc)}
    return BRACKETS.sub("", desc).strip() or None, extra


def option(head, desc):
    names = [p.split(" ")[0].split("=")[0] for p in head.split(", ") if p.startswith("-")]
    desc, extra = split_brackets(desc)
    opt = {"names": names, "description": desc}
    if "<" in head:
        values = extra.get("possible values", [])
        opt["args"] = [{"suggestions": [{"names": [v]} for v in values]}]
    return opt


def command(argv, depth):
    parts = sections(help_text(argv))
    spec = {
        "options": [option(h, d) for h, d in parts.get("Options", []) if h.startswith("-")],
        "subcommands": [],
    }
    for head, desc in parts.get("Commands", []):
        name = head.split()[0]
        if name == "help":
            continue
        desc, extra = split_brackets(desc)
        sub = {"names": [name, *extra.get("aliases", [])], "description": desc}
        if depth < MAX_DEPTH:
            sub.update(command([*argv, name], depth + 1))
        spec["subcommands"].append(sub)
    return spec


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    parser.add_argument("command")
    parser.add_argument("description", nargs="?")
    args = parser.parse_args()
    spec = {"name": args.command}
    if args.description:
        spec["description"] = args.description
    spec.update(command([args.command], 1))
    if not spec["options"] and not spec["subcommands"]:
        sys.exit(f"{args.command} -h printed no clap options or commands")
    text = json.dumps(spec, separators=(",", ":")) + "\n"
    if not args.output:
        sys.stdout.write(text)
        return
    tmp = f"{args.output}.tmp"
    with open(tmp, "w") as f:
        f.write(text)
    os.replace(tmp, args.output)


if __name__ == "__main__":
    main()
