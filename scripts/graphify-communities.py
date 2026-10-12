#!/usr/bin/env python3
"""Deterministic community index for a Graphify graph.json: no model call, ever.

`graphify cluster-only --no-label` leaves every community named "Community N", which tells a
reader nothing. Naming them needs an LLM backend that Graphify auto-picks from whichever API key
is set (docs/15). This names each community by the source files that dominate it and lists its
best-connected symbols instead, so the map stays offline and reproducible.

    python3 -I scripts/graphify-communities.py <map>/graphify-out/graph.json > <map>/COMMUNITIES.md

Stdlib only. Run it with -I: graph.json is derived data, never code.
"""

import collections
import json
import sys

TOP_COMMUNITIES = 60
TOP_COUPLINGS = 50
TOP_FILES = 40


def main(path: str) -> str:
    with open(path, encoding="utf-8") as f:
        graph = json.load(f)
    nodes = {n["id"]: n for n in graph["nodes"]}
    edges = graph.get("links") or graph.get("edges") or []

    degree = collections.Counter()
    coupling = collections.Counter()
    for e in edges:
        degree[e["source"]] += 1
        degree[e["target"]] += 1
        a, b = nodes.get(e["source"]), nodes.get(e["target"])
        # EXTRACTED only: INFERRED edges are Graphify's guesses, even under --code-only.
        if a and b and e.get("confidence") == "EXTRACTED":
            fa, fb = a.get("source_file") or "", b.get("source_file") or ""
            if fa and fb and fa != fb:
                coupling[tuple(sorted((fa, fb)))] += 1

    communities = collections.defaultdict(list)
    for n in graph["nodes"]:
        communities[n.get("community")].append(n)

    def files_of(members):
        named = (n.get("source_file") for n in members)
        return collections.Counter(f for f in named if f).most_common(3)

    def hubs_of(members):
        return sorted(members, key=lambda n: (-degree[n["id"]], n["id"]))[:6]

    out = [
        "# Code map: deterministic community index",
        "",
        f"From `graph.json` built at commit `{graph.get('built_at_commit') or 'unknown'}`. "
        "Each community is named by its dominant source files, with no model call. "
        "Use it to find where to look, then read the files.",
        "",
        f"## Communities (largest {TOP_COMMUNITIES})",
        "",
    ]
    # Nodes Graphify left without a community (none on a clustered graph, but an unclustered
    # extract has them) are listed once, last, as "unassigned" rather than "CNone".
    ranked = sorted(communities.items(), key=lambda kv: (kv[0] is None, -len(kv[1]), str(kv[0])))
    for cid, members in ranked[:TOP_COMMUNITIES]:
        files = ", ".join(f"`{f}`×{c}" for f, c in files_of(members))
        hubs = ", ".join(f"`{n.get('label', n['id'])}`" for n in hubs_of(members))
        label = "unassigned" if cid is None else f"C{cid}"
        out.append(f"- **{label}** ({len(members)} nodes): {files} · hubs: {hubs}")

    out += ["", f"## Strongest cross-file coupling (EXTRACTED edges, top {TOP_COUPLINGS})", ""]
    out += [f"- `{a}` ↔ `{b}`: {c}" for (a, b), c in coupling.most_common(TOP_COUPLINGS)]

    per_file = collections.Counter(n.get("source_file") for n in graph["nodes"] if n.get("source_file"))
    out += ["", f"## Largest files by symbol count (top {TOP_FILES})", ""]
    out += [f"- `{f}`: {c}" for f, c in per_file.most_common(TOP_FILES)]
    return "\n".join(out) + "\n"


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: graphify-communities.py <graph.json>")
    sys.stdout.write(main(sys.argv[1]))
