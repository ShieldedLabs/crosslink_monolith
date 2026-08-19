# Top-level `flake-parts` module: wires together the modules in this
# directory. Each module contributes to `perSystem` (packages, checks,
# devShells, ...) for every system listed below.
{ ... }:
{
  systems = [
    "x86_64-linux"
    "aarch64-linux"
    "x86_64-darwin"
    "aarch64-darwin"
  ];

  imports = [
    ./toolchain.nix
    ./packages.nix
  ];
}
