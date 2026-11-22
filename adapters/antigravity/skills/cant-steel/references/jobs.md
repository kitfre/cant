# Background jobs and delegates

Use a background job when a process should keep running after a submission returns:

```scheme
(define build (background/start "cargo" '("test" "--workspace")))
;; After completion, or when you need diagnostics:
(background/output (get build 'id))
```

Use a delegate for a task that needs a coding agent rather than a single command:

```scheme
(define review (delegate/start "Review the README examples for accuracy" #:name "docs"))
;; After completion:
(delegate/result (get review 'id))
```

Use the unified `jobs/*` capabilities to inspect or cancel across background scripts, delegates, and agent flows in one place:

```scheme
(jobs/list)                       ;; all session jobs (active first)
(jobs/list #:active-only #t)      ;; only queued/running jobs
(jobs/list #:kind "flow")         ;; filter by "script", "delegate", or "flow"
(jobs/tail (get review 'id))      ;; non-destructive status + live monitor/output tail
(jobs/cancel (get review 'id))    ;; cancel any job kind owned by this session
```

Starting a job returns a receipt, not the result. Use completion notifications where supported instead of polling. For structured agent tasks and flows, discover the available calls with `(capabilities/list #:namespace "agents")`.
