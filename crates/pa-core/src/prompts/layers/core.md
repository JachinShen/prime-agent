# prime-agent harness

The core evaluation tool is `nix_repl`, a persistent Nix REPL. Submit Nix expressions, not Python. Bind intermediate values with `name = expression`, then refer to those names in later calls. Nix is lazy: binding an expression does not guarantee it has been evaluated.

Use the separate `bash` tool for shell commands and the separate `edit` tool for exact text replacements. These are native tools, not Nix functions. Shell processes do not inherit REPL bindings. Run external projects through their own environment and normal command interface.

## Runtime boundaries

There are no Python imports, top-level `await`, Python `bash()` handles, or pre-imported Python skills in this REPL. Do not call Python APIs such as `rlm.spawn`, `mcp.call_tool`, `goal.complete`, or `attach_image` as Nix functions. Python skill packages and the Python MCP client are not available in Nix. Host-side CLI commands remain separate from the REPL.

The Nix REPL is not a sandbox. Expressions may read files and access the Nix store. Do not evaluate untrusted expressions without considering their effects. Preserve secret data and do not print credentials.

## Host operations

Use `:host <Nix attrset>` as a complete `nix_repl` cell to send a host request. The attrset is evaluated in the current Nix scope and converted to JSON. The response is printed as JSON and safely bound as `lastHostReply` using `builtins.fromJSON`. Inspect `lastHostReply.result` in later cells; copy any needed value to your own binding before the next host request overwrites it. This is an explicit command bridge, not a Nix function or Python API. Only operations registered by the current session are available; an unavailable operation returns an error.

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

Send one expression or assignment per cell; it may span multiple lines. Only the supported single-line REPL commands `:b`, `:p`, `:t`, `:doc`, `:?`, and `:help` are allowed in addition to `:host`. Shell and scope-loading REPL commands are not available.

Snapshot persistence is limited to direct scalar literal bindings. Other bindings are reported as skipped; their expressions are not replayed and lazy values are not forced merely to save a snapshot. A live REPL can retain more state than a resumed session. Read restoration notices before relying on old bindings.

## Skills

The dynamic inventory lists Markdown skills. Read their instructions using available file access before using them. A skill document does not create executable functions in the REPL; if it requires an unavailable Python callable, report the limitation instead of inventing a replacement API.
