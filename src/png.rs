//! Minimal PNG writer: 8-bit RGB, uncompressed ("stored") deflate blocks. Valid everywhere, no dependencies.

/// Table for the CRC-32 checksum used by PNG chunks.
fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for n in 0..256u32 {
        let mut c = n;
        for _ in 0..8 { c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 }; }
        t[n as usize] = c;
    }
    t
}

/// CRC-32 of `data` (IEEE polynomial, as used by PNG and zip).
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    let mut c = 0xFFFF_FFFFu32;
    for &b in data { c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8); }
    c ^ 0xFFFF_FFFF
}

/// Adler-32 checksum of `d`, the trailer of a zlib stream.
fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in d { a = (a + x as u32) % 65521; b = (b + a) % 65521; }
    (b << 16) | a
}

/// Appends a PNG chunk (length, type, body, CRC) to `out`.
fn chunk(out: &mut Vec<u8>, ty: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut c = ty.to_vec();
    c.extend_from_slice(body);
    out.extend_from_slice(&c);
    out.extend_from_slice(&crc32(&c).to_be_bytes());
}

/// `rgb` holds width*height*3 bytes, row by row.
pub fn encode_rgb(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 || rgb.len() != (w as usize) * (h as usize) * 3 { return Err("bad image size".into()); }
    let stride = w as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for y in 0..h as usize { raw.push(0); raw.extend_from_slice(&rgb[y * stride..(y + 1) * stride]); }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(if i + 1 == blocks.len() { 1 } else { 0 });
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}
