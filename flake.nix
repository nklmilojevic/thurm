{
  description = "Thurm: a macOS terminal (Apple silicon)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      system = "aarch64-darwin";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
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
          # Docs: uv run --with-requirements requirements-docs.txt mkdocs serve
          uv
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
