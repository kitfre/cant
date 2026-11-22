---
name: cant-steel
description: "Steel Scheme REPL recipes and references for Cant: language idioms, text/JSON data processing, files, processes, pipelines, scripts, REPL state, and inter-agent peers/channels."
---

# Cant Steel Scheme REPL

Use native slash/kebab-case procedures in the `cant` `repl` tool. Load targeted references in `references/` when needed:

- [Language and values](references/language.md) — discovery, records, inputs, macros, errors, bindings.
- [Text and data](references/data.md) — string/JSON/collection recipes.
- [Repository files](references/files.md) — exact reads, batching, guarded mutations.
- [Processes and output](references/shell-and-output.md) — argv, pipelines, bounded output.
- [Scripts](references/scripts.md) — executable files and inline interpreters.
- [REPL](references/repl.md) — retained state, inspection, native redefinition, explicit loss.
- [Network](references/http.md) — HTTP requests and optional web search/fetch.
- [Jobs](references/jobs.md) — background processes and delegated work.
- [Peers and channels](references/channels.md) — discovering other local agents, opening channels/rooms, waiting for replies, and attaching spawned workers.

## Quick Rules

- Prefer discovered slash/kebab-case procedures: `(files/read "README.md" #:limit 80)`. Use `(capabilities/describe 'files/read)` when unsure.
- Prefer pure descriptor constructors (`scripts/inline`, `scripts/file`, `system/exec-step`, `system/script-step`, `replace-text`) over manually spelling JSON tags.
- Ordinary calls and `map`/`apply` are sequential; use `(parallel-calls #:limit 4 ...)` for concurrent independent capability calls.
- Pass long or syntax-sensitive text through `inputs`, accessed with `(input 'key)`.
- Keep intermediates in `let` and return compact summaries; define reusable bindings with `define`.
