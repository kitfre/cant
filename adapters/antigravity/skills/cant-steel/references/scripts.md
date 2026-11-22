# Scripts and inline source in Steel

Use `scripts/run` when source text or an existing executable script is the main process payload. For ordinary commands use `system/exec`; for bounded text/JSON inspection prefer file batching and Scheme transformations over an interpreter.

```scheme
(capabilities/describe 'scripts/run)
```

Pass source through top-level `inputs`, never interpolate it into Scheme or build a heredoc, `mktemp`, or cleanup `trap` merely to deliver source. The host creates a private temporary file and removes it after execution.

```scheme
(define result
  (scripts/run
    (scripts/inline "python3" (input 'script)
      #:filename-extension ".py")
    '("--format" "json")
    #:timeout-seconds 60))
(if (and (get result 'ok) (not (get result 'stdoutTruncated)))
    (json-parse (get result 'stdout))
    result)
```

`scripts/inline` and `scripts/file` are pure first-class record constructors, not host calls.
They hide the wire tags and exact field spelling, without choosing defaults or granting authority.
Optional fields remain omitted; duplicate/unknown arguments fail locally. Host schemas still validate
types and limits when the descriptor is executed. Existing raw records remain accepted.

```scheme
(scripts/inline interpreter content
  #:interpreter-args '("-u") #:filename-extension ".py")
(scripts/file "scripts/test-native-runner.py")
```

Interpreters, paths, source, and any manually supplied JSON tags remain strings, not symbols.

Argument order is deterministic:

1. `interpreterArgs` appear before the temporary script path.
2. The generated temporary script path follows.
3. Ordinary `args` appear after the temporary script path.

Set `filenameExtension` (including the leading dot) if the interpreter requires it.

Existing file scripts run directly and must be executable regular files:

```scheme
(scripts/run (scripts/file "scripts/test-native-runner.py"))
```

Use `system/exec` when explicitly invoking an interpreter around a non-executable file. Arguments are literal, not shell syntax. Inline files are temporary, but the interpreter retains the host's documented process authority; it is not behavior-sandboxed by Steel. See [Processes and output](shell-and-output.md) for status, truncation, separate streams. Return compact parsed values or bounded diagnostics, not full process logs.
