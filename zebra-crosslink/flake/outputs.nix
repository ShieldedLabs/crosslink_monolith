# Shared flake outputs for zebra-crosslink and monorepo wrappers.
{
  project-name,
  src-root,
  rust-toolchain-toml,
  flake-lib-path,
  flake-utils,
  nixpkgs,
  crane,
  rust-overlay,
  advisory-db,
  nixfmt-check-paths,
}: flake-utils.lib.eachDefaultSystem (
  system:
  let
    # Local utility library:
    flakelib = import flake-lib-path {
      inherit
        nixpkgs
        crane
        rust-overlay
        flake-utils
        advisory-db
        ;
      self = null;
    } {
      pname = "${project-name}-workspace";
      inherit
        src-root
        rust-toolchain-toml
        system
        ;
    };

    inherit (flakelib)
      build-rust-workspace
      links-table
      nixpkgs
      run-command
      select-source
      ;

    # We use this style of nix formatting in checks and the dev shell:
    nixfmt = nixpkgs.nixfmt-rfc-style;

    # We use the latest nixpkgs `libclang`:
    inherit (nixpkgs.llvmPackages) libclang;

    src-book = select-source {
      name-suffix = "book";
      paths = [
        (src-root + "/book")
        (src-root + "/README.md")
      ];
    };

    src-rust = select-source {
      name-suffix = "rust";
      paths = [
        (src-root + "/.cargo")
        (src-root + "/.config")
        (src-root + "/Cargo.lock")
        (src-root + "/Cargo.toml")
        (src-root + "/clippy.toml")
        (src-root + "/crosslink-test-data")
        (src-root + "/release.toml")
        (src-root + "/rust-toolchain.toml")
        (src-root + "/tower-batch-control")
        (src-root + "/tower-fallback")
        (src-root + "/zebra-chain")
        (src-root + "/zebra-consensus")
        (src-root + "/zebra-crosslink")
        (src-root + "/zebra-grpc")
        (src-root + "/zebra-network")
        (src-root + "/zebra-node-services")
        (src-root + "/zebra-rpc")
        (src-root + "/zebra-scan")
        (src-root + "/zebra-script")
        (src-root + "/zebra-state")
        (src-root + "/zebra-test")
        (src-root + "/zebra-utils")
        (src-root + "/zebrad")
      ];
    };

    zebrad-outputs = build-rust-workspace (src-root + "/zebrad") {
      src = src-rust;

      strictDeps = true;

      # Note: we disable tests since we'll run them all via cargo-nextest
      doCheck = false;

      # Use the clang stdenv, overriding any downstream attempt to alter it:
      stdenv = _: nixpkgs.llvmPackages.stdenv;

      nativeBuildInputs = with nixpkgs; [
        pkg-config
        protobuf
      ];

      buildInputs = with nixpkgs; [
        libclang
        rocksdb
      ];

      # Additional environment variables can be set directly
      LIBCLANG_PATH = "${libclang.lib}/lib";
    };

    zebrad = zebrad-outputs.pkg;

    zebra-book = nixpkgs.stdenv.mkDerivation rec {
      name = "zebra-book";
      src = src-book;
      buildInputs = with nixpkgs; [
        mdbook
        mdbook-mermaid
      ];
      builder = nixpkgs.writeShellScript "${name}-builder.sh" ''
        if mdbook build --dest-dir "$out/book/book" "$src/book" 2>&1 | grep -E 'ERROR|WARN'
        then
          echo 'Failing due to mdbook errors/warnings.'
          exit 1
        fi
      '';
    };

    # Invoke `check_path` for each configured nixfmt check path.
    render-path = p: "check_path '${p}'";

    nixfmt-check-script = builtins.concatStringsSep "\n" (map render-path nixfmt-check-paths);
  in
  {
    packages = (
      let
        base-pkgs = {
          inherit
            zebrad
            zebra-book
            src-book
            src-rust
            ;
        };

        all = links-table "all" {
          "./bin" = "${zebrad}/bin";
          "./book" = "${zebra-book}/book";
          "./src/${project-name}/book" = "${src-book}/book";
          "./src/${project-name}/rust" = src-rust;
        };
      in

      base-pkgs
      // {
        inherit all;
        default = all;
      }
    );

    checks = (
      zebrad-outputs.checks
      // {
        # Build the crates as part of `nix flake check` for convenience
        inherit zebrad;

        # Check formatting
        nixfmt-check = run-command "nixfmt" [ nixfmt ] ''
          set -efuo pipefail
          exitcode=0
          check_file() {
            local f="$1"
            printf '+ nixfmt --check --strict %q\n' "$f"
            nixfmt --check --strict "$f" || exitcode=1
          }
          check_path() {
            local path="$1"
            if [ -d "$path" ]
            then
              while IFS= read -r -d "" f
              do
                check_file "$f"
              done < <(find "$path" -type f -name '*.nix' -print0)
            elif [ -f "$path" ]
            then
              check_file "$path"
            fi
          }
          ${nixfmt-check-script}
          [ "$exitcode" -eq 0 ] && touch "$out" # signal success to nix
          exit "$exitcode"
        '';
      }
    );

    apps = {
      zebrad = flake-utils.lib.mkApp { drv = zebrad; };
    };

    # TODO: BEWARE: This dev shell may have buggy deviations from the build.
    devShells.default = (
      let
        mkClangShell = nixpkgs.mkShell.override { inherit (nixpkgs.llvmPackages) stdenv; };

        devShellInputs = with nixpkgs; [
          rustup
          mdbook
          mdbook-mermaid
          nixfmt
          yamllint
        ];

        dynlibs = with nixpkgs; [
          libGL
          libxkbcommon
          xorg.libX11
          xorg.libxcb
          xorg.libXi
        ];

        crate-args = zebrad-outputs.args.crate;
      in
      mkClangShell (
        crate-args
        // {
          # Include devShell inputs:
          nativeBuildInputs = crate-args.nativeBuildInputs ++ devShellInputs;

          LD_LIBRARY_PATH = nixpkgs.lib.makeLibraryPath dynlibs;
        }
      )
    );
  }
)
