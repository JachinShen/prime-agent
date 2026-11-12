{ lib
, callPackage
, coreutils
, python3
, llm-agents
, src
, extraPythonPackages ? (_: [ ])
}:

let
  base = callPackage "${llm-agents}/packages/prime-agent/package.nix" {
    flake = llm-agents;
    inherit python3 extraPythonPackages;
    versionCheckHomeHook = callPackage "${llm-agents}/packages/versionCheckHomeHook/package.nix" { };
  };
in
base.overrideAttrs (oldAttrs: {
  inherit src;
  version = (builtins.fromTOML (builtins.readFile "${src}/Cargo.toml")).workspace.package.version;

  # The personal fork has its own Cargo.lock; keep the vendor hash in sync with it.
  cargoHash = "sha256-IEqwXSPNn2N9/B3M8JktSzqfI258mE5nK3I85snlovM=";

  passthru = oldAttrs.passthru // {
    unwrapped = oldAttrs.passthru.unwrapped.overrideAttrs (_: {
      doCheck = true;
      cargoTestFlags = [ "-p" "pa-daemon" "--lib" "rlm_children::external_tests::" ];
    });
  };

  # Keep the runtime, skills, catalogs, and dependency closure from the pinned recipe;
  # both Rust and Python are built from this checkout through finalAttrs.src.
  installPhase = assert lib.hasInfix "--set PRIME_AGENT_KERNEL_PYTHON" oldAttrs.installPhase;
    lib.replaceStrings
      [ "--set PRIME_AGENT_KERNEL_PYTHON" ]
      [ "--set-default PRIME_AGENT_KERNEL_PYTHON" ]
      oldAttrs.installPhase;

  postInstall = (oldAttrs.postInstall or "") + ''
    # Each personal build starts its own compatible host unless a socket is explicitly selected.
    buildId="$(basename "$out" | cut -c1-32)"
    wrapProgram "$out/bin/prime-agent" \
      --run 'if [ -z "''${PRIME_AGENT_DAEMON_SOCKET:-}" ]; then export PRIME_AGENT_DAEMON_SOCKET="/tmp/prime-agent-personal-$(${coreutils}/bin/id -u)/'"$buildId"'.sock"; fi'
  '';

  meta = oldAttrs.meta // {
    homepage = "https://github.com/JachinShen/prime-agent";
    changelog = "https://github.com/JachinShen/prime-agent/commits/personal-release";
  };
})
