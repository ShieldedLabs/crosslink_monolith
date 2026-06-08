# flakelib:
#
# The "generic plumbing" for the `zebra-crosslink` flake.
#
# This utility library is written to be general purpose as much as
# possible for "any rust workspace project", with `zebra`-specific
# parameters passed as the second argument attrset on import.

# flake-inputs:
{
  self,
  nixpkgs,
  crane,
  rust-overlay,
  flake-utils,
  advisory-db,
}:
# Our application-specific parameters:
{
  pname,
  src-root,
  rust-toolchain-toml,
  system,
}:
let
  pkgs = import nixpkgs {
    inherit system;
    overlays = [ (import rust-overlay) ];
  };

  # crane-lib provides a rust build/deps API bound to `pkgs` with the rust toolchain version specified in `./rust-toolchain.toml`:
  crane-lib =
    let
      # This function is named for call-site readability:
      fromToolchainFile = p: p.rust-bin.fromRustupToolchainFile rust-toolchain-toml;
    in
    (crane.mkLib pkgs).overrideToolchain fromToolchainFile;

  flakelib = rec {
    nixpkgs = pkgs;

    # select-source :: {
    #   name :: String,
    #   paths :: [ Path or FileSet ],
    # } -> Source
    #
    # Create a Source with the given name which includes the given
    # paths that can be directories or files. When a directory is
    # encountered, all contained contents are also included.
    select-source =
      { name-suffix, paths }:
      let
        inherit (builtins) map;
        inherit (pkgs.lib.fileset) toSource unions;
        inherit (pkgs.lib.trivial) flip;
        inherit (pkgs) symlinkJoin;
        inherit run-command;

        base-name = "${pname}-src-${name-suffix}";

        # NB: We have to un-symlink as a work-around for a crane bug:
        copy-symlinks = p: run-command base-name [ ] ''cp -rL '${p}' "$out"'';
      in
      copy-symlinks (symlinkJoin {
        name = "${base-name}-symlinks";
        paths = [
          (toSource {
            root = src-root;
            fileset = unions paths;
          })
        ];
      });

    # links-table :: (Name :: String) -> { relpath -> [Deriv or Path] } -> Derivation
    #
    # Create a derivation which maps relpath's to target paths or
    # derivations which come from a table (attrset names are relpaths).
    links-table =
      let
        inherit (pkgs.lib.attrsets) mapAttrsToList;
        inherit (pkgs) linkFarm;

        kv-to-entry = name: path: { inherit name path; };
      in
      name-suffix: table: linkFarm "${pname}-${name-suffix}" (mapAttrsToList kv-to-entry table);

    # run-command :: (name-suffix :: String) -> [ BuildInputs ] -> Script -> Derivation
    #
    # A wrapper around pkgs.runCommand specialized to take only `buildInputs`.
    run-command =
      name-suffix: buildInputs: script:
      pkgs.runCommand "${pname}-cmd-${name-suffix}" { inherit buildInputs; } script;

    # build-rust-workspace :: (crate :: Path) -> (common-args :: Attrset) -> { pkg :: Derivation, checks, args, artifacts }
    #
    # Provide derivations for a crates binaries, arguments, dependency
    # artifacts, and various flake checks.
    build-rust-workspace =
      target-crate: common:
      let
        # Build *just* the cargo dependencies (of the entire workspace),
        # so we can reuse all of that work (e.g. via cachix) when running in CI
        artifacts = crane-lib.buildDepsOnly (
          common
          // {
            pname = "${pname}-dependency-artifacts";
            version = "0.0.0"; # TODO: Fix this to workspace-wide version
          }
        );

        args = {
          inherit common;

          crate = (
            let
              cargoToml = target-crate + "/Cargo.toml";
              meta = crane-lib.crateNameFromCargoToml { inherit cargoToml; };
            in
            common
            // {
              cargoArtifacts = artifacts;
              inherit (meta) pname version;
            }
          );
        };

      in
      {
        inherit args artifacts;

        pkg = crane-lib.buildPackage args.crate;

        checks = {
          # clippy = crane-lib.cargoClippy (args.crate // {
          #   cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          # });

          # TODO: make this a standard build package:
          cargo-doc = crane-lib.cargoDoc args.crate;

          rustfmt = crane-lib.cargoFmt args.crate;

          # toml-format = crane-lib.taploFmt {
          #   src = pkgs.lib.sources.sourceFilesBySuffices src-root [ ".toml" ];
          #   # taplo arguments can be further customized below as needed
          #   # taploExtraArgs = "--config ./taplo.toml";
          # };

          # Audit dependencies
          #
          # TODO: Most projects that don't use this frequently have errors due to known vulnerabilities in transitive dependencies! We should probably re-enable them on a cron-job (since new disclosures may appear at any time and aren't a property of a revision alone).
          #
          # audit = crane-lib.cargoAudit (args.common // {
          #   inherit src advisory-db;
          # });

          # Audit licenses
          #
          # TODO: Zebra fails these license checks.
          #
          # cargo-deny = crane-lib.cargoDeny args.common;

          # Run tests with cargo-nextest
          # Consider setting `doCheck = false` on other crate derivations
          # if you do not want the tests to run twice
          #
          # TODO: Ensure the "PR merge acceptance" tests are run identically to CI:
          cargo-nextest = crane-lib.cargoNextest (
            args.crate
            // {
              partitions = 1;
              partitionType = "count";
            }
          );
        };
      };
  };
in
flakelib
