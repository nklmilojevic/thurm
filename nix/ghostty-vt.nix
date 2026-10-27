# The prebuilt libghostty-vt the Nix package links: ghostty-vt-rs-sys's Ghostty commit and, for
# each system the flake builds, the archive's target and SHA-256 (the crate's prebuilt.rs). CI
# checks that they agree; update them with the crate.
{
  commit = "4a4fc017940bec38c93b278c10c6a18c591f3e63";
  targets = {
    aarch64-darwin = {
      triple = "aarch64-apple-darwin";
      sha256 = "6e2a4196776954fc409e3cbf5903d4886c5186efb7d43ce1b3d37b187d18b36c";
    };
    x86_64-linux = {
      triple = "x86_64-unknown-linux-gnu";
      sha256 = "54f1b939ed1cce662d20e3d58829bdf4c15ee2b707785318d7cd912166227c8a";
    };
    aarch64-linux = {
      triple = "aarch64-unknown-linux-gnu";
      sha256 = "d139ff8f1431f83c74789109a187ea511792450639b7676d317c9c8dd46bc41b";
    };
  };
}
