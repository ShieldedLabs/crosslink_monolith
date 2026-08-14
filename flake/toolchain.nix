# Shared Rust toolchain, crate source, and `crane` build plumbing.
#
# `crane` assumes the `src` it's given *is* the cargo workspace root (its
# dummy-source generation for `buildDepsOnly`, and its post-build
# `cargo metadata` install step, both look for `Cargo.toml`/`Cargo.lock`
# right at `${src}`, with no way to point them at a nested manifest). So
# `crateSrc` here is rooted at `zebra-crosslink/` itself. But the GUI
# binary (`zebrad` built with the `viz_gui` feature) has path-dependencies
# on sibling directories (`zebra-gui`, `clay-rs`, `librustzcash`,
# `tenderlink`, `patches`) that live *outside* that workspace. `postUnpack`
# copies those into place next to the unpacked source before cargo ever
# runs, so `../zebra-gui` etc. resolve exactly as they do in the working
# tree.
#
# This module publishes `pkgs`, `craneLib`, `crateSrc`, and
# `crateCommonArgs` as `perSystem` args (via `_module.args`) so other
# modules (`packages.nix`, `checks.nix`, `devshells.nix`) can consume them
# without recomputing the toolchain.
#
# NB: we deliberately don't use `craneLib.buildDepsOnly` to pre-warm a
# shared dependency cache. Its dummy-source generation only stubs out
# crates *inside* `crateSrc` (the `zebra-crosslink` workspace) - the real
# `zebra-gui` sibling (copied in unstubbed, see below) depends on the
# now-stubbed `wallet` workspace member and fails to compile against its
# empty placeholder API. Every package/check below simply builds against
# the real source instead.
{ inputs, ... }:
{
  perSystem =
    { system, ... }:
    let
      pkgs = import inputs.nixpkgs {
        inherit system;
        overlays = [ (import inputs.rust-overlay) ];
      };

      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ../zebra-crosslink/rust-toolchain.toml;

      craneLib = ((inputs.crane.mkLib pkgs).overrideToolchain (_: rustToolchain)).overrideScope (
        _final: _prev: {
          # Use the clang stdenv for every crate build (e.g. `rocksdb`'s C++ bindings).
          stdenvSelector = p: p.llvmPackages.stdenv;
        }
      );

      # A plain path (rather than `lib.fileset.toSource`) so the unpacked
      # source directory is named `zebra-crosslink` (matching its store
      # path's basename) instead of the generic `source` - some sibling
      # path-dependencies (e.g. `zebra-gui`'s `../zebra-crosslink/wallet`)
      # hardcode that directory name.
      crateSrc = ../zebra-crosslink;

      # Sibling directories `zebra-crosslink`'s path-dependencies reach via
      # `../`, copied into place next to the unpacked workspace source.
      siblingSrcs = {
        "zebra-gui" = ../zebra-gui;
        "clay-rs" = ../clay-rs;
        "librustzcash" = ../librustzcash;
        "tenderlink" = ../tenderlink;
        "patches" = ../patches;
      };

      # NB: `postUnpack` runs with `$PWD` at the *parent* of the unpacked
      # source directory (i.e. before `genericBuild` `cd`s into it), so
      # `${name}` here (not `../${name}`) lands the siblings next to it.
      copySiblingSrcs = pkgs.lib.concatStrings (
        pkgs.lib.mapAttrsToList (name: path: ''
          cp -r --no-preserve=mode,ownership ${path} ${name}
        '') siblingSrcs
      );

      # `crane`'s auto-vendoring writes `[source."<url>"]` replacement
      # entries for git dependencies without the `git+` scheme prefix that
      # `cargo` itself requires there (compare `cargo vendor`'s own output).
      # The mismatch means `cargo` doesn't recognize the replacement for
      # crates patched via `[patch.crates-io]` onto a git source (e.g.
      # `core2`, patched because it was yanked from crates.io), and tries
      # to reach the network in the sandboxed build. Patch the prefix back in.
      cargoVendorDir = pkgs.runCommand "crosslink-monolith-vendor" { } ''
        cp -r --no-preserve=mode ${craneLib.vendorCargoDeps { src = crateSrc; }} "$out"
        sed -i -E 's#^\[source\."(https?://)#[source."git+\1#' "$out/config.toml"
      '';

      # Native libraries needed to *build* zebrad(viz_gui): rocksdb/protobuf
      # for the node, and the X11/GL stack the `winit`/`softbuffer`-based
      # GUI links against.
      crateCommonArgs = {
        src = crateSrc;
        strictDeps = true;

        inherit cargoVendorDir;
        postUnpack = copySiblingSrcs;

        nativeBuildInputs = with pkgs; [
          pkg-config
          protobuf
        ];

        buildInputs =
          with pkgs;
          [
            llvmPackages.libclang
            rocksdb
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [
            libGL
            libxkbcommon
            libx11
            libxcb
            libxi
            libxcursor
            libxrandr
          ];

        LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
      };
    in
    {
      _module.args.pkgs = pkgs;
      _module.args.craneLib = craneLib;
      _module.args.crateSrc = crateSrc;
      _module.args.crateCommonArgs = crateCommonArgs;
    };
}
