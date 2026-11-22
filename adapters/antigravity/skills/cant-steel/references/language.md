# Steel language, discovery, and values

Write ordinary Scheme. The REPL retains definitions and displays the last expression. Ordinary capability calls are synchronous and sequential. JSON is the host boundary, not the language to program in.

## Discover and call

```scheme
(capabilities/search "read")
(capabilities/list #:namespace "files")
(capabilities/describe 'files/read)
(capabilities/describe "files/read")
(capabilities/describe #:names '(system/exec files/edit))
```

Describe accepts discovered binding strings/symbols or exact canonical names. String bindings resolve against the live catalog, including newly discovered capabilities; unknown names return `json-null`. Symbols resolve against the invocation snapshot. Search returns bounded compact signatures; use describe for canonical argument/result schemas. Namespace listing remains available for a known area. These describe host capabilities, not Steel's pure library; the optional [Text and data](data.md) reference indexes pure procedures and recipes. Its `argumentMode` describes the host contract; the Scheme signature describes how to call the guest procedure. Prefer slash-separated namespaces and kebab-case. Object capabilities accept keywords; explicitly documented primary arguments also accept positionals:

```scheme
(files/read "README.md" #:limit 80)
(files/read #:path "README.md" #:limit 80)
(system/exec "git" '("status" "--short") #:max-output-bytes 4096)
```

Positionals precede keywords. Optional fields stay omitted unless supplied; keywords may also supply primary arguments. Duplicate/unknown keywords are errors. Do not infer positional order from schemas or invent a short binding when discovery supplies an exact fallback.

Bindings are first-class procedures; `map` and `apply` work normally. `(map files/read '("README.md" "Cargo.toml"))` is sequential, not parallel. Prefer explicit host batch capabilities when appropriate.

## Explicit concurrent calls

Use `parallel-calls` for independent registered capability calls:

```scheme
(parallel-calls #:limit 4
  (files/read "README.md")
  (files/read "Cargo.toml")
  (system/exec "git" '("status" "--short")))
```

Calls must be independent registered capabilities; results are returned in source order.

Prefer a purpose-built batch capability such as `files/read-many` when it fits. A background-start capability still returns a launch receipt, not completion of the background job; follow its notification contract.

## Values, inputs, and transformations

Objects are immutable exact string-keyed hashes; arrays are lists (immutable vectors also serialize). `obj` quotes field identifiers, not value expressions. `object` accepts computed string/symbol keys. Neither silently renames keys; duplicate keys are errors. Keywords use kebab-case, but nested record fields retain their exact schema spelling.

```scheme
(define data (json-parse (input 'payload)))
(obj (doubled (map (lambda (x) (* x 2)) (get data 'items))))

(object "arbitrary.key" 42)
(get (obj (maxOutputBytes 4096)) 'maxOutputBytes)
```

`get` takes an exact symbol/string key and errors if missing. An optional third argument is an explicit fallback, evaluated eagerly like any ordinary procedure argument. False, missing, empty lists, and JSON null are different. `json-null` is a dedicated value, not Steel `void`.

Use `(input 'source)` or `(input "source")` for long, multiline, or syntax-sensitive strings. Inputs are immutable literal data and are not evaluated. For edit payloads and `replace-text`, see [Files](files.md). Pure descriptor constructors hide fixed wire tags: [scripts/file and scripts/inline](scripts.md), [system/exec-step and system/script-step](shell-and-output.md), and [agent task/node/references](jobs.md). They are ordinary procedures with no host calls; records for arbitrary payloads stay exact.

```scheme
(let-fields ([text sha256] (files/read "README.md"))
  (obj (characters (string-length text)) (sha256 sha256)))
```

Reuse Steel's standard `map`, `filter`, `->`, and `->>` for local transformations. Use ordinary helper procedures for repeated operations; use hygienic `syntax-rules` macros when binding or domain syntax is the actual benefit. Executable expansion stays in the runner process.

## Errors and inspection

```scheme
(with-handler error-info
  (files/read "missing-file.txt"))
```

`error-info` returns a caught error's `kind`, `name`, and `message`; host errors retain their original identity, such as `PermissionError`. Handle refusal; don't retry through another authority route. This helper is diagnostic, never proof of authorization.

Symbols, procedures, structs, and syntax are useful locally. Arbitrary symbols and procedures are not automatically coerced to JSON strings. Capability discovery explicitly accepts binding symbols and strings; ordinary payload fields still follow their JSON schemas.

```scheme
(inspect '(read transform write))
(value->string '(a (b c)))
```

`inspect` emits bounded Scheme-oriented console text; `value->string` returns it. Unsafe/custom printers are not invoked.
