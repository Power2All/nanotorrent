---
version: "0.1.0"
level: pair
processes:
  design: pair
  implementation: pair
  testing: pair
  documentation: author
  review: assist
  deployment: hint
---

# AI Declaration

This file follows the [AI Declaration Standard v0.1.2](https://ai-declaration.md/en/0.1.2/).

## Notes

NanoTorrent is built with **transparency as a core value**.

NanoTorrent is a Rust 2024 re-implementation of the C++ / libtorrent / wxWidgets
client [PicoTorrent](https://github.com/picotorrent/picotorrent). That
conversion — porting the application structure, settings database and behaviour
onto pure-Rust building blocks ([librqbit](https://crates.io/crates/librqbit)
and native-windows-gui) — was carried out **predominantly with AI assistance**,
under human direction and review. This document states honestly how.

The general rules we hold to:

1. Human review of what the AI produces.
2. The maintainer reads and understands the code before it lands.
3. No secrets, keys or credentials are committed; AI output is checked for them.
4. Every build is tested on a real Windows machine before it is considered done.

### What we use AI for

- **Anthropic Claude — the Opus family**, driven through the **Claude Code CLI**.
  The current development cycle uses **Claude Opus 4.8**; earlier sessions in the
  port's history may have used other Claude Opus/Sonnet versions.

The AI wrote the large majority of the Rust code, the vendored-engine patches,
the dialogs and dark-mode drawing, the unit tests, and this documentation. The
maintainer set the scope, made the product decisions, ran and tested every
build, and reported the bugs that drove the fixes.

### How AI is used

The declared level per process reflects how NanoTorrent is actually built:

- **Design — pair:** Architecture and product decisions are made by the human;
  the AI proposes options, traces the existing code, and pressure-tests
  trade-offs (e.g. "extend, don't fork" for the engine). The final call is human.
- **Implementation — pair (AI-authored):** The AI writes most of the code —
  Rust source, the vendored `librqbit` / `librqbit-tracker-comms` patches, the
  UI. Nothing is treated as final until the human has built it and exercised it.
- **Testing — pair:** The AI writes and runs the unit tests and drives
  automated screenshot/registry checks; the human tests the running app on
  Windows and reports what breaks.
- **Documentation — author:** The README, this declaration and code comments are
  drafted by the AI and edited by the human for accuracy.
- **Review — assist:** Review happens by the human using the app, reading the
  diffs, and reporting defects, which the AI then diagnoses and fixes.
- **Deployment — hint:** Builds and releases are human-run; the AI helps with
  commands and configuration.

### Human review is non-negotiable

Regardless of how much AI wrote a change, it is not "done" until a human has
verified it:

- AI output is treated as a draft, not a commit.
- Factual claims (crate names, Win32 APIs, registry paths, documentation links)
  are verified against reality — several were caught and corrected exactly this
  way (e.g. the comctl32 manifest and magnet-association behaviour).
- Anything touching the network, the filesystem, encryption or the user's
  existing data (session/database migration) gets extra scrutiny.
- Tests and the app must actually run on the maintainer's machine, not just in
  an AI's response.

### Why we publish this

BitTorrent clients handle real network traffic and touch a user's files and
settings. People running NanoTorrent deserve to know how it was built. Being
open that this is an AI-heavy port — with a human accountable for every
release — is the honest thing to do, and lets others judge the code on its
merits.

If the tooling, workflow or declared levels change, this file and its version
are updated to match.
