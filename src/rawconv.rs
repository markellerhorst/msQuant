//! Thermo .raw -> in-memory mzML via ThermoRawFileParser (CompOmics, Apache-2.0), run as an external program.
//! The converter writes mzML to its standard output (-s); we capture it in memory. Nothing is written to disk.
use crate::sha256::Sha256;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// ThermoRawFileParser: user-given path, $THERMO_RAW_FILE_PARSER, any tools\ThermoRawFileParser* folder
/// next to the exe or in the working directory, then PATH.
pub fn find_trfp(user: &str) -> Option<PathBuf> {
    let u = user.trim().trim_matches('"');
    if !u.is_empty() { let p = PathBuf::from(u); return if p.is_file() { Some(p) } else { None }; }
    if let Some(v) = std::env::var_os("THERMO_RAW_FILE_PARSER") { let p = PathBuf::from(v); if p.is_file() { return Some(p); } }
    let mut roots: Vec<PathBuf> = vec![];
    if let Ok(e) = std::env::current_exe() { if let Some(d) = e.parent() { roots.push(d.join("tools")); roots.push(d.to_path_buf()); } }
    if let Ok(c) = std::env::current_dir() { roots.push(c.join("tools")); }
    let mut dirs: Vec<PathBuf> = vec![];
    for root in roots {
        dirs.push(root.clone());
        if let Ok(rd) = std::fs::read_dir(&root) {
            let mut subs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path())
                .filter(|p| p.is_dir() && p.file_name().map(|n| n.to_string_lossy().to_lowercase().starts_with("thermorawfileparser")).unwrap_or(false))
                .collect();
            subs.sort();
            subs.reverse();
            dirs.extend(subs);
        }
    }
    if let Some(path) = std::env::var_os("PATH") { dirs.extend(std::env::split_paths(&path)); }
    for d in dirs {
        for n in ["ThermoRawFileParser.exe", "ThermoRawFileParser.dll", "ThermoRawFileParser"] {
            let c = d.join(n);
            if c.is_file() { return Some(c); }
        }
    }
    None
}

/// Streaming SHA-256 of a file (equals `Get-FileHash -Algorithm SHA256`).
pub fn hash_file(p: &Path) -> Result<String, String> {
    let mut h = Sha256::new();
    let mut fh = std::fs::File::open(p).map_err(|e| format!("{}: {}", p.display(), e))?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = fh.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 { return Ok(h.finish_hex()); }
        h.update(&buf[..n]);
    }
}

/// Result of a conversion: the mzML text and a description of how it was made, for the audit trail.
pub struct Converted { pub mzml: Vec<u8>, pub detail: String }

/// Whether `exe` is a .NET library that has to be started through `dotnet`.
fn is_dll(exe: &Path) -> bool { exe.extension().map(|x| x.eq_ignore_ascii_case("dll")).unwrap_or(false) }

/// dotnet.exe from the standard Windows install folder if present, else whatever is on PATH.
fn dotnet() -> PathBuf {
    if cfg!(windows) {
        for v in ["ProgramFiles", "ProgramW6432"] {
            if let Some(p) = std::env::var_os(v) {
                let c = PathBuf::from(p).join("dotnet").join("dotnet.exe");
                if c.is_file() { return c; }
            }
        }
    }
    PathBuf::from("dotnet")
}

/// Command that starts the converter at `exe`, through `dotnet` for a `.dll`.
fn base_cmd(exe: &Path) -> Command {
    if is_dll(exe) {
        let mut c = Command::new(dotnet());
        c.arg(exe);
        c
    } else {
        Command::new(exe)
    }
}

/// Makes a converter process work in, and use for its own temporary files, the msQuant temporary folder instead of the system one.
fn confine_to_temp(cmd: &mut Command) {
    let work = crate::paths::temp_dir();
    cmd.current_dir(&work)
        .env("TMP", &work)
        .env("TEMP", &work)
        .env("TMPDIR", &work)
        .env("DOTNET_BUNDLE_EXTRACT_BASE_DIR", &work);
}

/// Runs: ThermoRawFileParser -i=<raw> -s -f=1 -L=1,2   (stdout, plain mzML, MS1+MS2 scans)
pub fn convert_in_memory(exe: &Path, raw: &Path) -> Result<Converted, String> {
    let mut cmd = base_cmd(exe);
    cmd.arg(format!("-i={}", raw.display())).args(["-s", "-f=1", "-L=1,2"]);
    confine_to_temp(&mut cmd);
    let o = cmd.output().map_err(|e| {
        if is_dll(exe) {
            format!("could not start 'dotnet' to run {} ({}). Install the .NET 8 runtime (https://dotnet.microsoft.com/en-us/download/dotnet/8.0/runtime) and restart this app, or point the path box at a self-contained ThermoRawFileParser.exe.", exe.display(), e)
        } else {
            format!("could not start {} ({}). If this is a framework-dependent build it needs the .NET 8 runtime.", exe.display(), e)
        }
    })?;
    if !o.status.success() {
        let err = String::from_utf8_lossy(&o.stderr);
        let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).rev().take(5).collect();
        return Err(format!("ThermoRawFileParser failed: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" | ")));
    }
    let head = String::from_utf8_lossy(&o.stdout[..o.stdout.len().min(300)]).to_string();
    if !head.contains('<') { return Err("unexpected converter output (not mzML); check `ThermoRawFileParser -h`".into()); }
    let version = base_cmd(exe).arg("-v").output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into());
    let exe_hash = hash_file(exe).unwrap_or_else(|_| "n/a".into());
    Ok(Converted { mzml: o.stdout, detail: format!("converter_sha256={} converter_version=[{}] cmd=[{:?}]", exe_hash, version, cmd) })
}

/// Output of `ThermoRawFileParser -v` (used by the preferences "Test" button).
pub fn converter_version(exe: &Path) -> Result<String, String> {
    let mut cmd = base_cmd(exe);
    cmd.arg("-v");
    confine_to_temp(&mut cmd);
    let o = cmd.output().map_err(|e| format!("could not start {}: {}", exe.display(), e))?;
    let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if !out.is_empty() { return Ok(out); }
    let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
    if err.is_empty() { Err("no version output".into()) } else { Ok(err) }
}
