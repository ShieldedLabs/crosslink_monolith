# Build, test, and development environment for the `crosslink_monolith` workspace.
#
# # Prerequisites
#
# - Install the `nix` package manager: https://nixos.org/download/
# - Configure `flake` support: https://nixos.wiki/wiki/Flakes
#
# # Build
#
# ```
# $ nix build --print-build-logs
# ```
#
# This produces the default package's outputs in `./result`, including the
# GUI binary at `./result/bin/`.
#
# # Check
#
# ```
# $ nix flake check
# ```
#
# This runs formatting/lint/test checks (see `./flake/checks.nix`).
#
# # Development
#
# ```
# $ nix develop
# ```
#
# This starts a subshell with a development environment (`cargo`, `clang`,
# `protoc`, etc).
#
# Everything beyond this file lives under `./flake/`, so people unfamiliar
# with `nix` only need to know about `./flake{/,.nix,.lock}`.
{
  description = "crosslink_monolith: zebra-crosslink node + wallet + GUI workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{ flake-parts, ... }: flake-parts.lib.mkFlake { inherit inputs; } (import ./flake);
}
