# `nix develop` shell.
#
# Provides the same pinned Rust toolchain and native build inputs as the
# `nix build` packages, so that from the project root:
#
# ```
# $ nix develop --command cargo build --manifest-path ./zebra-crosslink/Cargo.toml
# ```
#
# builds against the working tree (siblings like `../zebra-gui` resolve in
# place, no copying needed).
{ ... }:
{
  perSystem =
    {
      pkgs,
      self',
      craneLib,
      crateCommonArgs,
      ...
    }:
    {
      devShells.default = craneLib.devShell {
        inputsFrom = [ self'.packages.zebrad ];

        packages = with pkgs; [
          cargo-nextest
          mdbook
          mdbook-mermaid
          nixfmt-rfc-style
          yamllint
        ];

        inherit (crateCommonArgs) LIBCLANG_PATH;
      };
    };
}
