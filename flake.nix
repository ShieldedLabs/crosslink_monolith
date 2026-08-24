{
  description = "crosslink_monolith flake delegating to zebra-crosslink";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-utils.url = "github:numtide/flake-utils";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    inputs:
    import ./zebra-crosslink/flake/outputs.nix {
      project-name = "zebra-crosslink";
      src-root = ./zebra-crosslink;
      rust-toolchain-toml = ./zebra-crosslink/rust-toolchain.toml;
      flake-lib-path = ./zebra-crosslink/flake;
      nixfmt-check-paths = [
        ./flake.nix
        ./zebra-crosslink
      ];
      inherit (inputs)
        flake-utils
        nixpkgs
        crane
        rust-overlay
        advisory-db
        ;
    };
}
