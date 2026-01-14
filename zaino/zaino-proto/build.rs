#![allow(missing_docs)]

// use sha2::{Digest, Sha256};
use std::{
    env,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};
// use walkdir::WalkDir;

const PROTO_DIR: &str = "proto";
const COMPACT: &str = "proto/compact_formats.proto";
const SERVICE: &str = "proto/service.proto";
const PROPOSAL: &str = "proto/proposal.proto";

fn main() -> io::Result<()> {
    emit_rerun_directives();

    // Abort early if protoc is missing.
    if !protoc_available() {
        println!("cargo:warning=protoc not found; using committed proto output");
        return Ok(());
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // println!("cargo:warning=Protos changed; rebuilding…");
    build(&out_dir)?;
    Ok(())
}

fn protoc_available() -> bool {
    env::var_os("PROTOC")
        .map(PathBuf::from)
        .or_else(|| which::which("protoc").ok())
        .is_some()
}

fn emit_rerun_directives() {
    // Ensure Cargo reruns the script when proto files change.
    println!("cargo:rerun-if-changed={PROTO_DIR}/");

    // Optionally: rerun if build.rs itself changes.
    println!("cargo:rerun-if-changed=build.rs");
}

fn build(out_dir: &Path) -> io::Result<()> {
    // Build the compact format types.
    tonic_prost_build::compile_protos(COMPACT)?;

    // Copy the generated types into the source tree so changes can be committed.
    fs::copy(
        out_dir.join("cash.z.wallet.sdk.rpc.rs"),
        "src/proto/compact_formats.rs",
    )?;

    // Build the gRPC types and client.
    tonic_prost_build::configure()
        .build_server(true)
        .extern_path(
            ".cash.z.wallet.sdk.rpc.ChainMetadata",
            "crate::proto::compact_formats::ChainMetadata",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactBlock",
            "crate::proto::compact_formats::CompactBlock",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactTx",
            "crate::proto::compact_formats::CompactTx",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactSaplingSpend",
            "crate::proto::compact_formats::CompactSaplingSpend",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactSaplingOutput",
            "crate::proto::compact_formats::CompactSaplingOutput",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactOrchardAction",
            "crate::proto::compact_formats::CompactOrchardAction",
        )
        .compile_protos(&[SERVICE], &[PROTO_DIR])?;

    // Copy the generated types into the source tree so changes can be committed.
    //
    //
    fs::copy(out_dir.join("cash.z.wallet.sdk.rpc.rs"), "src/proto/service.rs")?;

    // Build the proposal types.
    tonic_prost_build::compile_protos(PROPOSAL)?;

    // Copy the generated types into the source tree so changes can be committed.
    fs::copy(out_dir.join("cash.z.wallet.sdk.ffi.rs"), "src/proto/proposal.rs")?;

    Ok(())
}
