//! Minimal in-memory mzML reader: MS1 and MS2 spectra (m/z, intensity, RT, scan number).
use crate::{inflate::zlib_decompress, sha256::sha256_hex};

/// A centroided or profile spectrum.
pub struct Spectrum {
    /// Retention time in minutes.
    pub rt: f64,
    /// Scan number from the spectrum id (the position in the file if the id has none).
    pub scan: u32,
    /// Peak m/z values in ascending order.
    pub mz: Vec<f64>,
    /// Peak intensities, parallel to `mz`.
    pub inten: Vec<f32>,
    /// m/z of the fragmented precursor ion (MS2 only): selected ion m/z, else the isolation window target; None if not recorded.
    pub precursor: Option<f64>,
}

/// The MS1 and MS2 content of one mzML file, held in memory.
pub struct MzData {
    /// File name, for display.
    pub name: String,
    /// SHA-256 of the mzML text as it was parsed.
    pub sha256: String,
    /// MS2 spectra, sorted by retention time.
    pub spectra: Vec<Spectrum>,
    /// MS1 spectra, sorted by retention time (needed for quantification on the MS1 precursor signal)
    pub ms1: Vec<Spectrum>,
    /// Number of spectra in the file, of any MS level.
    pub n_total: usize,
    /// (rt, value) total-ion and base-peak chromatograms of MS1 and MS2 scans, same order as `ms1` / `spectra`
    pub tic1: Vec<[f64; 2]>,
    pub bpc1: Vec<[f64; 2]>,
    pub tic2: Vec<[f64; 2]>,
    pub bpc2: Vec<[f64; 2]>,
}

/// Value of the XML attribute `name` in the tag text `tag`, if present.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{}=\"", name);
    let mut from = 0;
    while let Some(p) = tag[from..].find(&pat) {
        let idx = from + p;
        if idx == 0 || tag.as_bytes()[idx - 1].is_ascii_whitespace() {
            let s = idx + pat.len();
            let e = tag[s..].find('"')?;
            return Some(&tag[s..s + e]);
        }
        from = idx + pat.len();
    }
    None
}

/// Decodes base64 text, ignoring white space; decoding stops at the first `=`.
fn b64(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut n) = (0u32, 0u32);
    for c in s.bytes() {
        let v: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | v;
        n += 6;
        if n >= 8 { n -= 8; out.push((acc >> n) as u8); acc &= (1 << n) - 1; }
    }
    out
}

/// "controllerType=0 controllerNumber=1 scan=1234" -> 1234.
fn scan_no(id: &str) -> Option<u32> {
    let p = id.rfind("scan=")?;
    let d: String = id[p + 5..].chars().take_while(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}

/// The spectrum that is currently being read.
struct Cur { level: u32, rt: f64, scan: u32, mz: Vec<f64>, inten: Vec<f32>, prec: Option<f64>, iso: Option<f64> }

/// Completes a spectrum: drops it if its arrays are missing or inconsistent and sorts the peaks by m/z.
fn finish(mut c: Cur) -> Option<Spectrum> {
    if c.mz.is_empty() || c.mz.len() != c.inten.len() { return None; }
    if !c.mz.windows(2).all(|w| w[0] <= w[1]) {
        let mut p: Vec<(f64, f32)> = c.mz.iter().cloned().zip(c.inten.iter().cloned()).collect();
        p.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        c.mz = p.iter().map(|x| x.0).collect();
        c.inten = p.iter().map(|x| x.1).collect();
    }
    Some(Spectrum { rt: c.rt, scan: c.scan, mz: c.mz, inten: c.inten, precursor: c.prec.or(c.iso) })
}

/// Parses mzML text that is already in memory.
///
/// Spectra of MS level 1 and 2 are kept; everything else (chromatograms, other levels) is ignored.
/// Numpress-compressed arrays are not supported and give an error. `name` is only used for
/// display.
///
/// # Errors
///
/// Returns a message if an array cannot be decoded or if the file contains no MS1 or MS2 spectra.
pub fn parse_bytes(name: &str, bytes: &[u8]) -> Result<MzData, String> {
    let sha = sha256_hex(bytes);
    let text = String::from_utf8_lossy(bytes);
    let t: &str = &text;
    let bt = t.as_bytes();
    let name = name.to_string();

    let mut spectra: Vec<Spectrum> = Vec::new();
    let mut ms1: Vec<Spectrum> = Vec::new();
    let mut n_total = 0usize;
    let mut cur: Option<Cur> = None;
    let (mut in_bda, mut in_bin) = (false, false);
    let (mut is64, mut zlib, mut kind, mut unsupported) = (true, false, 0u8, false);
    let mut bin = String::new();
    let mut i = 0;
    while i < bt.len() {
        if bt[i] == b'<' {
            let end = match t[i..].find('>') { Some(e) => i + e, None => break };
            let tag = &t[i + 1..end];
            i = end + 1;
            if tag.starts_with('?') || tag.starts_with('!') { continue; }
            if let Some(n) = tag.strip_prefix('/') {
                match n.trim() {
                    "binary" => {
                        in_bin = false;
                        if let Some(c) = cur.as_mut() {
                            if in_bda && (c.level == 1 || c.level == 2) && kind != 0 {
                                if unsupported { return Err("numpress-compressed data is not supported".into()); }
                                let mut raw = b64(&bin);
                                if zlib { raw = zlib_decompress(&raw)?; }
                                let vals: Vec<f64> = if is64 {
                                    raw.chunks_exact(8).map(|b| f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])).collect()
                                } else {
                                    raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64).collect()
                                };
                                if kind == 1 { c.mz = vals; } else { c.inten = vals.into_iter().map(|v| v as f32).collect(); }
                            }
                        }
                    }
                    "binaryDataArray" => in_bda = false,
                    "spectrum" => {
                        if let Some(c) = cur.take() {
                            let level = c.level;
                            if level == 1 || level == 2 {
                                if let Some(sp) = finish(c) { if level == 1 { ms1.push(sp); } else { spectra.push(sp); } }
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let selfc = tag.ends_with('/');
            let nm = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
            match nm {
                "spectrum" if !selfc => {
                    n_total += 1;
                    let scan = attr(tag, "id").and_then(scan_no).unwrap_or(n_total as u32);
                    cur = Some(Cur { level: 0, rt: 0.0, scan, mz: vec![], inten: vec![], prec: None, iso: None });
                }
                "binaryDataArray" if !selfc => { in_bda = true; is64 = true; zlib = false; kind = 0; unsupported = false; }
                "binary" if !selfc => { in_bin = true; bin.clear(); }
                "cvParam" => {
                    if let (Some(c), Some(acc)) = (cur.as_mut(), attr(tag, "accession")) {
                        let val = attr(tag, "value").unwrap_or("");
                        if !in_bda {
                            if acc == "MS:1000511" { c.level = val.parse().unwrap_or(0); }
                            else if acc == "MS:1000744" { c.prec = val.parse().ok(); }
                            else if acc == "MS:1000827" { c.iso = val.parse().ok(); }
                            else if acc == "MS:1000016" {
                                let v: f64 = val.parse().unwrap_or(0.0);
                                let sec = attr(tag, "unitAccession") == Some("UO:0000010") || attr(tag, "unitName") == Some("second");
                                c.rt = if sec { v / 60.0 } else { v };
                            }
                        } else {
                            match acc {
                                "MS:1000521" => is64 = false,
                                "MS:1000523" => is64 = true,
                                "MS:1000574" => zlib = true,
                                "MS:1000576" => zlib = false,
                                "MS:1000514" => kind = 1,
                                "MS:1000515" => kind = 2,
                                "MS:1002312" | "MS:1002313" | "MS:1002314" | "MS:1002746" | "MS:1002747" | "MS:1002748" => unsupported = true,
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        } else {
            let e = t[i..].find('<').map(|p| i + p).unwrap_or(bt.len());
            if in_bin { bin.push_str(t[i..e].trim()); }
            i = e;
        }
    }
    if spectra.is_empty() && ms1.is_empty() {
        return Err(format!("{}: no MS1/MS2 spectra found (is this centroid/profile LC-MS data?)", name));
    }
    spectra.sort_by(|a, b| a.rt.partial_cmp(&b.rt).unwrap_or(std::cmp::Ordering::Equal));
    ms1.sort_by(|a, b| a.rt.partial_cmp(&b.rt).unwrap_or(std::cmp::Ordering::Equal));
    let tic = |s: &Spectrum| [s.rt, s.inten.iter().map(|&v| v as f64).sum::<f64>()];
    let bpc = |s: &Spectrum| [s.rt, s.inten.iter().cloned().fold(0.0f32, f32::max) as f64];
    let (tic1, bpc1) = (ms1.iter().map(tic).collect(), ms1.iter().map(bpc).collect());
    let (tic2, bpc2) = (spectra.iter().map(tic).collect(), spectra.iter().map(bpc).collect());
    Ok(MzData { name, sha256: sha, spectra, ms1, n_total, tic1, bpc1, tic2, bpc2 })
}
