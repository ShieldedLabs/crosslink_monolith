{
  description = "Visual Lexicon mdBook";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        buildInputs = with pkgs; [
          mdbook
          # mdbook-admonish
          mdbook-katex
          mdbook-linkcheck
          mdbook-mermaid
        ];

      in
      {
        packages.default = pkgs.stdenv.mkDerivation {
          pname = "visual-lexicon";
          version = "0.1.0";
          src = ./.;

          inherit buildInputs;

          buildPhase = ''
            mdbook-mermaid install
            mdbook build
          '';

          installPhase = ''
            mkdir -p $out
            cp -r book/* $out/
          '';
        };

        devShells.default = pkgs.mkShell { inherit buildInputs; };
      }
    );
}
