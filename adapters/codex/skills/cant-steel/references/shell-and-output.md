# Processes and bounded output in Steel

Prefer literal argv: `system/exec` for an executable, `system/pipeline` for shell-free streaming between steps, and [scripts/run](scripts.md) when an executable script or inline source is the payload. Describe unfamiliar signatures first.

```scheme
(define result (system/exec "git" '("status" "--short")
                 #:max-output-bytes 4096))
(if (and (get result 'ok) (not (get result 'stdoutTruncated)))
    (obj (status (get result 'stdout)))
    result)
```

Arguments are literal: shell syntax in argv is not evaluated.

## Shell-free pipelines

```scheme
(system/pipeline
  (list
    (system/exec-step "git" '("ls-files"))
    (system/exec-step "wc" '("-l")))
  #:timeout-seconds 60)
```

`system/exec-step` takes a command and optional argv; `system/script-step` takes a script
descriptor and optional argv. Both accept `#:cwd` and `#:env` and only build data.
Use `scripts/file` or `scripts/inline` for a script step's descriptor. Execution happens
only at `system/pipeline`.

## Reading results

For `system/exec` and `scripts/run`, inspect `ok`, `exitCode`, and `timedOut`. Check `stdoutTruncated` and `stderrTruncated` before parsing bounded output. For a pipeline, `stdout` is the final stream; `steps` contains each step's exit code and diagnostics.

Process capabilities run on the host with your user permissions, not inside the Steel runner. For long-running commands see [Jobs](jobs.md).
