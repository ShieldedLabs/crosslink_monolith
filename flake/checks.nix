# `nix flake check` checks.
#
# Every package is also a check, so `nix flake check` builds the GUI binary
# and the book in addition to the lint/format checks below.
{ ... }: {
  perSystem =
    {
      pkgs,
      self',
      craneLib,
      crateCommonArgs,
      ...
    }:
    let
      # See `./toolchain.nix` for why no shared `buildDepsOnly` artifacts.
      crateArgs = crateCommonArgs // {
        pname = "zebra-crosslink-workspace";
        version = "0.0.0";
        cargoArtifacts = null;
      };
    in
    {
      checks = self'.packages // {
        cargo-doc = craneLib.cargoDoc crateArgs;

        # TODO: `craneLib.cargoFmt` and `craneLib.cargoNextest` checks belong
        # here too, but the workspace is not yet `rustfmt`-clean and some
        # `zebra-chain` test vectors fail to deserialize, so they would keep
        # `nix flake check` permanently red.

        # Our own nix files only: the subtrees (`crosslink_book`, `zips`, ...)
        # carry their own flakes and formatting conventions.
        nixfmt = pkgs.runCommand "crosslink-monolith-nixfmt" { nativeBuildInputs = [ pkgs.nixfmt ]; } ''
          nixfmt --check --strict ${../flake.nix} ${./.}/*.nix
          touch "$out"
        '';
      };
    };
}
