# Steel data recipes (on demand)

These examples transform bounded inputs in memory. Supply the named strings through `inputs`; use `capabilities/describe` for host calls, not for pure Steel procedures.

## Split, match, replace, join

Input `text`: ` src/a.ts,src/b.ts;;src/a.ts\nREADME.md\r\n` (with actual newlines). Steel uses **`split-many`, not `string-split`**, and **`trim`, not `string-trim`**. The separator is one literal string, not a list of delimiters or a regex. Normalize alternative separators first; explicitly discard empty tokens when appropriate. Matching and replacement below are literal and case-sensitive; replacement changes every occurrence.

```scheme
;; recipe: strings
(define normalized
  (-> (input 'text)
      (string-replace ";" ",")
      (string-replace "\n" ",")))
(define tokens
  (filter (lambda (s) (not (equal? s "")))
          (map trim (split-many normalized ","))))
(obj (tokens tokens)
     (typescript (filter (lambda (s)
                           (and (starts-with? s "src/")
                                (ends-with? s ".ts")))
                         tokens))
     (contains (string-contains? "src/a.ts" "/a."))
     (replaced (string-replace "src/a.ts src/b.ts" "src/" "lib/"))
     (joined (string-join tokens " | ")))
```

## JSON and nested exact keys

Input `payload`: `{"build":{"maxOutputBytes":4096,"odd.key":"λ"},"items":[2,3],"note":null}`. JSON objects become immutable string-keyed hashes, arrays become lists. Return a structured value; no JSON stringification is needed at the result boundary. `value->string` is diagnostic Scheme output, **not a JSON encoder**.

```scheme
;; recipe: json
(define data (json-parse (input 'payload)))
(define build (get data 'build))
(obj (limit (get build 'maxOutputBytes))
     (label (get build "odd.key"))
     (doubled (map (lambda (n) (* n 2)) (get data 'items)))
     (note (get data 'note)))
```

## JSONL, including blank lines and CRLF

Input `jsonl`: one JSON object per line, each with `name` and boolean `ok`. For example, `{"name":"a","ok":true}` followed by a newline. Trim before filtering blanks so CRLF and whitespace-only lines work. Parse each nonblank line independently; malformed lines fail rather than silently disappearing. This is not a parser for pretty-printed, multiline JSON objects.

```scheme
;; recipe: jsonl
(define lines
  (filter (lambda (line) (not (equal? line "")))
          (map trim (split-many (input 'jsonl) "\n"))))
(define rows (map json-parse lines))
(obj (count (length rows))
     (names (map (lambda (row) (get row 'name))
                 (filter (lambda (row) (get row 'ok)) rows))))
```

## Filter, sort, group, deduplicate, aggregate

Input `payload`: a JSON array of `{ "team": string, "name": string, "score": number }` records. Rank positive scores descending; preserve input order within teams and keep the first occurrence of each name. Hash iteration order is unspecified, so ordered output is a list. The local deduplication loop uses an immutable seen-key hash, not repeated linear membership scans. Grouping uses a right fold to preserve row order.

```scheme
;; recipe: collections
(define rows (json-parse (input 'payload)))
(define (name row) (get row 'name))
(define ranked
  (sort (filter (lambda (row) (> (get row 'score) 0)) rows)
        (lambda (a b) (> (get a 'score) (get b 'score)))))
(define groups
  (foldr (lambda (row groups)
           (let ([team (get row 'team)])
             (hash-insert groups team (cons (name row) (get groups team '())))))
         (obj) rows))
(define unique
  (let loop ([remaining (map name rows)] [seen (obj)] [out '()])
    (if (null? remaining)
        (reverse out)
        (let ([item (car remaining)])
          (if (hash-contains? seen item)
              (loop (cdr remaining) seen out)
              (loop (cdr remaining) (hash-insert seen item #t) (cons item out)))))))
(obj (ranked (map name ranked))
     (unique unique)
     (groups groups)
     (total (foldl (lambda (row total) (+ (get row 'score) total)) 0 rows))
     (count (length rows)))
```

## Missing is not false, null, or empty

`get` takes one **exact** symbol/string key, not a dotted path. Missing fields raise unless you supply a fallback; that fallback is evaluated eagerly. `hash-contains?` checks presence without conflating values, but unlike `get` it does not normalize symbols to string keys: use strings for JSON/object records. `json-null` is not Steel void, and nested void cannot cross the JSON result boundary.

```scheme
;; recipe: records
(define key "odd.key")
(define record
  (obj (nested (obj (maxOutputBytes 42)))
       (computed (object key "exact"))
       (no #f) (nil json-null) (empty '())))
(obj (nested (get (get record 'nested) 'maxOutputBytes))
     (computed (get (get record 'computed) key))
     (missing (get record 'absent "fallback"))
     (false (get record 'no "fallback"))
     (null (get record 'nil "fallback"))
     (empty (get record 'empty "fallback"))
     (hasNull (hash-contains? record "nil"))
     (hasMissing (hash-contains? record "absent"))
     (nullIsVoid (void? json-null)))
```

## Check bounded reads before parsing

Requires discovered `files/read` and a small `report.json` such as `{"items":[1,2,3]}`. Check `complete` for **this reply's text**; cumulative `coverageComplete` does not mean the current chunk contains the whole file. Do not parse a partial JSON document. For larger inputs, deliberately collect bounded chunks or use an appropriate host-side processor; see [Files](files.md).

```scheme
;; recipe: bounded-read
(define reply (files/read "report.json" #:limit 80))
(if (get reply 'complete)
    (obj (count (length (get (json-parse (get reply 'text)) 'items))))
    (obj (parsed #f) (reason "incomplete file")))
```

## Check process status and truncation before parsing

Requires discovered `system/exec` and `printf`. `ok` encodes process success; also check the relevant truncation flags. This conservative recipe requires both streams complete and retains separate stdout/stderr plus exit metadata on refusal. A successful exit alone does not make a bounded inline tail valid JSON. See [Processes and bounded output](shell-and-output.md).

```scheme
;; recipe: checked-process
(define reply
  (system/exec "printf" '("%s\\n" "[1,2,3]") #:max-output-bytes 4096))
(if (and (get reply 'ok)
         (not (get reply 'stdoutTruncated))
         (not (get reply 'stderrTruncated)))
    (let ([values (json-parse (get reply 'stdout))])
      (obj (count (length values)) (total (apply + values))))
    (obj (parsed #f) (result reply)))
```

## Pure procedure index

Supported call shapes exercised above (not a full stdlib catalog). No imports or host discovery calls are needed for these APIs. Square brackets indicate optional arguments, not literal call syntax.

| Procedure / form | Purpose and argument order |
| --- | --- |
| `(split-many text separator)` | Literal delimiter splitting into a list; filter unwanted empty fields explicitly. |
| `(trim text)` | Remove leading/trailing whitespace, including CR/LF. |
| `(starts-with? text prefix)`, `(ends-with? text suffix)` | Literal prefix/suffix predicates. |
| `(string-contains? text fragment)` | Literal substring predicate. |
| `(string-replace text from to)` | Replace all literal occurrences. |
| `(string-join strings separator)` | Join a list of strings. |
| `(json-parse text)` | Parse one complete JSON value; malformed JSON raises. |
| `(input key)` | Retrieve a literal input string by symbol/string key. |
| `(obj (key value) ...)`, `(object key value ...)` | Exact immutable records; `obj` quotes field names, `object` accepts computed keys. |
| `(get record key [fallback])` | Exact field lookup; optional eagerly evaluated missing-field fallback. |
| `(hash-contains? hash key)` | Presence check; JSON record keys are strings. |
| `(hash-insert hash key value)` | Return an updated immutable hash; original remains unchanged. |
| `(map procedure list)`, `(filter predicate list)` | Transform/select in list order; not parallel execution. |
| `(sort list less?)` | Comparator takes two elements; use `>` for descending numeric order. |
| `(foldl procedure initial list)`, `(foldr procedure initial list)` | Procedure takes **element, accumulator**; left/right traversal respectively. |
| `(apply procedure list)` | Supply list elements as arguments; `(apply + values)` sums numbers. |
| `(cons item list)`, `(car list)`, `(cdr list)` | Prepend, first element, remaining elements. Guard empty lists before `car`/`cdr`. |
| `(length list)`, `(reverse list)`, `(null? value)` | Count, reverse, empty-list predicate. |
| `(equal? a b)`, `(not value)`, `(void? value)` | Equality, boolean negation, void predicate; `json-null` is a dedicated value. |
| `(-> value (procedure args ...) ...)` | Thread value into each call's first argument; used for string normalization. |

Use small local procedures/loops to compose grouping and deduplication rather than inventing host tools or assuming APIs from another Scheme implementation. Pure processing does not add filesystem, process, network, or import authority.
