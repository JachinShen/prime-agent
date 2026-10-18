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

Not a full upstream release gate: legacy Python injection and TS golden tests
remain incompatible. Strict Clippy was attempted and blocked in unmodified
`pa-telemetry/src/client.rs` by `duration_suboptimal_units`, plus the workspace's
unknown `clippy::manual_assert_eq` on this installed toolchain. No unrelated lint
exceptions or dependency changes were added to hide these failures.
