# Personal releases with Nix

The `personal-release` branch combines the background-coroutine `rlm.notify` API
with the Kitty repeated-character input fix. It is the integration branch for
personal builds; feature branches can stay focused on their individual changes.

The flake builds Rust, the Python runtime, and bundled skills from the same
checkout. It reuses a pinned `llm-agents.nix` packaging recipe and ships Python
3.14 with the kernel dependencies. Supported systems are `x86_64-linux`,
`aarch64-linux`, and `aarch64-darwin`.

```sh
nix run github:JachinShen/prime-agent/personal-release -- --version
nix build github:JachinShen/prime-agent/personal-release
```

For a reproducible deployment, commit the consuming project's `flake.lock`, or
replace `personal-release` in the input URL with a commit or personal release tag.
Update a deployment with `nix flake update prime-agent` and rebuild that deployment.

## NixOS and Home Manager

Add this input to `nixos-config/flake.nix`:

```nix
inputs.prime-agent = {
  url = "github:JachinShen/prime-agent/personal-release";
  inputs.nixpkgs.follows = "nixpkgs-unstable";
};
```

Use the package in a NixOS module that receives `inputs` through `specialArgs`:

```nix
{ inputs, pkgs, ... }: {
  environment.systemPackages = [
    inputs.prime-agent.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];
}
```

For Home Manager or nix-darwin, put the same package in `home.packages` or
`environment.systemPackages`, respectively. Replace the existing Prime Agent
entry when switching to this package.

The flake also exports `overlays.default` for configurations that prefer
`pkgs.prime-agent`. The overlay uses the consuming package set, which must provide
a recent Rust toolchain and Python 3.14.

## Project Python environments

Select a custom Python and add project packages through the package's overrides:

```nix
primeAgent = inputs.prime-agent.packages.${pkgs.stdenv.hostPlatform.system}.default.override {
  python3 = pkgs.python314;
  extraPythonPackages = ps: [ ps.rich ];
};
```

`primeAgent.pythonRuntime` exposes the complete Python environment for a dev shell.
The launcher uses it by default and honors an explicit `PRIME_AGENT_KERNEL_PYTHON`.
An override interpreter must include the matching runtime, skills, and dependencies.

Each package build uses a separate daemon socket under
`/tmp/prime-agent-personal-<uid>/`. This ensures a modified host is used even while
an upstream daemon with the same version is running. An explicit
`PRIME_AGENT_DAEMON_SOCKET` takes precedence, including an IC dev shell's socket.
Existing kernels keep the runtime they started with; package updates apply to
new kernels.

## Development and verification

```sh
nix develop
cargo build --locked -p pa-cli --bin prime-agent
nix flake check
```

`nix flake check` builds the package with its install checks and runs the
notification tests against the installed Python runtime, including a background
coroutine that sends notifications after its cell has returned.

The upstream release workflow depends on upstream publication credentials.
Personal Nix deployments consume this branch or an explicitly selected commit/tag.
