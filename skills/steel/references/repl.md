# Steel REPL

Definitions persist between evaluations in the same workspace.

## Small, explicit submissions

Call `repl` with `op: "eval"`, `code`, and optional literal-string `inputs`:

```json
{"op":"eval","code":"(define items (json-parse (input 'source))) (length items)","inputs":{"source":"[1,2,3]"}}
```

Later in the same live workspace:

```json
{"op":"eval","code":"(map (lambda (x) (* x 2)) items)"}
```

Submissions are sequential. Inputs refresh each time; a saved procedure using the supplied
`input` helper sees current inputs. Retain data explicitly with `define`, not imaginary result
handles. Results are bounded native Steel text, not JSON envelopes. Top-level strings display
as multiline text; nested strings remain quoted. Return `(get (files/read "path") 'text)` for
file contents. Use `(print value ...)` for explicit bounded output; `console-log` remains a
JSON-only compatibility name. Ordinary port writers are still discarded. A compact header identifies
the workspace, generation/submission (g1/#5), and outcome. Previews are not replayable source.

## Look before executing

- `{"op":"inspect","origin":"user","offset":0,"limit":20}` inventories user bindings.
- `{"op":"inspect","name":"items"}` targets one binding.
- `{"op":"search","query":"map","origin":"all","offset":0,"limit":20}` searches known metadata.
- `{"op":"describe","name":"map"}` asks for available metadata, not an inferred perfect signature.
- `{"op":"history","offset":0,"limit":20}` lists retained submissions; use `id` to retrieve source.
- `{"op":"reset"}` discards the workspace/history and starts a new generation.
- `{"op":"close"}` releases state; a later explicit eval can start fresh.

`inspect`, `search`, and `describe` are passive and bounded. They do not evaluate expressions
or expand executable macros. Missing metadata is unknown, not proof of a missing callable.
An inventory is neither a complete lexical environment nor a complete executable-macro list.
Follow pagination/truncation markers; do not dump the whole standard library or all captures.
History preserves submitted source, not reconstructed closure dependencies, and never replays it.

## Redefinition is native Steel

```scheme
;; First submission
(define x 10)
(define (read-x) x)
;; Later submission
(define x 20)
(list x (read-x)) ; (20 10)
```

Old compiled closures can retain old bindings. Redefine dependents explicitly if needed.
`set!` is not a repair for closures after shadowing. Do not blindly rerun definitions that
perform host effects, and do not assume builtin docs still describe a shadowed binding.

## State loss

Read the workspace and generation in each reply. An error may leave partial changes; inspect before retrying. Reset, close, or daemon restart can discard definitions and history. Host effects are not rolled back and submissions are not automatically replayed.
