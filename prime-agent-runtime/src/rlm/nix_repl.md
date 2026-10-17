# Nix REPL transport

Run `python -m rlm.nix_repl` with `nix` and `nix-instantiate` on PATH.
Python hosts the JSONL transport only; model code is parsed and evaluated by Nix.
The runtime starts one persistent `nix repl` process in a separate process group.
It uses protocol version 3 from `repl.md`, with these intentional differences:

- `ready` identifies `runtime: "nix"`, not a Python evaluator.
- Each `execute.code` is one Nix expression, `name = expression` binding, or
  supported REPL command. Expressions and bindings may span lines. Nix's own
  parser normalizes them before submission, preventing incomplete input from
  consuming later cells. Relative paths resolve from the runtime working directory.
- Commands: `:b`, `:p`, `:t`, `:doc`, `:?`, `:help`. Other commands, including
  `:sh`, editors, scope loaders and quit, fail clearly. Use `:host { type = "tool.bash"; command = "..."; }`
  for `nix shell` / `nix develop`. Load a flake with
  `flake = builtins.getFlake (toString ./.)`.
- `:host { type = "rlm.list_subagents"; }` evaluates the attribute set with
  `builtins.toJSON` in the live REPL and sends a normal `host_request`. The host
  reply is printed as JSON and bound to `lastHostReply` via `builtins.fromJSON`.
  For example, `lastHostReply.result` accesses its result. Error replies fail the
  cell and remain available in `lastHostReply`.
- Results combine Nix stdout/stderr, remove ANSI decoration, and are capped at
  1,048,576 characters. Output, including builds, is buffered until completion.
- Interrupts send SIGINT to the Nix process group; queued interrupts are retained.
  EOF and shutdown terminate the group (SIGTERM, then SIGKILL after two seconds).
- `list_names` reports bindings made through this runtime. MCP, Python skills,
  bash activity and factory activity requests fail explicitly; no Python bootstrap
  is executed.

## Checkpoints

Snapshots have format `prime-agent-nix-literals-v1`, not Python pickle.
Only direct JSON-compatible scalar literals (numbers, booleans, null and
non-interpolated strings) are saved. **Calculated bindings, functions, lists,
attribute sets, derivations, and loaded flakes survive only in the live REPL.**
They are reported in `skipped` and are lost on restart. Checkpointing never forces
lazy values or replays expressions/builds. `pruned` is always empty: pruning
live REPL scope is unsupported. Restore imports only validated scalar values.
The aggregate and per-variable byte limits are honored; aggregate overflow fails
rather than silently discarding saved values. Rebuild any skipped bindings explicitly.
