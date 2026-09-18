{
  description = "A kanban-style terminal UI for the GitButler CLI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    # Source for the `but` CLI on macOS, the one runtime dependency kanstack
    # can't ship. GitButler publishes raw `but` binaries only for Linux, so
    # darwin builds from source; pinned to the release kanstack is verified
    # through (src/but/mod.rs). nixpkgs' own `gitbutler` package is
    # deliberately not used: it is 0.19.x (kanstack needs >= 0.22) and its
    # build strips the `but` binary entirely.
    gitbutler-src = {
      url = "github:gitbutlerapp/gitbutler/release/0.22.3";
      # Not used as a flake, only as a source tree; reuse our nixpkgs.
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    crane,
    gitbutler-src,
    ...
  }:
    flake-utils.lib.eachSystem [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ] (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [rust-overlay.overlays.default];
          config.allowUnfreePredicate = pkg:
            builtins.match "^(gitbutler|but)(-.*)?$" (pkg.pname or "") != null;
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = crane.mkLib pkgs;

        kanstackArgs = {
          pname = "kanstack";
          # crane's default cleanCargoSource only keeps .rs/.toml/Cargo.lock,
          # which would drop tests/fixtures/*.json loaded via include_str!.
          src = pkgs.lib.cleanSourceWith {
            src = pkgs.lib.cleanSource ./.;
            filter = path: type:
              type
              == "directory"
              || builtins.match ".*\\.(rs|toml|json)$" path != null
              || baseNameOf path == "Cargo.lock"
              || (baseNameOf (dirOf path) == ".cargo" && baseNameOf path == "config");
          };
          cargoLock = ./Cargo.lock;
          strictDeps = true;
        };
        kanstack = craneLib.buildPackage (kanstackArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly kanstackArgs;
          });

        # kanstack needs `but`, `tmux`/`cmux`, and `git` on PATH at runtime, but
        # the raw crane build ships none of them. The default package is
        # therefore this wrapped binary, so any consumer — `systemPackages`,
        # `nix profile install`, `nix run` — gets the pinned `but` plus the
        # harness tools without installing anything by hand. The dev shell
        # wires these directly so editing kanstack goes through its own tools.
        runtimePath = pkgs.lib.makeBinPath (
          [but pkgs.tmux pkgs.git]
          ++ pkgs.lib.optionals (system == "aarch64-darwin") [pkgs.cmux]
        );
        kanstack-wrapped = pkgs.stdenv.mkDerivation {
          pname = "kanstack";
          inherit (kanstack) version;
          nativeBuildInputs = [pkgs.makeWrapper];
          meta.mainProgram = "kanstack";
          buildCommand = ''
            mkdir -p $out/bin
            makeWrapper ${kanstack}/bin/kanstack $out/bin/kanstack \
              --prefix PATH : "${runtimePath}"
          '';
        };

        # The `but` CLI, the one runtime dependency kanstack can't ship.
        # GitButler's official CDN exposes raw `but` binaries for Linux
        # (fetched below); macOS only ships a self-extracting installer, so
        # there the crate is built from upstream source instead. A 0.22+ `but`
        # is required to run kanstack, and nixpkgs' `gitbutler` package (0.19.x,
        # no `but` binary) does not qualify.
        butVersion = "0.22.3";
        butRelease = "0.22.3-3234";
        butSources = {
          x86_64-linux = {
            url = "https://releases.gitbutler.com/releases/release/${butRelease}/linux/x86_64/but";
            sha256 = "sha256-P09UOmzNkxzmyGxIQFjb5eKUOVl9AB9qCpwo2eUz6hg=";
          };
          aarch64-linux = {
            url = "https://releases.gitbutler.com/releases/release/${butRelease}/linux/aarch64/but";
            sha256 = "sha256-MVoL554hxmzXJ4XuR3Xq0Z2qqZKhsixJAiBg/olYHus=";
          };
        };
        butFromSource = craneLib.buildPackage {
          pname = "but";
          version = gitbutler-src.rev or butVersion;
          src = gitbutler-src;
          cargoLock = "${gitbutler-src}/Cargo.lock";
          cargoExtraArgs = "-p but";
          doCheck = false;
          meta = {
            license = pkgs.lib.licenses.fsl11Mit;
            description = "GitButler's official CLI";
          };
        };
        but =
          if builtins.hasAttr system butSources
          then
            pkgs.stdenv.mkDerivation {
              pname = "but";
              version = butVersion;
              src = pkgs.fetchurl {
                inherit (butSources.${system}) url sha256;
              };
              dontUnpack = true;
              nativeBuildInputs = [pkgs.autoPatchelfHook];
              buildInputs = [
                pkgs.stdenv.cc.cc.lib
                pkgs.dbus
                pkgs.zlib
              ];
              installPhase = ''
                mkdir -p $out/bin
                cp $src $out/bin/but
                chmod +x $out/bin/but
              '';
              meta = {
                license = pkgs.lib.licenses.fsl11Mit;
                mainProgram = "but";
              };
            }
          else butFromSource;
      in {
        packages = {
          default = kanstack-wrapped;
          kanstack-bin = kanstack;
          inherit but;
        };

        apps.default = {
          type = "app";
          program = "${kanstack-wrapped}/bin/kanstack";
        };

        devShells.default = pkgs.mkShell {
          packages =
            [
              rustToolchain
              pkgs.rust-analyzer
              pkgs.git
              but
              pkgs.tmux
              # cmux is the primary split backend on macOS and is packaged there;
              # on every other platform (and on Intel macs, where nixpkgs has no
              # cmux build) kanstack falls back to tmux automatically.
            ]
            ++ pkgs.lib.optionals (system == "aarch64-darwin") [
              pkgs.cmux
            ];
          RUST_BACKTRACE = 1;
          shellHook = ''
            echo "🦀 $(rustc --version)"

            if command -v but >/dev/null 2>&1; then
              echo "kanstack: $(but --version 2>/dev/null | head -1)"
              but --version 2>/dev/null | grep -qE '0\.(2[2-9]|[3-9][0-9])' \
                || echo "kanstack: warning: but < 0.22 might not work; upgrade via https://docs.gitbutler.com/cli-overview"
            else
              echo "kanstack: warning: 'but' not on PATH"
            fi

            backends=""
            for b in cmux tmux; do
              if command -v $b >/dev/null 2>&1; then
                backends="$backends $b"
              fi
            done
            [ -n "$backends" ] \
              && echo "kanstack: split backends:$backends" \
              || echo "kanstack: warning: neither cmux nor tmux found (no harness splits)"
          '';
        };
      }
    );
}
