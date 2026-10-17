# Experimental Nix execution kernel

This fork uses a real, persistent `nix repl` for model code instead of the
upstream Python REPL. Rust still hosts the application. Python is only the
JSON-lines transport adapter (`rlm.nix_repl`); model cells are not Python code.

## Scope

The model uses `nix_repl` to evaluate Nix expressions and keep bindings between
calls. Standalone `bash` and `edit` tools remain available. Use `bash` with
`nix shell ... -c ...` or `nix develop ... -c ...` to run other languages and
programs. A derivation build is not a general-purpose side-effect runner:
substitution/caching can skip its commands and builds normally run sandboxed.

This is an experimental fork, not a feature-equivalent upstream release.
Existing Python skill imports and Python snippets cannot run in Nix cells.
Never translate an unavailable capability into an invented Nix function.

## Build and run from this checkout

Prerequisites: Nix on PATH, Cargo/Rust, and uv. On NixOS, use a compiler shell
for the Rust build (the package reference follows your local `nixpkgs` registry):

```sh
uv sync --project prime-agent-runtime --group dev
nix shell nixpkgs#gcc nixpkgs#pkg-config -c cargo build -p pa-cli --locked
export PRIME_AGENT_KERNEL_PYTHON="$PWD/prime-agent-runtime/.venv/bin/python"
./target/debug/prime-agent
```

The Python override selects this checkout's adapter instead of an installed
upstream runtime. Start a fresh session; do not reuse a running upstream daemon
or assume that old Python cells/checkpoints can resume as Nix.

## Example cells

```nix
answer = 40 + 2
```

In a subsequent call:

```nix
answer
```

In a flake repository, bind the flake explicitly (scope-loading REPL commands
such as `:lf` are not supported by this adapter yet):

```nix
flake = builtins.getFlake (toString ./.)
```

Then inspect a configuration (replace the host name for your flake):

```nix
flake.nixosConfigurations.amd.config.networking.hostName
```

A system build does not activate it:

```nix
:b flake.nixosConfigurations.amd.config.system.build.toplevel
```

## Host operations

The adapter adds `:host <Nix attribute set>` as an explicit host-call boundary.
The attribute set is evaluated in the current Nix scope and encoded as JSON;
the Rust host dispatches the request. The response envelope is bound to
`lastHostReply`, so later Nix expressions can inspect it. This is not an ordinary Nix function and
is not an upstream `nix repl` command.

For example, list child agents:

```nix
:host { type = "rlm.list_subagents"; }
```

Only existing host request types are valid. A Python skill does not become
available merely because this bridge exists. Use the fork's tool instructions
for supported calls and response handling.

## State lifetime

Bindings persist in the live Nix process. The experimental checkpoint only
saves directly assigned JSON-compatible scalar literals, without forcing arbitrary
lazy expressions or replaying commands. Functions, derivations, and calculated
bindings can be skipped and will need to be recreated after a kernel restart.
This deliberately avoids silently replaying builds or host-side effects.

## Safety and reproducibility

The REPL runs with the agent user's permissions. It is not a security sandbox.
Nix build sandboxing does not isolate standalone shell tools, file access, or
host requests. Review commands before deploying or activating a configuration.

Pin flake inputs and keep source revisions when reproducibility matters. A live
REPL binding is not automatically an immutable snapshot of the working tree.

## Verification environment

Initial implementation was exercised on Linux with Nix 2.34.8. The CLI is still
versioned 0.10.0 like its upstream base; use the Git branch/commit to distinguish
this fork. No system configuration activation is performed by setup or tests.

## Test coverage and remaining upstream checks

The real-Nix runtime suite, Rust-to-Nix kernel integration, prompt assembly, and
CLI build/smoke checks are exercised separately. The complete upstream test
suite is **not** a passing release gate for this prototype: its TS golden schema
and Python wording fixtures still describe the intentionally replaced kernel.
On this NixOS machine, shell golden fixtures also require `/bin/bash`, which
is not present (the shell is `/run/current-system/sw/bin/bash`). Do not interpret focused tests as full upstream parity.

## Validation commands

```sh
prime-agent-runtime/.venv/bin/python -m unittest discover \
  -s prime-agent-runtime/test -p 'test_nix_repl.py' -v
nix shell nixpkgs#gcc nixpkgs#pkg-config -c env \
  PRIME_AGENT_KERNEL_PYTHON="$PWD/prime-agent-runtime/.venv/bin/python" \
  cargo test -p pa-core --test nix_kernel -- --ignored
nix shell nixpkgs#gcc nixpkgs#pkg-config -c \
  cargo test -p pa-core prompts::system_prompt --lib --locked
```

## Current limitations

- Linux/macOS-style process groups are required; Windows is not supported by
  this experimental adapter.
- Each cell contains one expression, binding, or supported REPL command.
- REPL/build output is buffered until completion and capped at 1,048,576
  characters; use the standalone shell tool when streaming output is needed.
- Scope-loading commands (`:lf`, `:l`, `:a`) and interactive shells/editors are
  not exposed. Bind `builtins.getFlake` or `import` results explicitly instead.
- Python skills and kernel-side MCP clients are unavailable.
- Only directly assigned scalar literals are checkpointed; live calculated
  values and functions do not survive a kernel restart.
- This fork changes the tool contract; historical Python cells are not runnable
  as Nix code.

## Changed interfaces

The model tool name changes from `python_repl` to `nix_repl`. The runtime entry
point changes from `rlm.repl` to `rlm.nix_repl`. Snapshot files are now
`nix-kernel-state.json` and `nix-kernel-state-manifest.json`, separate from old
Python/dill state. Wire protocol version remains 3. Existing Rust identifiers
such as `PYTHON_TOOL_NAME` remain internal implementation names in this prototype.

## Upstream compatibility

This branch intentionally changes the upstream model tool and prompt contract.
Upstream Python-kernel integration tests are not evidence that a Nix kernel
works; real Nix runtime tests are required. Old Python session state is not a
Nix checkpoint. Use a fresh session and a separate development kernel environment.
