# Engram

> Brain-inspired long-term memory for Claude Code — **tiered, self-forgetting, auto-consolidating**. Injects relevant memory at session start and consolidates at session end. One Rust binary, zero deps, no vector DB.

**English** | [中文](./README.zh-CN.md)

---

## Why

Most "memory" for AI agents dumps everything into a vector database and stuffs chunks back into the prompt — token-heavy, noisy, and awkward to actually use. Engram takes the opposite approach, modeled on how human memory works:

- **Remember the gist, not the details.** Each memory is a one-line *cue* + a *pointer* to the ground truth (`file:line`, a doc, a URL). You recall the cue, then follow the pointer when you need detail — **verification, not reconstruction**.
- **Store the complement of your artifacts.** Your code is already the perfect detail store (`grep` finds it). Engram only stores what the code *doesn't* capture: intent, decisions, dead-ends, the "why".
- **Forget the noise.** Memories decay over time (ACT-R-style) unless reinforced by real use; low-value ones demote out of the hot set. *Forgetting = demotion, not deletion.*

The result: a tiny, always-relevant **hot index** in your context, plus a much larger searchable **cold store** — **without a vector database**.

## Install

```
/plugin marketplace add jimhy/engram
/plugin install engram
```

Restart a session; approve the hooks in `/hooks` if prompted. The engine is a single self-contained Rust binary shipped with the plugin — nothing else to install.

## Update

```
/plugin marketplace update engram-marketplace
/plugin update engram
/reload-plugins
```

Then `/doctor` should report no plugin errors. (If `/plugin update` doesn't pick up the new version, `/plugin uninstall engram` then `/plugin install engram` re-clones the latest.)

## What you get

**Automatic memory, both directions:**

| Hook | What it does |
|------|--------------|
| `SessionStart` | Injects the **hot index** (relevant memory) into context |
| `UserPromptSubmit` | **Dynamically mounts** the L4 memory of whichever sub-project you're working on — re-injects only when the active project changes |
| `SessionEnd` | Spins up an **independent reviewer** that reads the transcript and consolidates: writes new memories, promotes/demotes, supersedes, merges |

**Slash commands:** `/engram list` · `/engram recall <query>` · `/engram status` · `/engram render`

**Status-line indicator** (optional): `● Engram | L1:2 L2:0 L3:1`

## How it works

Memory is **tiered**, like human memory:

| Tier | Role | Decay |
|------|------|-------|
| **L1** | "subconscious" — core identity / preferences | almost never (high floor) |
| **L2** | important | slow |
| **L3** | ordinary | medium |
| **L4** | per-project, lives in `<project>/.claude/engram.redb` | scoped, mounted on demand |

- **Activation = importance + recency + frequency** (ACT-R base-level), with a per-tier floor so L1 stays put.
- **Climbing requires earned activation; falling is cushioned** by the floor and a grace period — new and important memories aren't killed early.
- **Consolidation** runs at session end via an *independent* `claude -p` reviewer reading the transcript, so the judgment of "what was actually used / worth keeping" isn't self-serving.

## Where memory lives

- **Shared** (cross-project L1-3): `~/.engram/general.redb` (auto-created)
- **Per-project** (L4): `<project>/.claude/engram.redb` (travels with the project)

Storage is **[redb](https://github.com/cberner/redb)** (embedded, single-file, ACID) — no server, no external database.

## Inspect / verify

```bash
/engram status            # counts per tier, projects, cold store
/engram list              # list everything
/engram recall <query>    # search (hot + cold)
/engram render            # preview what gets injected
```

## Status line

Claude Code allows only **one** status line. If you already use another (e.g. [claude-hud](https://github.com/jarrodwatts/claude-hud)), use `statusline/engram-statusline.sh` to **compose** them — it renders the other line, then appends `● Engram | …`. Point your `settings.json` `statusLine.command` at that script.

## Configuration

- `ENGRAM_REVIEWER_CLI` — which CLI the `SessionEnd` reviewer launches (default `claude`). Set it if your CLI isn't named `claude` (e.g. a local build).

## Platforms & releases

`bin/engram` is a small launcher that picks the right binary for your OS. Pushing a tag (`v*`) runs GitHub Actions ([`.github/workflows/release.yml`](./.github/workflows/release.yml)) to cross-compile **Windows / Linux x86_64 / macOS x86_64 / macOS arm64**, commit the binaries into `bin/`, and attach them to a GitHub Release. (Linux arm64 isn't built yet — PRs welcome.)

Engine source lives in [`engine/`](./engine) (Rust). Build locally with `cargo build --release` inside `engine/`.

On Windows, hooks run under bash, so paths use forward slashes (handled by the bundled launcher/scripts).

## License

MIT
