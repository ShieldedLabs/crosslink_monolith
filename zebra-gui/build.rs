//! Decodes the window icon to raw RGBA at build time, so the crate itself carries no PNG decoder,
//! and embeds the Win32 icon resource that Explorer and the taskbar read off the .exe.

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    #[cfg(feature = "ape")]
    apeify();

    #[cfg(windows)]
    embed_icon_resource(&manifest_dir.parent().unwrap().join("packaging/icons/zebra-crosslink.ico"));

    let src = manifest_dir.join("assets/favicon.png");
    println!("cargo:rerun-if-changed={}", src.display());

    let mut decoder = png::Decoder::new(fs::File::open(&src).unwrap());
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    let pixels = &buf[..info.buffer_size()];

    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        other => panic!("{}: unsupported colour type {:?}", src.display(), other),
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out_dir.join("window_icon.rgba"), &rgba).unwrap();
    fs::write(
        out_dir.join("window_icon.rs"),
        format!(
            "pub static WINDOW_ICON_RGBA: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/window_icon.rgba\"));\n\
             pub const WINDOW_ICON_W: u32 = {};\n\
             pub const WINDOW_ICON_H: u32 = {};\n",
            info.width, info.height
        ),
    )
    .unwrap();
}

// The icon set on the window at runtime only skins the live window; Explorer, pinned
// shortcuts and the taskbar read RT_GROUP_ICON out of the binary itself. Compiling the
// resource needs rc.exe from the Windows SDK, so a missing one warns instead of failing.
#[cfg(windows)]
fn embed_icon_resource(ico: &std::path::Path) {
    println!("cargo:rerun-if-changed={}", ico.display());

    let rc = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app_icon.rc");
    let ico = ico.display().to_string().replace('\\', "/");
    fs::write(&rc, format!("1 ICON \"{}\"\n", ico)).unwrap();

    if let Err(err) = embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=application icon not embedded: {err}");
    }
}

/// Build the crate again, once per architecture, and fuse the two ELFs into one
/// Actually Portable Executable.
///
/// cosmo-build drives the *link* through cosmocc but leaves C compilation to
/// cc-rs, and this tree has plenty of it (clay-rs compiles clay.h; wallet pulls
/// bundled rusqlite, secp256k1 and zcash_script). cc-rs picks its compiler from
/// the target triple, and nothing on the host answers to `*-unknown-cosmo`, so
/// it is pointed at cosmocc's own cross compilers here. Setting them before
/// `apeify()` is what makes them stick: cosmo-build scrubs the toolchain
/// variables from the nested cargo's environment but passes everything else
/// through, and by the time that cargo runs a build script cosmocc is unpacked.
#[cfg(feature = "ape")]
fn apeify() {
    // Same default as cosmo-build's own Cache::locate.
    let cosmo_home = env::var_os("COSMO_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let base = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env::var("HOME").expect("HOME")).join(".cache"));
        base.join("cargo-cosmo")
    });
    let bin = cosmo_home.join("cosmocc").join("bin");

    // `<triple>-cc` / `-c++` are cosmocross, a plain shell script that adds the
    // things a bare `<arch>-linux-cosmo-gcc` has no idea about: -nostdinc, the
    // cosmopolitan include root, the normalize.inc prologue, -fno-pie and the
    // per-arch register reservations. Driving the raw gcc instead fails on
    // `#include <string.h>`, and hand-copying that flag set here would be a
    // second copy of cosmocc's own logic to keep in step.
    //
    // `ar` has no such driver and is a bare APE: no ELF magic and no shebang, so
    // the kernel refuses to execve it and cc-rs gets "Exec format error". It gets
    // a wrapper that hands it to /bin/sh, the same route cosmo-build's linker
    // shim takes.
    let wrappers = PathBuf::from(env::var("OUT_DIR").unwrap()).join("cosmocc-wrappers");
    fs::create_dir_all(&wrappers).unwrap();

    for (triple, arch) in [
        ("x86_64-unknown-cosmo",  "x86_64"),
        ("aarch64-unknown-cosmo", "aarch64"),
    ] {
        let cc  = bin.join(format!("{triple}-cc"));
        let cxx = bin.join(format!("{triple}-c++"));
        let ar  = shell_wrapper(&wrappers, &bin.join(format!("{arch}-linux-cosmo-ar")));

        // cc-rs accepts the triple with either dashes or underscores; set both so
        // it does not matter which spelling it looks up first.
        for key in [triple.to_string(), triple.replace('-', "_")] {
            set_if_unset(&format!("CC_{key}"),  &cc);
            set_if_unset(&format!("CXX_{key}"), &cxx);
            set_if_unset(&format!("AR_{key}"),  &ar);
        }
    }

    cosmo_build::apeify();
}

/// Write a shell script next to `dir` that runs `real` through /bin/sh, and hand
/// back its path. Needed for cosmocc's APE tools, which cannot be execve'd.
#[cfg(feature = "ape")]
fn shell_wrapper(dir: &std::path::Path, real: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(real.file_name().unwrap());
    fs::write(&path, format!("#!/bin/sh\nexec /bin/sh {} \"$@\"\n", real.display())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Leave an explicit override alone; a caller who set `CC_<triple>` meant it.
#[cfg(feature = "ape")]
fn set_if_unset(key: &str, value: &std::path::Path) {
    if env::var_os(key).is_none() {
        // Safety: build scripts are single-threaded at this point; nothing else
        // in this process reads the environment concurrently.
        unsafe { env::set_var(key, value); }
    }
}
