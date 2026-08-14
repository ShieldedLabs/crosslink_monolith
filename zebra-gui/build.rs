//! Decodes the window icon to raw RGBA at build time, so the crate itself carries no PNG decoder,
//! and embeds the Win32 icon resource that Explorer and the taskbar read off the .exe.

use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

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
