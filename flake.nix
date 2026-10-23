{
  description = "Thurm: a macOS terminal (Apple silicon), and thurm/thurmd for remote hosts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  # Rust from rust-toolchain.toml, the version CI uses, rather than nixpkgs' rustc.
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  # The dependencies build in a derivation of their own (crane), which the thurm Cachix cache keeps.
  inputs.crane.url = "github:ipetkov/crane";

  # The package (and its dependencies) for every main commit, built by CI's Nix workflow.
  nixConfig = {
    extra-substituters = [ "https://thurm.cachix.org" ];
    extra-trusted-public-keys = [ "thurm.cachix.org-1:WuDyAyzpnw27eEYA6LWBTKxRK7/uVuzzJk5RB6+ooKw=" ];
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
      ...
    }:
    let
      system = "aarch64-darwin";
      pkgsFor =
        s:
        import nixpkgs {
          system = s;
          overlays = [ rust-overlay.overlays.default ];
        };
      pkgs = pkgsFor system;
      rustVersion = (nixpkgs.lib.importTOML ./rust-toolchain.toml).toolchain.channel;
      # `thurm` and `thurmd` (the CLI and the session daemon) build everywhere a remote
      # workspace can live; the app itself is macOS only (macos/build.sh).
      packageSystems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forPackageSystems = f: nixpkgs.lib.genAttrs packageSystems (s: f (pkgsFor s));

      thurmFor =
        pkgs:
        let
          inherit (pkgs) lib;
          version = (lib.importTOML ./Cargo.toml).workspace.package.version;
          # libghostty-vt prebuilt by ghostty-vt-rs: the build has no network, so the archive is
          # fetched (and checked) here and unpacked for its build script
          # (GHOSTTY_VT_RS_SYS_PREBUILT_DIR). Zig is not needed.
          ghosttyVt =
            let
              pin = import ./nix/ghostty-vt.nix;
              archive = pin.targets.${pkgs.stdenv.hostPlatform.system};
              short = builtins.substring 0 12 pin.commit;
            in
            pkgs.runCommandLocal "ghostty-vt-${short}"
              {
                src = pkgs.fetchurl {
                  url = "https://github.com/nklmilojevic/ghostty-vt-rs/releases/download/ghostty-${short}/ghostty-vt-${short}-${archive.triple}.tar.gz";
                  inherit (archive) sha256;
                };
              }
              ''
                mkdir -p "$out"
                tar -xzf "$src" -C "$out"
              '';
          craneLib = (crane.mkLib pkgs).overrideToolchain (p: p.rust-bin.stable.${rustVersion}.minimal);
          # thurm-tailnet, the Tailscale helper thurmd runs to serve over Tailscale ([tailscale]).
          # Go from tailnet/go.mod's minor version, as macos/build.sh builds it.
          tailnet = (pkgs.buildGoModule.override { go = pkgs.go_1_27; }) {
            pname = "thurm-tailnet";
            inherit version;
            src = ./tailnet;
            vendorHash = "sha256-EkFanR5bVhb0S8FQEVFJ6ghZSdqzvJv3bmjLlZC9b+Y=";
            subPackages = [ "cmd/thurm-tailnet" ];
            env.CGO_ENABLED = 0;
            ldflags = [
              "-s"
              "-w"
            ];
          };
          common = {
            pname = "thurm";
            inherit version;
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./crates
                ./shell-integration
                ./completions
                # crates/thurm-cli/src/skill.rs includes skills/thurm/SKILL.md.
                ./skills
              ];
            };
            strictDeps = true;
            cargoExtraArgs = "--locked -p thurm-cli -p thurm-daemon";
            # ghostty-vt-rs-sys is a dependency: its build script links the prebuilt archive.
            env.GHOSTTY_VT_RS_SYS_PREBUILT_DIR = "${ghosttyVt}";
            # The suites need PTYs and sshd; CI runs them.
            doCheck = false;
          };
          # Only Cargo.toml and Cargo.lock reach it (crane replaces the sources with stubs), so it
          # stays the same, and cached, until a dependency changes.
          cargoArtifacts = craneLib.buildDepsOnly common;
        in
        craneLib.buildPackage (
          common
          // {
            inherit cargoArtifacts;
            env = common.env // {
              # `<version>+nix.<commit>`: the app treats it as its own build of that commit. Here
              # and not in the dependencies, which would otherwise change with every commit.
              THURM_BUILD = "${version}+nix.${self.shortRev or "dirty"}";
            };
            # thurmd looks for the helper beside its own executable.
            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              install -Dm755 ${tailnet}/bin/thurm-tailnet $out/bin/thurm-tailnet
            ''
            + lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
              installShellCompletion --cmd thurm \
                --bash <($out/bin/thurm completion bash) \
                --fish <($out/bin/thurm completion fish) \
                --zsh <($out/bin/thurm completion zsh)
            '';
            passthru = { inherit tailnet ghosttyVt cargoArtifacts; };
            meta = {
              description = "Thurm's CLI, session daemon (remote workspaces) and Tailscale helper";
              homepage = "https://github.com/nklmilojevic/thurm";
              license = lib.licenses.asl20;
              mainProgram = "thurm";
              platforms = packageSystems;
            };
          }
        );
      rustToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
        extensions = [
          "rust-src"
          "rust-analyzer"
        ];
        # The iOS app's core (thurm-ffi's `mobile` feature, built by the iOS app's repository),
        # and the browser client's (wasm32).
        targets = [
          "aarch64-apple-ios"
          "aarch64-apple-ios-sim"
          "wasm32-unknown-unknown"
        ];
      };
    in
    {
      packages = forPackageSystems (pkgs: rec {
        thurm = thurmFor pkgs;
        default = thurm;
      });

      # programs.thurm: config.toml, themes, and (off macOS, where the app ships it) the CLI.
      homeManagerModules = rec {
        thurm = import ./nix/hm-module.nix self;
        default = thurm;
      };

      # Everything but Xcode (Swift, the macOS SDK, codesign, notarytool), which comes from the
      # system: `nix develop`, or `direnv allow` with the .envrc.
      devShells.${system}.default = pkgs.mkShellNoCC {
        packages = with pkgs; [
          rustToolchain
          # libghostty-vt comes prebuilt; Zig 0.16 builds it from source instead
          # (GHOSTTY_VT_RS_SYS_BUILD=source, as the Arch package does).
          zig_0_16
          # The Tailscale helper (tailnet/); the same minor version as tailnet/go.mod.
          go_1_27
          python3
          uv
          # thurm-mosh's and thurm-direct's tests talk to a real mosh-server when there is one.
          mosh
          # Docs: cd docs && bun install && bun run dev. The web client: `just web`.
          bun
          # The web client's wasm: the same version as thurm-web's wasm-bindgen, and wasm-opt.
          wasm-bindgen-cli
          binaryen
          gh
        ];

        RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

        # No Nix compiler or SDK: cargo links with Xcode's clang, and swift/xcrun must see
        # Xcode's developer dir and SDK.
        shellHook = ''
          unset DEVELOPER_DIR SDKROOT MACOSX_DEPLOYMENT_TARGET IPHONEOS_DEPLOYMENT_TARGET NIX_CFLAGS_COMPILE NIX_LDFLAGS
        '';
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
