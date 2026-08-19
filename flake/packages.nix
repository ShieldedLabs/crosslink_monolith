# `nix build` outputs.
#
# The default package is a unix-like tree: `./result/bin/` holds the GUI
# binary (`zebrad`, built with the `viz_gui` feature), and `./result/doc/`
# holds the rendered book.
{ ... }:
{
  perSystem =
    {
      pkgs,
      craneLib,
      crateCommonArgs,
      ...
    }:
    let
      zebrad-meta = craneLib.crateNameFromCargoToml { cargoToml = ../zebra-crosslink/zebrad/Cargo.toml; };

      # The GUI binary: `zebrad` built with the `viz_gui` feature enables the
      # embedded visualizer (see `zebra-crosslink/giorun.sh`: `cargo run -F viz_gui`).
      zebrad = craneLib.buildPackage (
        crateCommonArgs
        // {
          pname = zebrad-meta.pname;
          version = zebrad-meta.version;
          cargoExtraArgs = "-p zebrad --features viz_gui";

          # `buildPackage` otherwise auto-runs `buildDepsOnly` (see
          # `../flake/toolchain.nix` for why that's incompatible here).
          cargoArtifacts = null;

          # Tests run separately via the `cargo-nextest` check.
          doCheck = false;
        }
      );

      book = pkgs.stdenv.mkDerivation {
        pname = "zebra-crosslink-book";
        version = "0.0.0";
        src = pkgs.lib.fileset.toSource {
          root = ../zebra-crosslink/book;
          fileset = ../zebra-crosslink/book;
        };
        nativeBuildInputs = with pkgs; [
          mdbook
          mdbook-mermaid
        ];
        dontConfigure = true;
        buildPhase = ''
          runHook preBuild
          if mdbook build --dest-dir "$PWD/out" . 2>&1 | grep -E 'ERROR|WARN'
          then
            echo 'Failing due to mdbook errors/warnings.'
            exit 1
          fi
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p "$out"
          cp -r out "$out/book"
          runHook postInstall
        '';
      };

      default = pkgs.linkFarm "crosslink-monolith" {
        "bin" = "${zebrad}/bin";
        "doc" = "${book}/book";
      };
    in
    {
      packages = {
        inherit zebrad book default;
      };
    };
}
