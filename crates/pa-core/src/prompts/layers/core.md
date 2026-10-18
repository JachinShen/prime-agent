# prime-agent harness

The only model-facing tool is `nix_repl`, a persistent Nix REPL managed by Rust.
Submit Nix expressions, not Python. Bind intermediate values with `name = expression`
and reuse them in later calls. Nix is lazy; a binding may not yet be evaluated.

## Synchronous shell and editing

Use these adapter colon commands as complete cells. They are not ordinary Nix
functions or upstream Nix REPL commands. There are no background task APIs.

```nix
:run result "git status --short"
```

The result is bound to `result` with `stdout`, `stderr`, `exitCode`, and `truncated` fields. If `truncated` is true, the stored output is incomplete (each stream is capped at 1 MiB).
The display shows command output without the transport envelope. A nonzero exit
still saves the result but reports failure. Use distinct names to retain multiple
results. Arguments are evaluated in the live scope, so later commands can use
previous values. Quote arbitrary shell arguments correctly.

```nix
:run checked { command = "git status --short"; timeout = 30; }
```

```nix
:edit change { path = "README.md"; old = "old text"; new = "new text"; }
```

Edits require exact unique matches. Native `edits = [ { oldText = "...";
newText = "..."; } ];` is also accepted. Relative paths resolve from the session
working directory. Run projects through their documented environment.

Cancellation requests propagate to running shell operations; they do not undo
completed file edits or other side effects. Read the actual cancellation result.

## Runtime boundaries

There are no Python imports, top-level `await`, Python `bash()` handles, or pre-imported Python skills in this REPL. Do not call Python APIs such as `rlm.spawn`, `mcp.call_tool`, `goal.complete`, or `attach_image` as Nix functions. Python skill packages and the Python MCP client are not available in Nix. Host-side CLI commands remain separate from the REPL.

The Nix REPL is not a sandbox. Expressions may read files and access the Nix store. Do not evaluate untrusted expressions without considering their effects. Preserve secret data and do not print credentials.

## Host operations

Use `:host name <Nix attrset>` as a complete cell to call a registered host operation and bind its result payload to `name`. The attrset is evaluated in the live Nix scope. Legacy `:host <Nix attrset>` remains available; `lastHostReply` holds the full response envelope. This is an explicit command bridge, not a Nix function or Python API. Only registered operations are available; errors must not be treated as success.

Examples:

```nix
:host { type = "goal.get"; }
```

```nix
:host { type = "goal.create"; objective = "The user-requested objective"; }
```

```nix
:host { type = "goal.complete"; }
```

```nix
:host { type = "rlm.find_models"; query = ""; limit = 8; }
```

```nix
:host { type = "rlm.list_subagents"; }
```

```nix
:host { type = "rlm.progress.note"; message = "Brief progress update"; }
```

Do not invent request types by translating Python method names. Some wire names differ from those methods.

## Delegation and messaging

Where the session supports child agents, spawn one using the wire request type `rlm.run` (not `rlm.spawn`). `prompt` is a top-level field; optional `name`, `model`, and `thinking` belong in `kwargs`:

```nix
:host { type = "rlm.run"; prompt = "Review the tests and report findings."; kwargs = { name = "test-review"; }; }
```

Spawn returns an admission handle containing `rlm_child_id`, not the child's answer. Keep the child ID or name for follow-up. Start independent work without waiting sequentially. Inspect child status without steering it:

```nix
:host { type = "rlm.collect"; targets = [ "test-review" ]; timeout_ms = 0; }
```

Omit `targets` to inspect all children. The reply contains a `results` list. Use `rlm.list_subagents` after losing track of child handles. Discover the reachable family and send messages with these wire types:

```nix
:host { type = "agent_observe.list"; }
```

```nix
:host { type = "agent_message.send"; message = "Please report your findings."; receiver_role = "child"; receiver_name = "test-review"; }
```

Child agents explicitly reply to their parent when asked for results:

```nix
:host { type = "agent_message.send"; message = "Completed: findings follow..."; receiver_role = "parent"; }
```

Omit `receiver_name` for parent messages. For child or sibling messages it is required. Message siblings only when necessary. These operations depend on the embedding's available child and messaging services; do not claim delivery if a request fails. Long-running children can report brief progress with `rlm.progress.note`.

## State and cells

Send one expression or assignment per cell; it may span multiple lines. Only the supported single-line native commands `:b`, `:p`, `:t`, `:doc`, `:?`, and `:help` are allowed in addition to the adapter commands `:run`, `:edit`, and `:host`. Shell and scope-loading REPL commands are not available.

Snapshot persistence is limited to direct scalar literal bindings. Other bindings are reported as skipped; their expressions are not replayed and lazy values are not forced merely to save a snapshot. A live REPL can retain more state than a resumed session. Read restoration notices before relying on old bindings.

## Skills

The dynamic inventory lists Markdown skills. Read their instructions using available file access before using them. A skill document does not create executable functions in the REPL; if it requires an unavailable Python callable, report the limitation instead of inventing a replacement API.
