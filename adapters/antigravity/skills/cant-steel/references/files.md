# Repository files in Steel

Prefer structured file capabilities to shell commands for reading, searching, and editing text. Discover unfamiliar signatures with `(capabilities/describe 'files/read)`; native option keywords are kebab-case while result fields retain exact schema keys.

## Search, batch, and read

Use `files/grep`, `files/find`, and `files/list` when available. Check `limitReached` before treating search results as exhaustive. Use `files/find-many` for several patterns sharing a root and `files/read-many` for bounded sets of small files; describe their live limits first. `map` does not make separate reads concurrent.

```scheme
(define files (files/read-many '("adapters/pi/package.json" "adapters/antigravity/plugin.json")))
(map (lambda (file)
       (obj (path (get file 'path))
            (value (json-parse (get file 'text)))))
     files)
```

`files/read` returns `.text` as the exact `text` field, plus coverage and version metadata, not a plain string. A whole-file read fails instead of truncating above its cap. `files/read-many` preserves path order, returns complete read metadata per file, and fails the whole batch if any file is unreadable or oversized. Successful batch reads establish coverage for guarded replacements.

For a large file, request chunks with `#:offset` and `#:limit`, follow `nextOffset` until absent, and verify the same `sha256` across every chunk. `complete` describes this read; `coverageComplete` tracks accumulated current-version coverage. Don't replace a file based on a partial or mixed-version read.

## Mutations and literal source

Choose the narrowest operation:

- `files/edit` applies unique, non-overlapping exact edits; a blanket whole-file read is not required. Include `#:expected-sha256` when the current version is known.
- `files/write` is create-only and fails if the path exists.
- `files/replace` requires complete current-version read coverage and a matching `#:expected-sha256`.

Supply long or syntax-sensitive content through top-level `inputs`, not embedded Scheme strings:

```scheme
(files/edit (input 'path)
  (list (replace-text (input 'old) (input 'new))))
```

`replace-text` constructs the existing exact edit record; it does not weaken matching or version checks.

```scheme
(files/write (input 'path) (input 'content))
```

For a full replacement, first read and inspect the current file, then use its version hash with the authored replacement:

```scheme
(files/replace (input 'path) (input 'content)
  #:expected-sha256 (input 'sha256))
```

Keep parsing, filtering, and projection in Scheme and return compact evidence rather than full documents. See [Language and values](language.md) for exact fields, `let-fields`, and JSON-safe results.
