# The prebuilt libghostty-vt the Nix package links: ghostty-vt-rs-sys's Ghostty commit and, for
# each system the flake builds, the archive's target and SHA-256 (the crate's prebuilt.rs). CI
# checks that they agree; update them with the crate.
{
  commit = "573c4a21e02311ae87844af26b6a7a179f82858e";
  targets = {
    aarch64-darwin = {
      triple = "aarch64-apple-darwin";
      sha256 = "138eaa723c92c444203f994914d6ddee5994f60d72d2b357e1ea39b4f70a3e16";
    };
    x86_64-linux = {
      triple = "x86_64-unknown-linux-gnu";
      sha256 = "69ce162abb4fff54135489a2e970a3de7ca5cdcd5b27f97d537a39c2242b4d6e";
    };
    aarch64-linux = {
      triple = "aarch64-unknown-linux-gnu";
      sha256 = "8b9e3b8507cb75f75bdb42b34e45fbb38bd0da8f401b8000f1c2a4a0fc412125";
    };
  };
}
