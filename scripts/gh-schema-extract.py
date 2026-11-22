#!/usr/bin/env python3
"""Trims GitHub's public GraphQL schema to what the features/gh claim's schema
walk reads: each mapped mutation's arguments and every input object reachable
from them, as JSON (tests/fixtures/gh/schema-extract.json).

Usage: scripts/gh-schema-extract.py <schema.docs.graphql> <claim feature json> > tests/fixtures/gh/schema-extract.json

The schema is GitHub's published SDL, https://docs.github.com/public/fpt/schema.docs.graphql
(from github/docs, CC BY 4.0: https://github.com/github/docs/blob/main/LICENSE).
The mapped mutations are the `graphql mutation <name>` routes of the claim's
`scopes` and `never`.
"""
import json
import re
import sys

SOURCE = "https://docs.github.com/public/fpt/schema.docs.graphql"
LICENSE = "CC BY 4.0, GitHub, Inc. (github/docs); trimmed to the mutations the features/gh claim maps"


def strip(sdl):
    sdl = re.sub(r'"""(?:.|\n)*?"""', " ", sdl)
    sdl = re.sub(r'"(?:\\.|[^"\\\n])*"', '""', sdl)
    sdl = re.sub(r"#[^\n]*", " ", sdl)
    sdl = re.sub(r"@\w+(\s*\([^)]*\))?", " ", sdl)
    return sdl


def blocks(sdl, keyword):
    out = {}
    for m in re.finditer(r"\b" + keyword + r"\s+(\w+)[^{]*\{", sdl):
        depth, i = 1, m.end()
        while depth:
            depth += {"{": 1, "}": -1}.get(sdl[i], 0)
            i += 1
        out[m.group(1)] = sdl[m.end():i - 1]
    return out


def fields(body):
    """name -> (args {name: type}, type), for `name(args): Type` or `name: Type`."""
    out = {}
    pos = 0
    pat = re.compile(r"\s*(\w+)\s*(\((?:[^()]|\n)*\))?\s*:\s*([\w\[\]!]+)(\s*=\s*[^\n]*)?")
    while True:
        m = pat.match(body, pos)
        if not m:
            break
        args = {}
        if m.group(2):
            for a in re.finditer(r"(\w+)\s*:\s*([\w\[\]!]+)", m.group(2)):
                args[a.group(1)] = a.group(2)
        out[m.group(1)] = (args, m.group(3))
        pos = m.end()
    if body[pos:].strip():
        raise SystemExit(f"unparsed: {body[pos:pos + 80]!r}")
    return out


def named(t):
    return re.sub(r"[\[\]!]", "", t)


def main():
    sdl = strip(open(sys.argv[1]).read())
    claim = json.load(open(sys.argv[2]))["customizations"]["devsandbox"]["credentials"]["github"]
    routes = [r for s in claim["scopes"].values() for r in s["routes"]] + claim.get("never", [])
    mapped = sorted(r.split()[2] for r in routes if r.startswith("graphql mutation "))

    mutation = fields(blocks(sdl, "type")["Mutation"])
    inputs_sdl = blocks(sdl, "input")
    inputs = {}
    out_mutations = {}
    todo = []
    for name in mapped:
        if name not in mutation:
            raise SystemExit(f"no mutation `{name}` in the schema")
        args = mutation[name][0]
        out_mutations[name] = args
        todo += [named(t) for t in args.values()]
    while todo:
        t = todo.pop()
        if t in inputs or t not in inputs_sdl:
            continue
        inputs[t] = {k: v for k, (_, v) in fields(inputs_sdl[t]).items()}
        todo += [named(v) for v in inputs[t].values()]
    json.dump(
        {"source": SOURCE, "license": LICENSE, "mutations": out_mutations, "inputs": dict(sorted(inputs.items()))},
        sys.stdout,
        indent=1,
        sort_keys=True,
    )
    print()


if __name__ == "__main__":
    main()
