{
  description = "Prime Agent personal release with a packaged Python kernel";

  inputs = {
    nixpkgs.url = "https://releases.nixos.org/nixpkgs/nixpkgs-26.11pre1107841.9d3b8db48266/nixexprs.tar.xz";
    llm-agents = {
      url = "github:numtide/llm-agents.nix/98309b147b1c011b7d3c16d63402ca7b6805e952";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, llm-agents, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      source = nixpkgs.lib.cleanSource self;
      packageFor = pkgs: pkgs.callPackage ./nix/package.nix {
        inherit llm-agents;
        src = source;
        python3 = pkgs.python314;
      };
    in {
      packages = forAllSystems (system:
        let package = packageFor nixpkgs.legacyPackages.${system};
        in {
          default = package;
          prime-agent = package;
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/prime-agent";
          meta.description = "Prime Agent personal release";
        };
      });

      overlays.default = final: _prev: {
        prime-agent = packageFor final;
      };

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          python = self.packages.${system}.default.pythonRuntime;
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy python ];
            PRIME_AGENT_KERNEL_PYTHON = "${python}/bin/python3";
          };
        });

      checks = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          package = self.packages.${system}.default;
        in {
          inherit package;
          kernel = pkgs.runCommand "prime-agent-packaged-kernel-tests" {
            nativeBuildInputs = [ package.pythonRuntime ];
          } ''
            export HOME="$TMPDIR/home"
            mkdir -p "$HOME" tests
            # Copy only the tests, so imports and child kernels use the installed runtime.
            cp ${source}/prime-agent-runtime/test/test_{notify,repl}.py tests/
            python -m unittest discover -s tests -p test_notify.py -v
            touch "$out"
          '';
        });
    };
}
