//! Minimal zlib/DEFLATE decoder (RFC 1950/1951), after zlib's "puff".
struct Bits<'a> { d: &'a [u8], pos: usize, bit: u32, cnt: u32 }
impl<'a> Bits<'a> {
    /// Reads `n` bits, least significant bit first.
    fn get(&mut self, n: u32) -> Result<u32, String> {
        while self.cnt < n {
            if self.pos >= self.d.len() { return Err("unexpected end of zlib data".into()); }
            self.bit |= (self.d[self.pos] as u32) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
        let v = self.bit & ((1u32 << n) - 1);
        self.bit >>= n;
        self.cnt -= n;
        Ok(v)
    }
}
/// Canonical Huffman decoding table.
struct Huff { count: [u16; 16], symbol: Vec<u16> }
impl Huff {
    /// Builds the table from the code lengths of the symbols.
    fn new(lens: &[u8]) -> Huff {
        let mut count = [0u16; 16];
        for &l in lens { count[l as usize] += 1; }
        let mut offs = [0u16; 16];
        for i in 1..15 { offs[i + 1] = offs[i] + count[i]; }
        let mut symbol = vec![0u16; lens.len()];
        for (s, &l) in lens.iter().enumerate() {
            if l != 0 { symbol[offs[l as usize] as usize] = s as u16; offs[l as usize] += 1; }
        }
        Huff { count, symbol }
    }
    /// Decodes one symbol.
    fn decode(&self, b: &mut Bits) -> Result<usize, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.get(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first { return Ok(self.symbol[(index + (code - first)) as usize] as usize); }
            index += count; first += count; first <<= 1; code <<= 1;
        }
        Err("bad huffman code".into())
    }
}
/// Smallest match length of each length code.
const LBASE: [u16; 29] = [3,4,5,6,7,8,9,10,11,13,15,17,19,23,27,31,35,43,51,59,67,83,99,115,131,163,195,227,258];
/// Number of extra bits of each length code.
const LEXT: [u8; 29] = [0,0,0,0,0,0,0,0,1,1,1,1,2,2,2,2,3,3,3,3,4,4,4,4,5,5,5,5,0];
/// Smallest match distance of each distance code.
const DBASE: [u16; 30] = [1,2,3,4,5,7,9,13,17,25,33,49,65,97,129,193,257,385,513,769,1025,1537,2049,3073,4097,6145,8193,12289,16385,24577];
/// Number of extra bits of each distance code.
const DEXT: [u8; 30] = [0,0,0,0,1,1,2,2,3,3,4,4,5,5,6,6,7,7,8,8,9,9,10,10,11,11,12,12,13,13];

/// Decompresses a zlib stream (RFC 1950). The Adler-32 checksum is not verified.
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 6 { return Err("zlib stream too short".into()); }
    let mut b = Bits { d: &data[2..], pos: 0, bit: 0, cnt: 0 };
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 4);
    loop {
        let last = b.get(1)?;
        match b.get(2)? {
            0 => {
                b.bit = 0; b.cnt = 0;
                if b.pos + 4 > b.d.len() { return Err("bad stored block".into()); }
                let len = u16::from_le_bytes([b.d[b.pos], b.d[b.pos + 1]]) as usize;
                b.pos += 4;
                if b.pos + len > b.d.len() { return Err("bad stored block".into()); }
                out.extend_from_slice(&b.d[b.pos..b.pos + len]);
                b.pos += len;
            }
            1 => {
                let mut l = [0u8; 288];
                for i in 0..144 { l[i] = 8; }
                for i in 144..256 { l[i] = 9; }
                for i in 256..280 { l[i] = 7; }
                for i in 280..288 { l[i] = 8; }
                codes(&mut b, &mut out, &Huff::new(&l), &Huff::new(&[5u8; 30]))?;
            }
            2 => {
                let nlen = b.get(5)? as usize + 257;
                let ndist = b.get(5)? as usize + 1;
                let ncode = b.get(4)? as usize + 4;
                if nlen > 286 || ndist > 30 { return Err("bad dynamic header".into()); }
                const ORDER: [usize; 19] = [16,17,18,0,8,7,9,6,10,5,11,4,12,3,13,2,14,1,15];
                let mut cl = [0u8; 19];
                for i in 0..ncode { cl[ORDER[i]] = b.get(3)? as u8; }
                let ch = Huff::new(&cl);
                let mut lens = vec![0u8; nlen + ndist];
                let mut i = 0;
                while i < nlen + ndist {
                    let s = ch.decode(&mut b)?;
                    if s < 16 { lens[i] = s as u8; i += 1; }
                    else {
                        let (v, rep) = match s {
                            16 => { if i == 0 { return Err("bad repeat".into()); } (lens[i - 1], 3 + b.get(2)? as usize) }
                            17 => (0, 3 + b.get(3)? as usize),
                            _ => (0, 11 + b.get(7)? as usize),
                        };
                        if i + rep > nlen + ndist { return Err("bad repeat length".into()); }
                        for _ in 0..rep { lens[i] = v; i += 1; }
                    }
                }
                codes(&mut b, &mut out, &Huff::new(&lens[..nlen]), &Huff::new(&lens[nlen..]))?;
            }
            _ => return Err("bad deflate block type".into()),
        }
        if last == 1 { break; }
    }
    Ok(out)
}

/// Decodes the literals and length/distance pairs of one block until its end-of-block symbol.
fn codes(b: &mut Bits, out: &mut Vec<u8>, lit: &Huff, dist: &Huff) -> Result<(), String> {
    loop {
        let s = lit.decode(b)?;
        if s < 256 { out.push(s as u8); }
        else if s == 256 { return Ok(()); }
        else {
            let s = s - 257;
            if s >= 29 { return Err("bad length symbol".into()); }
            let len = LBASE[s] as usize + b.get(LEXT[s] as u32)? as usize;
            let ds = dist.decode(b)?;
            if ds >= 30 { return Err("bad distance symbol".into()); }
            let d = DBASE[ds] as usize + b.get(DEXT[ds] as u32)? as usize;
            if d > out.len() { return Err("distance too far".into()); }
            for _ in 0..len { let v = out[out.len() - d]; out.push(v); }
        }
    }
}
