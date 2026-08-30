# Graph Report - messgr  (2026-08-30)

## Corpus Check
- 40 files · ~72,202 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 12 nodes · 10 edges · 3 communities (2 shown, 1 thin omitted)
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `5b42c397`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- messgr
- CHANGELOG.md
- Releasing messgr

## God Nodes (most connected - your core abstractions)
1. `Releasing messgr` - 4 edges
2. `messgr` - 3 edges
3. `Changelog` - 2 edges
4. `[Unreleased]` - 1 edges
5. `Cutting a release` - 1 edges
6. `Validating locally (no publish)` - 1 edges
7. `One-time setup this depends on — NOT YET CONFIRMED` - 1 edges
8. `Local development` - 1 edges
9. `Status` - 1 edges

## Surprising Connections (you probably didn't know these)
- None detected - all connections are within the same source files.

## Communities (3 total, 1 thin omitted)

### Community 0 - "messgr"
Cohesion: 0.50
Nodes (3): Local development, messgr, Status

### Community 2 - "Releasing messgr"
Cohesion: 0.50
Nodes (4): Cutting a release, One-time setup this depends on — NOT YET CONFIRMED, Releasing messgr, Validating locally (no publish)

## Knowledge Gaps
- **6 isolated node(s):** `[Unreleased]`, `Cutting a release`, `Validating locally (no publish)`, `One-time setup this depends on — NOT YET CONFIRMED`, `Local development` (+1 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Releasing messgr` connect `Releasing messgr` to `CHANGELOG.md`?**
  _High betweenness centrality (0.273) - this node is a cross-community bridge._
- **What connects `[Unreleased]`, `Cutting a release`, `Validating locally (no publish)` to the rest of the system?**
  _6 weakly-connected nodes found - possible documentation gaps or missing edges._