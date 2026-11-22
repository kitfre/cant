# Cant Steel REPL (Strict Mode)

You have a persistent Steel Scheme REPL via the `cant` MCP server (the eager `repl` tool, exposed as `mcp_cant_cant_repl` or via `call_mcp_tool`).

## Default to `cant` `repl`

- **Always use `cant` `repl` for repository work** — reading/searching/listing/editing/writing files (`files/read`, `files/read-many`, `files/grep`, `files/find`, `files/find-many`, `files/list`, `files/edit`, `files/write`, `files/replace`), running commands/builds/tests (`system/exec`, `system/pipeline`, `scripts/run`), HTTP (`http/request`), and background shell processes (`background/start`).
- **Use Antigravity's native subagents & workflows for delegation** — use `invoke_subagent`, `define_subagent`, `send_message`, `manage_subagents`, and `run_workflow` for delegated or parallel agent work. Each subagent automatically gets its own isolated Cant REPL session scoped to its conversation ID and worktree `cwd`.
- Direct workspace file/shell tools (`read_file`, `write_file`, `edit_file`, `run_command`) are blocked in strict mode (except Antigravity internal `.gemini/` schema/artifact/skill files and binary media).
- **Compound iterative work**: retain parsed files, search hits, and helpers in `define` bindings across turns; use `let` for one-off intermediates; return compact summaries rather than raw file dumps.
- **Background shell jobs**: do not poll — completed jobs deliver notifications automatically. Use the `jobs` MCP tool (`list`, `tail`, `cancel`) or `(jobs/list)` / `(jobs/tail id)` / `(jobs/cancel id)` in `repl` when the user asks for live status or diagnostics.
- **Cross-agent discovery & channels**: use `(peers/list)` / `(peers/announce #:name ... #:topic ...)` to find or advertise local agent sessions across repos and harnesses (Antigravity, Pi, Codex, CLI), and `(channels/open ...)`, `(channels/send ...)`, `(channels/read ...)`, `(channels/wait ...)` to ask questions or coordinate in rooms.
- For unfamiliar signatures, run `(capabilities/search "...")` or `(capabilities/describe 'files/read)` inside `repl`.
