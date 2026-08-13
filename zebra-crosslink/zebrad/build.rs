//! Build script for zebrad.

use vergen_git2::{CargoBuilder, Emitter, Git2Builder, RustcBuilder};

/// Process entry point for `zebrad`'s build script
#[allow(clippy::print_stderr)]
fn main() {
    #[cfg(windows)]
    embed_icon_resource();
    let mut emitter = Emitter::default();
    // Dependency instructions run nested `cargo metadata`, which cannot resolve
    // unpublished workspace versions during a multi-package publish.
    let cargo = CargoBuilder::default()
        .debug(true)
        .features(true)
        .opt_level(true)
        .target_triple(true)
        .build()
        .expect("requested cargo instructions should build successfully");

    // Configures an [`Emitter`] for everything except for `git` env vars.
    // This builder fails the build on error.
    emitter
        .fail_on_error()
        .add_instructions(&cargo)
        .expect("adding cargo instructions should succeed")
        .add_instructions(
            &RustcBuilder::all_rustc().expect("all_rustc() should build successfully"),
        )
        .expect("adding all_rustc() instructions should succeed");

    // Get git information. This is used by e.g. ZebradApp::register_components()
    // to log the commit hash
    let all_git = Git2Builder::default()
        .branch(true)
        .commit_author_email(true)
        .commit_author_name(true)
        .commit_count(true)
        .commit_date(true)
        .commit_message(true)
        .commit_timestamp(true)
        .describe(false, false, None)
        .sha(true)
        .dirty(false)
        .describe(true, true, Some("v*.*.*"))
        .build()
        .expect("all_git + describe + sha should build successfully");

    if let Err(e) = emitter.add_instructions(&all_git) {
        // The most common failure here is due to a missing `.git` directory,
        // e.g., when building from `cargo install zebrad`. We simply
        // proceed with the build.
        // Note that this won't be printed unless in cargo very verbose mode (-vv).
        // We could emit a build warning, but that might scare users.
        println!("git error in vergen build script: skipping git env vars: {e:?}",);
    }

    emitter.emit().expect("base emit should succeed");

    #[cfg(feature = "lightwalletd-grpc-tests")]
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_protos(
            &["tests/common/lightwalletd/proto/service.proto"],
            &["tests/common/lightwalletd/proto"],
        )
        .expect("Failed to generate lightwalletd gRPC files");
}

// Explorer, pinned shortcuts and the taskbar read RT_GROUP_ICON out of the binary; the
// GUI's runtime icon covers only a window that is already open. Compiling the resource
// needs rc.exe from the Windows SDK, so a missing one warns instead of failing.
#[cfg(windows)]
fn embed_icon_resource() {
    use std::{env, fs, path::PathBuf};

    let repo_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf();
    let ico = repo_root.join("packaging/icons/zebra-crosslink.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let rc = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app_icon.rc");
    let ico = ico.display().to_string().replace('\\', "/");
    fs::write(&rc, format!("1 ICON \"{}\"\n", ico)).unwrap();

    if let Err(err) = embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=application icon not embedded: {err}");
    }
}
