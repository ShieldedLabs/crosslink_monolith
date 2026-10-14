# `nix` Support

The `crosslink_monolith` repository root provides a `nix flake` (`flake.nix`,
`flake.lock`, and the `flake/` directory) covering `zebra-crosslink` together
with its sibling crates. This page assumes you
[have flake support enabled](https://nixos.wiki/wiki/Flakes) (or pass the temporary cli flags for
flake support).

## Standard flake commands

From the repository root, the standard flake commands all work:

- `nix build`
- `nix flake check`
- `nix develop`

The built package includes the `zebrad` binary and this book (rendered).

Inside `nix develop`, build the `zebra-crosslink` workspace with:

```
cargo build --manifest-path ./zebra-crosslink/Cargo.toml
```
