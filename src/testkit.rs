//! Synthetic mzML generator used by the test suites (not used by the application itself).
//! One run = 1000 MS1 + 1000 MS2 scans over 10 min with a Gaussian chromatographic peak at 5.0 min (+offset).
#![doc(hidden)]

/// m/z of the first fragment ion of the synthetic target.
pub const FRAG1: f64 = 150.0;
/// m/z of the second fragment ion; its intensity is half of the first.
pub const FRAG2: f64 = 250.0; // always 0.5 x FRAG1
/// Retention time of the synthetic peak in minutes.
pub const PEAK_RT: f64 = 5.0;
/// precursor m/z written into every synthetic MS2 scan (isolation window target is deliberately different)
pub const PRECURSOR: f64 = 286.1438;
/// Standard deviation of the synthetic peak in minutes.
pub const PEAK_SIGMA: f64 = 0.05;

/// Analytical peak area (intensity·seconds) of FRAG1 for a given scale.
pub fn expected_area_seconds(scale: f64) -> f64 { scale * 1000.0 * PEAK_SIGMA * (2.0 * std::f64::consts::PI).sqrt() * 60.0 }

/// Encodes bytes as base64.
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18) as usize & 63] as char);
        o.push(T[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    o
}

/// Adler-32 checksum of `d`.
fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in d { a = (a + x as u32) % 65521; b = (b + a) % 65521; }
    (b << 16) | a
}

/// Valid zlib stream made of stored (uncompressed) deflate blocks.
pub fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut o = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(65535).collect() };
    for (i, c) in chunks.iter().enumerate() {
        o.push(if i + 1 == chunks.len() { 1 } else { 0 });
        o.extend_from_slice(&(c.len() as u16).to_le_bytes());
        o.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        o.extend_from_slice(c);
    }
    o.extend_from_slice(&adler32(data).to_be_bytes());
    o
}

/// Unit-height Gaussian with centre `c` and standard deviation `s`, evaluated at `t`.
fn gauss(t: f64, c: f64, s: f64) -> f64 { (-0.5 * ((t - c) / s).powi(2)).exp() }

/// XML of one spectrum with a 64-bit m/z array and a zlib-compressed 32-bit intensity array.
fn spectrum(idx: usize, level: u8, rt_min: f64, secs: bool, mz: &[f64], inten: &[f32], prec: Option<f64>) -> String {
    let p = prec.map(|v| format!("<precursorList count=\"1\"><precursor><isolationWindow><cvParam cvRef=\"MS\" accession=\"MS:1000827\" name=\"isolation window target m/z\" value=\"286\"/></isolationWindow><selectedIonList count=\"1\"><selectedIon><cvParam cvRef=\"MS\" accession=\"MS:1000744\" name=\"selected ion m/z\" value=\"{}\"/></selectedIon></selectedIonList></precursor></precursorList>\n", v)).unwrap_or_default();
    let mzb: Vec<u8> = mz.iter().flat_map(|v| v.to_le_bytes()).collect();
    let inb: Vec<u8> = inten.iter().flat_map(|v| v.to_le_bytes()).collect();
    let time = if secs {
        format!("<cvParam cvRef=\"MS\" accession=\"MS:1000016\" name=\"scan start time\" value=\"{}\" unitCvRef=\"UO\" unitAccession=\"UO:0000010\" unitName=\"second\"/>", rt_min * 60.0)
    } else {
        format!("<cvParam cvRef=\"MS\" accession=\"MS:1000016\" name=\"scan start time\" value=\"{}\" unitCvRef=\"UO\" unitAccession=\"UO:0000031\" unitName=\"minute\"/>", rt_min)
    };
    format!(
        "<spectrum index=\"{i}\" id=\"scan={i}\" defaultArrayLength=\"{n}\">\n<cvParam cvRef=\"MS\" accession=\"MS:1000511\" name=\"ms level\" value=\"{l}\"/>\n<scanList count=\"1\"><scan>{t}</scan></scanList>\n{p}<binaryDataArrayList count=\"2\">\n<binaryDataArray encodedLength=\"0\"><cvParam cvRef=\"MS\" accession=\"MS:1000523\" name=\"64-bit float\"/><cvParam cvRef=\"MS\" accession=\"MS:1000576\" name=\"no compression\"/><cvParam cvRef=\"MS\" accession=\"MS:1000514\" name=\"m/z array\"/><binary>{m}</binary></binaryDataArray>\n<binaryDataArray encodedLength=\"0\"><cvParam cvRef=\"MS\" accession=\"MS:1000521\" name=\"32-bit float\"/><cvParam cvRef=\"MS\" accession=\"MS:1000574\" name=\"zlib compression\"/><cvParam cvRef=\"MS\" accession=\"MS:1000515\" name=\"intensity array\"/><binary>{b}</binary></binaryDataArray>\n</binaryDataArrayList>\n</spectrum>\n",
        i = idx, n = mz.len(), l = level, t = time, p = p, m = b64(&mzb), b = b64(&zlib_stored(&inb))
    )
}

/// `offset`: shifts the whole chromatogram in time (min). `scale`: multiplies the peak intensity.
pub fn synthetic_mzml(offset: f64, scale: f64) -> Vec<u8> {
    let mut o = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<mzML><run><spectrumList count=\"2000\">\n");
    let mut idx = 0;
    for k in 0..1000 {
        let rt = k as f64 * 0.01;
        let ms1 = 1000.0 + 50000.0 * gauss(rt, PEAK_RT + offset, 0.1) + 30000.0 * gauss(rt, 8.0 + offset, 0.1);
        o += &spectrum(idx, 1, rt, true, &[500.0], &[ms1 as f32], None);
        idx += 1;
        let rt2 = rt + 0.005;
        let f1 = scale * 1000.0 * gauss(rt2, PEAK_RT + offset, PEAK_SIGMA);
        o += &spectrum(idx, 2, rt2, false, &[FRAG1, FRAG2, 400.0], &[f1 as f32, (0.5 * f1) as f32, 10.0], Some(PRECURSOR));
        idx += 1;
    }
    o += "</spectrumList></run></mzML>\n";
    o.into_bytes()
}
