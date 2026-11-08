//! .quant project file: plain tab-separated text. Raw data is NOT stored (it is re-imported from the
//! recorded paths and verified by SHA-256); only targets, parameters and per-file analysis state are.
use crate::quant::Analyte;
use crate::sha256::Sha256;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Stored state of one target in one file.
#[derive(Clone, Debug, PartialEq)]
pub struct CellRec {
    pub analyte: usize, pub found: bool, pub manual: bool, pub start: f64, pub end: f64, pub include: bool,
    /// standard concentration of this target in this file (0 = not a standard); None in projects written by older versions.
    pub conc: Option<f64>,
}

/// Stored state of one file.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleRec {
    pub path: String, pub raw_sha256: String, pub kind: String, pub conc: f64, pub rt_shift: f64, pub show: bool,
    /// the original .raw file is stored inside the project file.
    pub embedded: bool,
    pub cells: Vec<CellRec>,
}

/// Everything a project stores except the raw data: targets, parameters and per-file state.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ProjectFile {
    pub app_version: String,
    pub user: String,
    pub params: Vec<(String, String)>,
    pub analytes: Vec<Analyte>,
    pub samples: Vec<SampleRec>,
}

/// First line of the text part of a project.
const MAGIC: &str = "LCMSQUANT-PROJECT";

/// Replaces tabs and line breaks, which separate fields and records, by spaces.
fn clean(s: &str) -> String { s.replace(['\t', '\n', '\r'], " ") }

impl ProjectFile {
    /// Serializes the project as tab-separated text.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut o = format!("{}\t1\n", MAGIC);
        o += &format!("V\t{}\t{}\n", clean(&self.app_version), clean(&self.user));
        for (k, v) in &self.params { o += &format!("P\t{}\t{}\n", clean(k), clean(v)); }
        for a in &self.analytes {
            let rt = a.rt.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into());
            let ions = a.ions.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(";");
            let prec = a.precursor.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into());
            o += &format!("A\t{}\t{}\t{}\t{}\n", clean(&a.name), rt, ions, prec);
        }
        for s in &self.samples {
            o += &format!("S\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n", clean(&s.path), s.raw_sha256, clean(&s.kind), s.conc, s.rt_shift, s.show, s.embedded);
            for c in &s.cells {
                o += &format!("C\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n", c.analyte, c.found, c.manual, c.start, c.end, c.include, c.conc.map(|v| v.to_string()).unwrap_or_else(|| "-".into()));
            }
        }
        o
    }

    /// Parses the text written by [`ProjectFile::to_text`].
    ///
    /// Unknown record types are ignored so that newer files can be read by older versions.
    ///
    /// # Errors
    ///
    /// Returns a message with the line number if the text is not a project or a field is invalid.
    pub fn parse(text: &str) -> Result<ProjectFile, String> {
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("");
        if !first.starts_with(MAGIC) { return Err("not an msQuant project file".into()); }
        let mut p = ProjectFile::default();
        for (n, l) in lines.enumerate() {
            let f: Vec<&str> = l.split('\t').collect();
            let err = |m: &str| format!("line {}: {}", n + 2, m);
            match f[0] {
                "V" if f.len() >= 3 => { p.app_version = f[1].into(); p.user = f[2].into(); }
                "P" if f.len() >= 3 => p.params.push((f[1].into(), f[2].into())),
                "A" if f.len() >= 4 => {
                    let rt = if f[2] == "n/a" { None } else { Some(f[2].parse().map_err(|_| err("bad RT"))?) };
                    let ions: Result<Vec<f64>, _> = f[3].split(';').filter(|s| !s.is_empty()).map(|s| s.parse::<f64>()).collect();
                    let ions = ions.map_err(|_| err("bad ion"))?;
                    if ions.is_empty() { return Err(err("target without ions")); }
                    let precursor = match f.get(4) { Some(s) if *s != "n/a" && !s.is_empty() => Some(s.parse().map_err(|_| err("bad precursor"))?), _ => None };
                    p.analytes.push(Analyte { name: f[1].into(), rt, ions, precursor });
                }
                "S" if f.len() >= 7 => p.samples.push(SampleRec {
                    path: f[1].into(), raw_sha256: f[2].into(), kind: f[3].into(),
                    conc: f[4].parse().map_err(|_| err("bad conc"))?, rt_shift: f[5].parse().map_err(|_| err("bad shift"))?,
                    show: f[6] == "true", embedded: f.get(7) == Some(&"true"), cells: vec![],
                }),
                "C" if f.len() >= 7 => {
                    let s = p.samples.last_mut().ok_or_else(|| err("cell before sample"))?;
                    s.cells.push(CellRec {
                        analyte: f[1].parse().map_err(|_| err("bad analyte index"))?, found: f[2] == "true", manual: f[3] == "true",
                        start: f[4].parse().map_err(|_| err("bad start"))?, end: f[5].parse().map_err(|_| err("bad end"))?,
                        include: f[6] == "true",
                        conc: f.get(7).and_then(|s| s.parse().ok()),
                    });
                }
                _ => {} // unknown / empty lines are ignored (forward compatible)
            }
        }
        Ok(p)
    }

    /// Value of the analysis parameter `key`, if the project stores it.
    #[must_use]
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Project container: [8 bytes "LCMSQNT2"][u64 text length][project text][for every embedded sample: u64 length + raw bytes]
// Older text-only projects (no embedded data) are still readable.
// ---------------------------------------------------------------------------------------------
/// First bytes of a project file that contains embedded raw files.
const MAGIC2: &[u8; 8] = b"LCMSQNT2";

/// Where the original .raw bytes of a loaded file can be found again (for saving into a project).
#[derive(Clone, Debug, PartialEq)]
pub enum RawSource {
    Disk(PathBuf),
    Embedded { project: PathBuf, offset: u64, len: u64 },
}

/// Data to copy into a project container.
pub enum BlobSource {
    File(PathBuf),
    Range { path: PathBuf, offset: u64, len: u64 },
}

/// Adds `what` to an I/O error message.
fn io<T>(r: std::io::Result<T>, what: &str) -> Result<T, String> { r.map_err(|e| format!("{}: {}", what, e)) }

/// Copies exactly `len` bytes from `src` to `dst` in chunks, optionally feeding them to `hash`.
fn copy_exact(src: &mut std::fs::File, len: u64, dst: &mut impl Write, mut hash: Option<&mut Sha256>) -> Result<(), String> {
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        io(src.read_exact(&mut buf[..n]), "reading source")?;
        io(dst.write_all(&buf[..n]), "writing")?;
        if let Some(h) = hash.as_mut() { h.update(&buf[..n]); }
        left -= n as u64;
    }
    Ok(())
}

/// Writes the project (streaming, large raw files never need to fit in memory). Returns (data offset, length) of each embedded blob.
/// The data is first written to a file in `tmp_dir` and moved to `path` at the end, so a source inside the same project file stays readable until then.
pub fn write_container(path: &Path, pf: &ProjectFile, blobs: &[Option<BlobSource>], tmp_dir: &Path) -> Result<Vec<Option<(u64, u64)>>, String> {
    if blobs.len() != pf.samples.len() { return Err("internal error: blob list does not match the samples".into()); }
    let mut pf2 = pf.clone();
    for (s, b) in pf2.samples.iter_mut().zip(blobs.iter()) { s.embedded = b.is_some(); }
    let text = pf2.to_text();
    io(std::fs::create_dir_all(tmp_dir), &tmp_dir.display().to_string())?;
    let file_name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project.quant".into());
    let tmp = tmp_dir.join(format!("{}.{}.tmp", file_name, std::process::id()));
    let mut out = BufWriter::new(io(std::fs::File::create(&tmp), &tmp.display().to_string())?);
    io(out.write_all(MAGIC2), "writing")?;
    io(out.write_all(&(text.len() as u64).to_le_bytes()), "writing")?;
    io(out.write_all(text.as_bytes()), "writing")?;
    let mut pos = 16 + text.len() as u64;
    let mut offs: Vec<Option<(u64, u64)>> = vec![None; blobs.len()];
    for (i, b) in blobs.iter().enumerate() {
        let Some(src) = b else { continue };
        let (mut f, off, len) = match src {
            BlobSource::File(p) => {
                let f = io(std::fs::File::open(p), &p.display().to_string())?;
                let len = io(f.metadata(), "metadata")?.len();
                (f, 0, len)
            }
            BlobSource::Range { path, offset, len } => (io(std::fs::File::open(path), &path.display().to_string())?, *offset, *len),
        };
        io(f.seek(SeekFrom::Start(off)), "seek")?;
        io(out.write_all(&len.to_le_bytes()), "writing")?;
        copy_exact(&mut f, len, &mut out, None)?;
        offs[i] = Some((pos + 8, len));
        pos += 8 + len;
    }
    io(out.flush(), "writing")?;
    drop(out);
    move_file(&tmp, path)?;
    Ok(offs)
}

/// Moves a file; falls back to copy and delete when source and target are on different volumes.
fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() { return Ok(()); }
    io(std::fs::copy(from, to), "writing the project file")?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

/// Reads the project text and locates the embedded blobs (without loading them).
pub fn read_container(path: &Path) -> Result<(ProjectFile, Vec<Option<(u64, u64)>>), String> {
    let mut f = io(std::fs::File::open(path), &path.display().to_string())?;
    let file_len = io(f.metadata(), "metadata")?.len();
    let mut head = [0u8; 8];
    let n = io(f.read(&mut head), "reading")?;
    if n == 8 && &head == MAGIC2 {
        let mut l = [0u8; 8];
        io(f.read_exact(&mut l), "reading")?;
        let tl = u64::from_le_bytes(l);
        if 16 + tl > file_len { return Err("project file is truncated".into()); }
        let mut tb = vec![0u8; tl as usize];
        io(f.read_exact(&mut tb), "reading")?;
        let pf = ProjectFile::parse(&String::from_utf8_lossy(&tb))?;
        let mut pos = 16 + tl;
        let mut offs = Vec::with_capacity(pf.samples.len());
        for s in &pf.samples {
            if !s.embedded { offs.push(None); continue; }
            if pos + 8 > file_len { return Err("project file is truncated".into()); }
            io(f.seek(SeekFrom::Start(pos)), "seek")?;
            let mut lb = [0u8; 8];
            io(f.read_exact(&mut lb), "reading")?;
            let len = u64::from_le_bytes(lb);
            if pos + 8 + len > file_len { return Err(format!("project file is truncated inside the embedded file {}", s.path)); }
            offs.push(Some((pos + 8, len)));
            pos += 8 + len;
        }
        Ok((pf, offs))
    } else {
        io(f.seek(SeekFrom::Start(0)), "seek")?;
        let mut t = String::new();
        io(f.read_to_string(&mut t), "reading")?;
        let pf = ProjectFile::parse(&t)?;
        let n = pf.samples.len();
        Ok((pf, vec![None; n]))
    }
}

/// Copies an embedded blob to `dest` (streaming) and returns its SHA-256.
pub fn extract_blob(project: &Path, offset: u64, len: u64, dest: &Path) -> Result<String, String> {
    let mut f = io(std::fs::File::open(project), &project.display().to_string())?;
    io(f.seek(SeekFrom::Start(offset)), "seek")?;
    let mut out = BufWriter::new(io(std::fs::File::create(dest), &dest.display().to_string())?);
    let mut h = Sha256::new();
    copy_exact(&mut f, len, &mut out, Some(&mut h))?;
    io(out.flush(), "writing")?;
    Ok(h.finish_hex())
}

/// Reads an embedded blob completely into memory (used for embedded mzML files, which need no converter).
pub fn read_blob(project: &Path, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    let mut f = io(std::fs::File::open(project), &project.display().to_string())?;
    io(f.seek(SeekFrom::Start(offset)), "seek")?;
    let mut v = vec![0u8; len as usize];
    io(f.read_exact(&mut v), "reading")?;
    Ok(v)
}
