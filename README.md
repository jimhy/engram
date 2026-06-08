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
/reload-plugins
```

`/reload-plugins` (or restart a session) applies it; approve the hooks in `/hooks` if prompted. The engine is a single self-contained Rust binary shipped with the plugin — nothing else to install.

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
| `SessionStart` | Injects the **hot index** (relevant memory) into context; also **catches up** any consolidation a previous session didn't finish (crash / abrupt exit) |
| `UserPromptSubmit` | Re-resolves the current project from the nearest **`.engram/` anchor** and re-injects its L4 — only when the project scope (root) changes |
| `SessionEnd` | Spins up an **independent reviewer** that consolidates only the **increment since the last watermark** (token-thrifty on long / resumed sessions): writes new memories, promotes/demotes, supersedes, merges |

**Slash commands:** `/engram list` · `/engram recall <query>` · `/engram status` · `/engram render` · `/engram root` · `/engram statusline [on|off]`

**Status-line indicator** (optional, **off by default**): `● Engram | L1:2 L2:0 L3:1`

## How it works

Memory is **tiered**, like human memory:

| Tier | Role | Decay |
|------|------|-------|
| **L1** | "subconscious" — core identity / preferences | almost never (high floor) |
| **L2** | important | slow |
| **L3** | ordinary | medium |
| **L4** | per-project, lives in `<project>/.engram/engram.redb` | scoped to the project, located via the `.engram/` anchor |

- **Activation = importance + recency + frequency** (ACT-R base-level), with a per-tier floor so L1 stays put.
- **Climbing requires earned activation; falling is cushioned** by the floor and a grace period — new and important memories aren't killed early.
- **Consolidation** runs at session end via an *independent* `claude -p` reviewer reading the transcript, so the judgment of "what was actually used / worth keeping" isn't self-serving.

### What goes in each tier

A memory is only worth keeping if it **can't be cheaply recovered** from the code/docs/git — engram stores the *complement* of your artifacts (intent, the "why", dead-ends, decisions, open loops), distilled into a one-line cue + a pointer to the ground truth.

**General — cross-project, in the shared store (`~/.engram/general.redb`):**
- **L1** — core identity & always-on global rules: who you are, how to address you, language, hard global conventions. Tiny, almost never forgotten.
- **L2** — important reusable knowledge that holds across projects (a tool gotcha, a durable preference).
- **L3** — ordinary, easily-forgotten general notes.

**Per-project — L4, in that project's store (`<project>/.engram/engram.redb`):**
- **L4.1** — project hard rules: the inviolable conventions / taboos for *this* repo, learned from your "always / never" directives or hard-won corrections. *Not* a copy of CLAUDE.md / lint configs (those are auto-loaded artifacts); L4.1 holds what they **don't** say.
- **L4.2** — durable project knowledge: what the project is, its **architecture / module mental-map** (what each part does and why it's split that way — distilled, not an `ls` dump), and settled / debated decisions (what was chosen, what was rejected and why — so a later session won't re-propose a dead option).
- **L4.3** — transient: open loops / hand-off-able work (current progress, what's blocked, next step). Decays fast; superseded once done.

> Golden rule: **store the distilled mental model, never what a single `grep` / `ls` already gives you.** File locations live in the pointer, not the cue.

## Where memory lives

- **Shared** (cross-project L1-3): `~/.engram/general.redb` (auto-created)
- **Per-project** (L4): `<project>/.engram/engram.redb` (travels with the project)

Storage is **[redb](https://github.com/cberner/redb)** (embedded, single-file, ACID) — no server, no external database.

## Inspect / verify

```bash
/engram status            # counts per tier, projects, cold store
/engram list              # list everything
/engram recall <query>    # search (hot + cold)
/engram render            # preview what gets injected
```

## Status line (optional, off by default)

The engram status line is **off by default** — installing the plugin won't show it. To toggle the live `● Engram | L1:n L2:n L3:n` indicator:

```
/engram:statusline on      # enable (no argument is the same as on)
/engram:statusline off     # disable (removes the config; leaves any status line you set for other tools untouched)
```

`on` pins the status-line script to `~/.engram/engram-statusline.sh` (a stable, version-independent path), merges `statusLine.command` into your **user-level** `settings.json`, and tells you to restart. The content is driven by `~/.engram/status.txt`, which the hooks refresh every turn — no DB hit. If you also have [claude-hud](https://github.com/jarrodwatts/claude-hud) installed, its line is prepended automatically; if not, only the Engram segment shows. `off` removes only the entry engram wrote, never a `statusLine` you configured for another tool.

> Why off by default, and why this step: `statusLine` is user-level config that a plugin **cannot** ship automatically (plugin.json has no such field) — that's both why it's off by default and why this command writes it into your `settings.json` for you. Plugin upgrades need **no** reconfiguration — only re-run `/engram:statusline on` if a future upgrade changes the status-line script itself.

## Configuration

- `ENGRAM_REVIEWER_CLI` — which CLI the `SessionEnd` reviewer launches (default `claude`). Set it if your CLI isn't named `claude` (e.g. a local build).

## Platforms & releases

`bin/engram` is a small launcher that picks the right binary for your OS. Pushing a tag (`v*`) runs GitHub Actions ([`.github/workflows/release.yml`](./.github/workflows/release.yml)) to cross-compile **Windows / Linux x86_64 / macOS x86_64 / macOS arm64**, commit the binaries into `bin/`, and attach them to a GitHub Release. (Linux arm64 isn't built yet — PRs welcome.)

Engine source lives in [`engine/`](./engine) (Rust). Build locally with `cargo build --release` inside `engine/`.

On Windows, hooks run under bash, so paths use forward slashes (handled by the bundled launcher/scripts).

## License

MIT
