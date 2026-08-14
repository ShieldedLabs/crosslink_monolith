//! Decodes the window icon to raw RGBA at build time, so the crate itself carries no PNG decoder.

use std::{env, fs, path::PathBuf};

fn main() {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets/favicon.png");
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
