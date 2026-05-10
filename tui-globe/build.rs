// SPDX-License-Identifier: GPL-3.0-or-later

//! Build-time compression for the embedded globe geometry.
//!
//! Quantizes `land_positions.gl` from f32 to i16 (positions live on the unit
//! sphere, so each axis is in [-1, 1] and i16 gives ~3e-5 angular resolution,
//! well below a Braille dot at any sane terminal size). All three buffers are
//! then zstd-compressed and written to `OUT_DIR`; `lib.rs` decodes them once
//! in `MapData::embedded`.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

const ZSTD_LEVEL: i32 = 19;

fn quantize_positions(bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len().is_multiple_of(4));
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for c in bytes.chunks_exact(4) {
        let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        let q = (f.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&q.to_le_bytes());
    }
    out
}

fn write_compressed(out: &Path, name: &str, bytes: &[u8]) {
    let compressed = zstd::encode_all(bytes, ZSTD_LEVEL).expect("zstd encode");
    fs::write(out.join(name), compressed).expect("write OUT_DIR");
}

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let assets = PathBuf::from("assets/geo");

    let pos_src = assets.join("land_positions.gl");
    let cidx_src = assets.join("land_contour_indices.gl");
    let tidx_src = assets.join("land_triangle_indices.gl");
    println!("cargo:rerun-if-changed={}", pos_src.display());
    println!("cargo:rerun-if-changed={}", cidx_src.display());
    println!("cargo:rerun-if-changed={}", tidx_src.display());
    println!("cargo:rerun-if-changed=build.rs");

    let pos_raw = fs::read(&pos_src).expect("read land_positions.gl");
    let pos_q = quantize_positions(&pos_raw);
    write_compressed(&out_dir, "land_positions.q16.zst", &pos_q);

    let cidx = fs::read(&cidx_src).expect("read land_contour_indices.gl");
    write_compressed(&out_dir, "land_contour_indices.zst", &cidx);

    let tidx = fs::read(&tidx_src).expect("read land_triangle_indices.gl");
    write_compressed(&out_dir, "land_triangle_indices.zst", &tidx);
}
