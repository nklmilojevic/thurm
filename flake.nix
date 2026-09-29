{
  description = "Thurm: a macOS terminal (Apple silicon), and thurm/thurmd for remote hosts";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      system = "aarch64-darwin";
      pkgs = nixpkgs.legacyPackages.${system};
      # `thurm` and `thurmd` (the CLI and the session daemon) build everywhere a remote
      # workspace can live; the app itself is macOS only (macos/build.sh).
      packageSystems = [
        "aarch64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forPackageSystems = f: nixpkgs.lib.genAttrs packageSystems (s: f nixpkgs.legacyPackages.${s});

      thurmFor =
        pkgs:
        let
          inherit (pkgs) lib;
          version = (lib.importTOML ./Cargo.toml).workspace.package.version;
          # libghostty-vt is built from this Ghostty commit (vendor/libghostty-vt-sys/build.rs).
          ghostty = pkgs.fetchFromGitHub {
            owner = "ghostty-org";
            repo = "ghostty";
            rev = "b40acce58dcf77df52231c3798ea58e924647c89";
            hash = "sha256-jGBYacSDg7Ya/P3OpkuM+EK+kW4JAPIiGTxEdWMc6PQ=";
          };
          # Its Zig packages, fetched ahead (the build has no network): Ghostty's own list.
          ghosttyZigDeps = pkgs.callPackage ./nix/ghostty-zig-deps.nix { };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "thurm";
          inherit version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
              ./vendor
              ./shell-integration
              ./completions
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [
            "-p"
            "thurm-cli"
            "-p"
            "thurm-daemon"
          ];
          # Zig for build.rs only: its setup hook would replace cargo's phases.
          # (and Apple's libtool, which it uses to merge static libraries on macOS).
          nativeBuildInputs = [
            pkgs.zig_0_16
          ]
          ++ lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.cctools ];
          dontUseZigBuild = true;
          dontUseZigCheck = true;
          dontUseZigInstall = true;
          env = {
            GHOSTTY_SOURCE_DIR = "${ghostty}";
            GHOSTTY_ZIG_SYSTEM_DIR = "${ghosttyZigDeps}";
            # `<version>+nix.<commit>`: the app treats it as its own build of that commit.
            THURM_BUILD = "${version}+nix.${self.shortRev or "dirty"}";
          };
          preBuild = ''
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
            mkdir -p "$ZIG_GLOBAL_CACHE_DIR/tmp"
          '';
          # The suites need PTYs and sshd; CI runs them.
          doCheck = false;
          meta = {
            description = "Thurm's CLI and session daemon (remote workspaces)";
            homepage = "https://github.com/nklmilojevic/thurm";
            license = lib.licenses.asl20;
            mainProgram = "thurm";
            platforms = packageSystems;
          };
        };
    in
    {
      packages = forPackageSystems (pkgs: rec {
        thurm = thurmFor pkgs;
        default = thurm;
      });

      # Everything but Xcode (Swift, the macOS SDK, codesign, notarytool), which comes from the
      # system: `nix develop`, or `direnv allow` with the .envrc.
      devShells.${system}.default = pkgs.mkShellNoCC {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          rust-analyzer
          # libghostty-vt is built from source with Zig 0.16.
          zig_0_16
          python3
          uv
          # Docs: cd docs && bun install && bun run dev
          bun
          gh
        ];

        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

        # No Nix compiler or SDK: cargo links with Xcode's clang, and swift/xcrun must see
        # Xcode's developer dir and SDK.
        shellHook = ''
          unset DEVELOPER_DIR SDKROOT MACOSX_DEPLOYMENT_TARGET NIX_CFLAGS_COMPILE NIX_LDFLAGS
        '';
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
