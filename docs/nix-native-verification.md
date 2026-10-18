# Native Nix synchronous kernel verification

Environment: Linux, Nix 2.34.8. Tests run through Cargo in a Nix gcc/pkg-config shell.

Passed:
- `cargo check -p pa-cli --locked`
- `cargo build -p pa-cli --locked`
- `cargo test -p pa-cli --test nix_runtime_protocol --locked`: 3 tests
- `cargo test -p pa-core --lib nix_ --locked`: 7 passed, 1 ignored
- `cargo test -p pa-core --lib tools::bash_local --locked`: 3 tests
- Explicit ignored `native_nix_run_and_edit_round_trip`: passed with
  `PRIME_AGENT_NIX_RUNTIME=.../target/debug/prime-agent` and
  `PRIME_AGENT_KERNEL_PYTHON=/does-not-exist`.
- `cargo test -p pa-core --test nix_kernel --locked -- --ignored`: 1 passed
  with the native executable override and nonexistent Python path.
- `cargo fmt --all --check` and `git diff --check`.

The native roundtrip verifies separate stdout/stderr, nonzero and signal exits,
retained structured results, actual file edits, host cancellation, shutdown
cleanup and healthy subsequent/new-kernel evaluation. Protocol tests verify
named-result composition and retention, rejected names before dispatch, host
interrupt/recovery and EOF cancellation. Evaluator tests exercise three real
SIGINT/recovery rounds, Unicode and safe literal-only checkpointing.

Interactive smoke: the existing “Prime Nix 实验版” Herdr tab was restarted
with the native binary and no Python override. The model made three separate
`nix_repl` calls: `:run demo "printf native-ready"` → `native-ready`,
`demo.stdout` → `"native-ready"`, and `6 * 7` → `42`.

## File and command ergonomics regression

Added `:write` (create-only), direct `argv`, per-call `cwd`, and text `stdin`.
No new dependencies or capture-limit changes. A typed `pa-agent` error carries
structured tool failure content/details across the core-to-loop bridge without
changing the wire schema.

Focused checks passed:
- `tools::write::tests`: 2 (UTF-8, overwrite refusal, missing parents, symlinks).
- `tools::bash::run_tests`: 5 (argument validation, literal argv, cwd, concurrent
  large stdin and capped stdout/stderr, exit status, dirty-tree Git guard).
- `tools::bash_local::tests`: 4 (including cancellation with blocked stdin).
- `errors_without_tracebacks_remain_visible`: 1.
- `session_engine::tool_bridge::tests`: 7, plus both native ignored roundtrips
  explicitly run with a nonexistent Python override: 2 passed.
- `pa-agent` structured-failure loop test: 1.
- `kernel::nix_runtime`: 5. The SIGINT test now waits for a unique trace marker
  instead of mistaking arbitrary socket readability for evaluator readiness.

Interactive regression on the updated Herdr binary confirmed direct `argv`
with `cwd`, source creation via `:write`, and an offline compiler environment
reused as an argument list (`shellArgs ++ [ "cargo" "test" "--offline" ]`).
Both a missing compiler and an intentionally invalid CSV amount now produce
nonempty tool error text with `isError = true`; the failed command's named
result remains readable in the next cell.

Not a full upstream release gate: legacy Python injection and TS golden tests
remain incompatible. Strict Clippy was attempted and blocked in unmodified
`pa-telemetry/src/client.rs` by `duration_suboptimal_units`, plus the workspace's
unknown `clippy::manual_assert_eq` on this installed toolchain. No unrelated lint
exceptions or dependency changes were added to hide these failures.
