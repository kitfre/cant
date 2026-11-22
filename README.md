<img src="logo.png" alt="Cant" width="84" />

# Cant

A local Steel Scheme REPL daemon, background jobs runner, and MCP server for coding agents.

Has adapters for [Pi](adapters/pi), [Antigravity](adapters/antigravity), [Codex](adapters/codex), and other harnesses plus a `cant` CLI and stdio MCP server.

## Install

Cant runs Steel in a native child process on Linux (x86_64/aarch64) and Apple-silicon macOS. One Cargo install puts both `cant` and `cant-steel-runner` side by side in Cargo's bin directory:

```bash
cargo install --locked --git https://github.com/kitfre/cant.git cant
```

If Cargo refuses to overwrite a previously installed binary, rerun the command with `--force`.

From a checkout, use `cargo install --locked --path crates/cli` to install, or `cargo build --release --locked` to build both binaries into `target/release/`. The daemon finds the runner next to its own executable unless `CANT_STEEL_RUNNER` is set. No container service is needed.

### Pi

```bash
pi install /path/to/cant/adapters/pi
```

The extension picks up `target/release/cant` from the checkout automatically (or `CANT_BIN`, or `cant` on your `PATH`) and starts the daemon when needed.

### Antigravity

```bash
export PATH="$PWD/target/release:$PATH"
mkdir -p ~/.gemini/config/plugins
ln -s "$(pwd)/adapters/antigravity" ~/.gemini/config/plugins/cant
```

Start Antigravity with `cant` on its `PATH`; both the MCP server and the hooks use it.

### Codex

See [the Codex plugin](adapters/codex/) to install the Cant REPL and Steel skill in Codex. To use your local Codex login for a delegated worker without installing the plugin:

```sh
cant repl '(agents/run "Reply with the word pong." #:driver "codex")'
```

`#:driver` overrides the workspace driver for that worker only. Use `#:model` alongside it to select a model. Both keywords also work on `delegate/start`, `agents/start`, and individual `agents/task` and `agents/node` descriptors.

The daemon starts a private `codex app-server` for each worker (tested CLI: 0.171.0). It uses `codex` on the daemon’s `PATH`, or `CANT_CODEX_BIN`. Restart an older daemon before trying the new driver.

### CLI

```bash
cant repl '(define xs (list 1 2 3)) (map (lambda (x) (* x 10)) xs)'
cant repl --op inspect --origin user
cant repl   # interactive

cant jobs
cant shutdown  # stop the daemon without starting one
cant restart   # start a fresh daemon using this binary
```

Both lifecycle commands discard the old daemon’s in-memory REPL bindings and jobs. After updating Cant, run `cant restart` in a terminal, then `/reload` in Pi to reconnect. If you only built from a checkout, use `target/release/cant` in place of `cant`. Use `--socket PATH` to target a separate daemon for testing.

## Using it in Pi

In Pi, Cant defaults to strict mode, which replaces the built-in file, bash, and `mcp__*` tools with the `repl` tool. It also adds a few commands:

- `/repl` opens an interactive Steel window attached to the same workspace the agent is using (`:vars`, `:history`, `:load <id>`, `:reset`).
- `/jobs` (or `/delegates`) opens the jobs overlay. Enter follows the selected job’s recent output. Press ↑ or Page Up to pause and scroll back, End to resume following, or Esc to return to the list. When the running-jobs footer is visible and the prompt is empty, press ↓ to open the overlay.
- `/cant [status|strict|hybrid|off|driver pi|agy]` checks daemon status, switches modes, or changes the worker driver (defaults to `pi`).

### Examples

Bind search results and keep working with them on later turns:

```scheme
(define crates (get (files/find "Cargo.toml") 'paths))
(define manifests (files/read-many crates))

(map (lambda (f)
       (obj (path (get f 'path))
            (lines (get f 'totalLines))))
     manifests)
```

Run a few independent calls at the same time:

```scheme
(parallel-calls #:limit 4
  (files/grep "TODO" #:glob "*.rs")
  (system/exec "git" '("status" "--short"))
  (system/exec "cargo" '("check" "--quiet")))
```

Kick off background work or hand something to a Pi subagent (both show up live in `/jobs` and notify when they finish):

```scheme
(background/start (scripts/inline "sh" "cargo test --workspace"))

(delegate/start "Audit crates/host for unwrap() calls in non-test code"
                #:name "unwrap-audit")
```

Call any MCP tools loaded in Pi from Scheme under `mcp/<server>/<tool>`:

```scheme
(capabilities/search "mcp/")
```

You can check the arguments for any capability with `(capabilities/describe 'files/edit)`.

Trusted Pi extensions can opt in to REPL-only capabilities via the `cant:repl:collect:v1` event (see [`repl-providers.ts`](adapters/pi/src/repl-providers.ts)). Register a `pi.events.on` listener in the extension factory and append `{ spec, invoke }` entries to the supplied array synchronously. Cant collects them at interactive session start and calls them only during an active agent REPL call. A provider owns argument validation, authorization, and any user confirmations; no direct Pi tool is registered.

## Security & notes

**Use Cant only with trusted projects and agents.** Steel runs in a separate process with a cleared environment, private working directory, and resource limits. This is not a filesystem sandbox. Linux additionally filters the runner’s system calls with seccomp; macOS does not. Host capabilities and delegated workers intentionally run with your user permissions, independently of those runner limits. Antigravity workers launch `agy` with `--dangerously-skip-permissions`. Pi strict mode routes tools through Cant; it is not a security boundary.

## Config

Optional config lives at `~/.config/cant/config.json` (or `$CANT_CONFIG`):

```json
{
  "disabledNamespaces": [],
  "workerDriver": "agy",
  "workerModel": null,
  "strict": true
}
```

## License

MIT
