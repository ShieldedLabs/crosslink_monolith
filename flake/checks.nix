# `nix flake check` checks.
#
# Every package is also a check, so `nix flake check` builds the GUI binary
# and the book in addition to the lint/format checks below.
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
        rustfmt = craneLib.cargoFmt crateArgs;

        cargo-doc = craneLib.cargoDoc crateArgs;

        cargo-nextest = craneLib.cargoNextest crateArgs;

        # Our own nix files only: the subtrees (`crosslink_book`, `zips`, ...)
        # carry their own flakes and formatting conventions.
        nixfmt =
          pkgs.runCommand "crosslink-monolith-nixfmt"
            {
              nativeBuildInputs = [ pkgs.nixfmt-rfc-style ];
            }
            ''
              nixfmt --check --strict ${../flake.nix} ${./.}/*.nix
              touch "$out"
            '';
      };
    };
}
