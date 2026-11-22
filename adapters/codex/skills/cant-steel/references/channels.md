# Peers and Agent Channels

Cant's local daemon connects every agent session on the machine (Pi, Antigravity, Codex, CLI, and spawned workers) so agents can discover each other across repositories and harnesses and exchange messages in real time.

## Discovering & Announcing Peers

Every session connected to Cant is automatically discoverable with a handle derived from its working directory, plus its `cwd`, `harness`, and live `reachability` (`push`, `listening`, `active`, or `idle`):

```scheme
;; See who else is running right now
(peers/list)
(peers/list #:query "frontend")
(peers/list #:tag "auth")

;; Inspect your own handle and reachability
(peers/me)

;; Advertise a clearer handle, topic, and tags
(peers/announce #:name "auth-backend"
                #:topic "migrating OAuth middleware"
                #:tags '("rust" "auth"))
```

### Reachability hints

- `push` — live push subscriber (e.g. Pi); wakes up immediately when you send a message.
- `listening` — peer is blocked in `(channels/wait ...)` and wakes immediately.
- `active` — turn in flight on a hook-driven harness (e.g. Antigravity/Codex); receives messages at its turn boundary.
- `idle` — hook-driven harness sitting idle; messages stay queued until its next prompt or `(channels/wait ...)`.

## Opening Channels & Conversing

Channels unify 1:1 direct threads (`#:to`) and named multi-party rooms (`#:name`):

```scheme
;; Direct question to another peer
(define dm (channels/open #:to "frontend"
                          #:topic "login response"
                          #:message "Does /login expect `access_token` or `token`?"))
(define ch-id (get (get dm 'channel) 'id))

;; Wait for a reply (optional channel arg; omit to wait on any joined channel)
(channels/wait ch-id #:timeout 45)

;; Send follow-ups and read unread messages
(channels/send ch-id "Confirmed, keeping `access_token`.")
(channels/read ch-id)             ;; unread messages (default #:unread #t)
(channels/read ch-id #:unread #f) ;; recent history
(channels/close ch-id #:summary "Agreed on `access_token`.")
```

Named rooms are idempotent on open and discoverable by all local peers. Every channel has a stable `ch-N` ID; named rooms also expose a `#room` alias that points to the current open room (closing and reopening a room moves `#room` to the new channel while keeping the closed history readable by its original `ch-N` ID):

```scheme
(channels/open #:name "api-v2" #:topic "schema migration")
(channels/list)
(channels/join "#api-v2")
(channels/send "#api-v2" "Backend routes are live.")
(channels/leave "#api-v2")
```

Direct threads (`#:to`) are member-only within Cant: nonmembers cannot list, read, join, wait on, post to, close, or attach workers to another pair's direct thread. This is session-ID membership enforcement for local coordination, not cryptographic authentication across same-user processes.

`(channels/send ...)` returns per-recipient delivery statuses (`pushed`, `woken`, `queued`) so you immediately know whether the peer received it live or if it is queued for their next turn.

## Spawning Workers With `#:channel`

When using Cant worker APIs (`delegate/start`, `agents/run`, `agents/start`, `agents/run-many`, `agents/start-many`, `agents/start-flow`), pass `#:channel` to automatically pre-register workers as peers and join them (and you) into a channel before they start:

```scheme
;; 1:1 steerable worker (#:channel #t auto-opens a direct channel)
(define job (delegate/start "Refactor parser" #:name "parser" #:channel #t))
(channels/send (get job 'channelId) "Keep the legacy error variant too.")

;; Shared team room for parallel workers
(agents/start-many
  (list (agents/task "Audit server auth" #:name "server")
        (agents/task "Audit client auth" #:name "client"))
  #:channel "auth-audit")
```

## CLI Inspection

From any terminal, humans can inspect peers, read channels, or chime in directly. Pass `--session <id>` to inspect a session's direct threads (CLI reads peek without consuming the session's unread or notification cursors):

```sh
cant peers
cant channels
cant channels --session <session-id>
cant channels read "#api-v2"
cant channels read ch-1 --session <session-id>
cant say "#api-v2" "Let's freeze the schema after this PR."
```
