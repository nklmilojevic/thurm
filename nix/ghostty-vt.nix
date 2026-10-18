# The prebuilt libghostty-vt the Nix package links: ghostty-vt-rs-sys's Ghostty commit and, for
# each system the flake builds, the archive's target and SHA-256 (the crate's prebuilt.rs). CI
# checks that they agree; update them with the crate.
{
  commit = "1e9e14af5df14211892f5fc15fd21a49c7d94868";
  targets = {
    aarch64-darwin = {
      triple = "aarch64-apple-darwin";
      sha256 = "58c2d5ed1cfedbc4c5093717f16a8dc6edad4f27c1a3661ba09fefbf3c526270";
    };
    x86_64-linux = {
      triple = "x86_64-unknown-linux-gnu";
      sha256 = "5106c2a58491733f589f04d2af1401e22d32947fd7aa11d1719b5c26bfe529ce";
    };
    aarch64-linux = {
      triple = "aarch64-unknown-linux-gnu";
      sha256 = "73369271de63054a07b575a5020ff43b827af61626fb1531a9f20475827c67f3";
    };
  };
}
