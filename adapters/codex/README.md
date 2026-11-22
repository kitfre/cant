# Cant for Codex

This plugin brings Cant’s Steel Scheme REPL workflow to Codex CLI:

- **`repl` MCP tool** (`cant mcp --disable delegate --disable agents --driver codex` over stdio) with persistent per-thread workspaces (`codex-<thread-id>`). Delegation stays on Codex's native subagents — each subagent thread automatically gets its own isolated Cant REPL workspace.
- **Custom Codex TUI statusline** (`ui://cant/statusline`) showing model, context usage, cwd, live Cant workspace generation (`repl gN`), and Pi-style running jobs badge (`▶ N jobs`).
- **Plugin-bundled lifecycle hooks** (`hooks/hooks.json`):
  - `PreToolUse`: enforces strict Cant mode (denying `Bash` and `apply_patch` with Steel redirects; toggle off via `CANT_STRICT=0` or `strict = false` in `~/.cant/config.toml`) and injects `cwd`/`sessionId` into `repl` calls (including Git worktrees and subagent threads).
  - `UserPromptSubmit` & `PostToolUse`: drains completed background job notices into `additionalContext`.
  - `Stop`: continues the turn automatically when an `active`-notify background job finishes.
- **`cant-steel` skill** with full Steel capability references.

## Install from a checkout

Follow the [Cant build instructions](../../README.md#install), including the native Steel runner. From the checkout root:

```sh
export PATH="$PWD/target/release:$PATH"
codex plugin marketplace add "$PWD"
codex plugin add cant@cant-local
```

Start a new Codex session with `cant` still on `PATH`:

1. **Approve the bundled hooks once:** open `/hooks` in Codex CLI and trust the Cant plugin hooks. (To run in hybrid mode where `Bash` and `apply_patch` remain allowed, set `CANT_STRICT=0` or `strict = false` in `~/.cant/config.toml`.)
2. **Enable the Cant statusline:** open `/experimental` and enable **Custom status line** (or set `tui_extensions_statusline = true` under `[features]` in `~/.codex/config.toml`), restart Codex, then open `/statusline` → **Choose a status line provider** → **Cant**.
3. **Verify:** ask Codex to evaluate `(+ 2 3)` in the Cant REPL; the footer will show `repl g1` once the workspace is active and `▶ N jobs` while background scripts or delegates run.

## Hide built-in tools with a Codex profile (`codex -p cant`)

Codex plugins cannot remove built-in tools from the model's tool catalog on their own. To hide Codex's shell and JS code-mode tools while the `PreToolUse` hook redirects `apply_patch`, create `~/.codex/cant.config.toml`:

```toml
[features]
shell_tool = false
code_mode = false
tui_extensions_statusline = true
```

Then start Codex with:

```sh
codex -p cant
```

## Companion terminal workflows (replacing Pi's modal overlays)

Because Codex CLI does not currently expose modal TUI overlays, use a split pane or Codex's integrated terminal to attach straight to the active Codex thread's live workspace and jobs:

```sh
cant repl --attach        # interactive Scheme REPL sharing Codex's live workspace (:vars, :history, :jobs, :reset)
cant jobs list --attach   # list jobs for the active Codex thread in this repo
cant jobs tail <id>       # inspect job output
cant jobs cancel <id>     # cancel a running job
```

## Try a worker

After signing in with `codex login`, try `cant repl --driver codex '(agents/run "Reply with the word pong.")'`. If an older daemon is already running, use a separate `CANT_SOCKET` or restart it when it is safe to discard its workspaces.

Workers run ephemeral turns without the outer Cant plugin. They use read-only sandboxing, disable shell tools and subagents, and reject explicit Cant capability lists (including `[]`) rather than treating them as prompt instructions. If the effective config contains another enabled MCP registration, the worker refuses to start a thread; use a worker configuration without external MCP servers. This policy is for the worker, not the outer Codex session.

## Verify discovery without changing your configuration

Run from the checkout root with a recent Codex CLI (tested with 0.171.0):

```sh
(
  set -eu
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  export HOME="$tmp" CODEX_HOME="$tmp/codex"
  mkdir "$CODEX_HOME"
  codex plugin marketplace add "$PWD"
  codex plugin list --marketplace cant-local --available --json
  codex plugin add cant@cant-local
  codex plugin list --marketplace cant-local --json
)
```

Look for `cant@cant-local` first under `available`, then under `installed` with `enabled: true`. Discovery does not require a running daemon.

## Packaging notes

This bundle uses Codex’s `.codex-plugin/plugin.json` + `.mcp.json` + `hooks/hooks.json` + `skills/` layout because Codex CLI 0.171.0 skips plugin-bundled hooks when a root Agent Plugins `plugin.json` is present. The [repo marketplace](../../.agents/plugins/marketplace.json) points to this directory. Codex caches the package without following external skill symlinks, so `skills/cant-steel/` must be a real copy of the canonical [`skills/steel/`](../../skills/steel/). After editing the canonical skill, refresh and check the bundle from the repo root:

```sh
rsync -a --delete skills/steel/ adapters/codex/skills/cant-steel/
diff -r skills/steel adapters/codex/skills/cant-steel
```
