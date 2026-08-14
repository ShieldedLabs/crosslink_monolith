//! Build script for zebrad.

fn main() {
    #[cfg(windows)]
    embed_icon_resource();

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
