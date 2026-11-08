//! Minimal .ico reader (PNG-in-ICO and 24/32-bit BMP entries) -> RGBA, so no image crate is needed.
use crate::inflate::zlib_decompress;

/// Reads a little-endian `u16` at byte offset `o`.
fn u16le(b: &[u8], o: usize) -> Option<u16> { Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?)) }
/// Reads a little-endian `u32` at byte offset `o`.
fn u32le(b: &[u8], o: usize) -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?)) }
/// Reads a big-endian `u32` at byte offset `o`.
fn u32be(b: &[u8], o: usize) -> Option<u32> { Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?)) }

/// Returns (rgba, width, height) of the largest image in the .ico.
pub fn decode_ico(b: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if b.len() < 22 || u16le(b, 0)? != 0 || u16le(b, 2)? != 1 { return None; }
    let n = u16le(b, 4)? as usize;
    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..n {
        let e = 6 + i * 16;
        let w = if *b.get(e)? == 0 { 256 } else { b[e] as u32 };
        let h = if *b.get(e + 1)? == 0 { 256 } else { b[e + 1] as u32 };
        let size = u32le(b, e + 8)? as usize;
        let off = u32le(b, e + 12)? as usize;
        if off + size <= b.len() && best.map(|x| w * h > x.0).unwrap_or(true) { best = Some((w * h, off, size)); }
    }
    let (_, off, size) = best?;
    let d = &b[off..off + size];
    if d.len() > 8 && d[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] { decode_png(d) } else { decode_dib(d) }
}

/// Decodes a 24 or 32-bit BMP image as stored in an icon entry into RGBA.
fn decode_dib(d: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let hs = u32le(d, 0)? as usize;
    let w = u32le(d, 4)? as i32;
    let h2 = u32le(d, 8)? as i32;
    let bpp = u16le(d, 14)? as usize;
    let comp = u32le(d, 16)?;
    if (comp != 0 && comp != 3) || (bpp != 32 && bpp != 24) || w <= 0 { return None; }
    let (w, h) = (w as usize, (h2.unsigned_abs() / 2) as usize);
    let stride = ((w * bpp + 31) / 32) * 4;
    let skip = if comp == 3 && hs == 40 { 12 } else { 0 };
    let px = d.get(hs + skip..)?;
    let bytes = bpp / 8;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let src = h - 1 - y;
        for x in 0..w {
            let o = src * stride + x * bytes;
            let p = px.get(o..o + bytes)?;
            let a = if bpp == 32 { p[3] } else { 255 };
            let i = (y * w + x) * 4;
            out[i..i + 4].copy_from_slice(&[p[2], p[1], p[0], a]);
        }
    }
    Some((out, w as u32, h as u32))
}

/// The Paeth predictor of the PNG filter type 4.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (a as i32, b as i32, c as i32);
    let p = ia + ib - ic;
    let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
}

/// 8-bit, non-interlaced, RGB or RGBA PNG.
pub fn decode_png(d: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut pos = 8;
    let (mut w, mut h, mut depth, mut ct, mut il) = (0usize, 0usize, 0u8, 0u8, 0u8);
    let mut idat: Vec<u8> = Vec::new();
    while pos + 8 <= d.len() {
        let len = u32be(d, pos)? as usize;
        let ty = d.get(pos + 4..pos + 8)?;
        let body = d.get(pos + 8..pos + 8 + len)?;
        match ty {
            b"IHDR" => { w = u32be(body, 0)? as usize; h = u32be(body, 4)? as usize; depth = body[8]; ct = body[9]; il = body[12]; }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos += 12 + len;
    }
    if depth != 8 || il != 0 || (ct != 6 && ct != 2) || w == 0 || h == 0 { return None; }
    let bpp = if ct == 6 { 4 } else { 3 };
    let raw = zlib_decompress(&idat).ok()?;
    let stride = w * bpp;
    if raw.len() < (stride + 1) * h { return None; }
    let mut img = vec![0u8; stride * h];
    for y in 0..h {
        let ft = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        for x in 0..stride {
            let a = if x >= bpp { img[y * stride + x - bpp] } else { 0 };
            let b = if y > 0 { img[(y - 1) * stride + x] } else { 0 };
            let c = if x >= bpp && y > 0 { img[(y - 1) * stride + x - bpp] } else { 0 };
            img[y * stride + x] = match ft {
                0 => line[x],
                1 => line[x].wrapping_add(a),
                2 => line[x].wrapping_add(b),
                3 => line[x].wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => line[x].wrapping_add(paeth(a, b, c)),
                _ => return None,
            };
        }
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for px in img.chunks_exact(bpp) {
        out.extend_from_slice(&px[..3]);
        out.push(if bpp == 4 { px[3] } else { 255 });
    }
    Some((out, w as u32, h as u32))
}
