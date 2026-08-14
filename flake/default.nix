# Top-level `flake-parts` module: wires together the modules in this
# directory. Each module contributes to `perSystem` (packages, checks,
# devShells, ...) for every system listed below.
{ inputs, ... }:
{
  systems = [
    "x86_64-linux"
    "aarch64-linux"
    "x86_64-darwin"
    "aarch64-darwin"
  ];

  # TODO: this placeholder package/check will be replaced by the real
  # zebrad(viz_gui) build in a follow-up commit.
  perSystem =
    { system, ... }:
    let
      pkgs = import inputs.nixpkgs { inherit system; };
    in
    {
      packages.default = pkgs.writeText "placeholder" "replaced by the real GUI build shortly\n";
      checks.placeholder = pkgs.runCommand "placeholder-check" { } "touch $out";
    };
}
