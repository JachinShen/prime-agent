# Experimental native Nix execution kernel

The only model-facing tool is `nix_repl`. The Rust application starts its own
native transport mode, which controls a real persistent `nix repl`. No Python
interpreter, virtual environment, or Python kernel bootstrap is needed for this
execution path. Legacy Python source in the repository is not the Nix backend.

## Build and run

Prerequisites: Nix (`nix` and `nix-instantiate`) on PATH and Cargo/Rust.
On NixOS:

```sh
nix shell nixpkgs#gcc nixpkgs#pkg-config -c cargo build -p pa-cli --locked
./target/debug/prime-agent --daemon-socket /tmp/prime-agent-nix-native.sock \
  --no-skills --no-context-files
```

Start a fresh session on a new socket rather than reusing a running daemon from
an older build. `PRIME_AGENT_KERNEL_PYTHON` is not needed for the native backend.
The CLI retains upstream version 0.10.0; identify this fork by Git commit.

## Synchronous commands and named results

These colon commands are adapter extensions, not upstream Nix language syntax.
They execute synchronously. There is no background-task API.

Run a shell command and name its captured result:

```nix
:run result "printf hello"
```

The display shows `hello`, not a nested protocol envelope. Later cells can use:

```nix
result.stdout
result.stderr
result.exitCode
```

Multiple named results coexist. Later commands may use earlier values:

```nix
:run next { argv = [ "printf" "%s" result.stdout ]; }
```

`argv` executes a program directly, without a shell or argument expansion.
Prefer it for ordinary programs; use `command` for pipes and redirects.
Exactly one of `argv` and `command` is required. `argv` must be a nonempty list
of strings. A configured shell command prefix cannot be combined with `argv`;
use `command` for that case. Do not concatenate unescaped output into shell code.

Command options are a Nix attribute set:

```nix
:run checked { command = "git status --short"; timeout = 30; }
```

A nonzero exit or signal termination preserves the structured result and reports
the call as failed (`exitCode = null` for signal termination).
Inspect the exit code rather than assuming all captured output means success.

Per-call `cwd` resolves relative to the session directory, without changing
later calls. Optional `stdin` supplies UTF-8 text and closes the input stream:

```nix
:run checked { cwd = project; argv = [ "cargo" "test" "--offline" ]; }
```

```nix
:run checked { cwd = project; argv = [ "./target/debug/app" ]; stdin = sample; }
```

Create a new file without shell heredocs:

```nix
:write source {
  path = project + "/main.rs";
  text = ''
    fn main() {
        println!("hello");
    }
  '';
}
```

`:write` refuses existing paths, including symlinks. Parent directories must
exist. Its result contains the path and number of UTF-8 bytes written. Use
`:edit` for existing files, not a write-with-overwrite option. Nix indented
strings still interpolate `${expression}`: write `''${` for a literal `${`
and `'''` for literal `''`. Double-quoted strings use `\${` for literal `${`.

Edit a file with an exact unique replacement:

```nix
:edit change { path = "README.md"; old = "old text"; new = "new text"; }
```

Native batched `edits = [ { oldText = "..."; newText = "..."; } ];` is also
accepted. The existing edit implementation supplies validation and diff output.
Relative paths and shell commands use the session working directory.

For other registered host capabilities:

```nix
:host children { type = "rlm.list_subagents"; }
```

`children` holds the result payload, not the `status/result` transport envelope.
The legacy `:host { ... }` form remains available; `lastHostReply` retains the
response envelope. Ordinary Nix functions cannot perform these host operations.
Invalid target variable names are rejected before dispatching side effects.

## Ordinary Nix

```nix
answer = 6 * 7
```

In another cell:

```nix
answer
```

Load a flake explicitly and inspect it:

```nix
flake = builtins.getFlake (toString ./.)
```

```nix
flake.nixosConfigurations.amd.config.networking.hostName
```

Native `:b` builds synchronously; it does not activate a system:

```nix
:b flake.nixosConfigurations.amd.config.system.build.toplevel
```

## Cancellation and state

Interrupts propagate to the current evaluation or the host shell operation.
Shutdown cleans up owned child processes. Cancellation cannot undo edits or
side effects already completed; a process that deliberately escapes its process
group is outside ordinary group cleanup. Native Nix builds are managed by the
Nix daemon and may be shared with other clients.

Bindings live in the persistent REPL. Restart recovery remains limited to safe
scalar literal bindings; calculated values, functions, and host results may be
lost and must be recreated. Checkpointing never replays commands or forces
arbitrary lazy expressions. This change does not expand recovery guarantees.

## Limitations and safety

- Unix process groups are required; Windows is not supported by this fork.
- Each cell contains one expression, assignment, or supported colon command.
- Evaluator output is parsed from the native REPL text protocol. A user trace
  containing a newline followed by `error:` can be mistaken for an evaluator error.
- Supported native commands are `:b`, `:p`, `:t`, `:doc`, `:?`, and `:help`.
  Scope-loading commands and interactive shell/editor commands are not exposed.
- Python skills and Python-kernel MCP clients are not available.
- Captured shell stdout and stderr are each capped at 1 MiB. The result includes
  `truncated`; when true, the stored text is not the complete output.
- The REPL and host operations run with the user's permissions, not in a general
  security sandbox. Nix build sandboxing does not isolate host shell commands.
- No dependencies are installed into an external project automatically. Use its
  normal environment (`nix develop`, etc.) for project commands.

## Verification

Tests must exercise native executable startup, not a Python interpreter override.
The implementation is tested with real Nix on Linux, including named binding reuse,
Unicode and quoting, errors, shell/edit operations, cancellation and cleanup.
For tests that launch a separately built runtime, set
`PRIME_AGENT_NIX_RUNTIME="$PWD/target/debug/prime-agent"`. This points to a native
executable, not a Python interpreter. The application itself defaults to its
current executable.

```sh
nix shell nixpkgs#gcc nixpkgs#pkg-config -c cargo test -p pa-core --lib kernel::nix_runtime --locked
nix shell nixpkgs#gcc nixpkgs#pkg-config -c cargo test -p pa-core --lib tools::bash_local --locked
nix shell nixpkgs#gcc nixpkgs#pkg-config -c env \
  PRIME_AGENT_NIX_RUNTIME="$PWD/target/debug/prime-agent" \
  cargo test -p pa-core --test nix_kernel --locked -- --ignored
```

The full upstream test suite still contains Python/TS golden expectations,
Python-interpreter injection fixtures, and NixOS-incompatible `/bin/bash`
assumptions. The native startup intentionally does not use `KernelManagerOptions.python`.
Focused native tests are not a claim of complete upstream parity. The local
strict Clippy gate also encounters upstream lint/toolchain mismatches in
unmodified dependencies; no full-workspace lint pass is claimed.
