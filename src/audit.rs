//! Append-only, hash-chained audit trail (tamper-evident). Line: ts|user|action|detail|hash
use crate::sha256::sha256_hex;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current UTC time as ISO 8601 text, for example `2026-01-31T12:00:00Z`.
#[must_use]
pub fn iso_now() -> String {
    let s = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Append-only, hash-chained audit trail.
///
/// Every line is `time|user|action|detail|hash`. The hash covers the hash of the previous line and
/// the text of the line, so a later change of any line is detected by [`verify`].
pub struct Audit { pub path: String, pub user: String, last: String, pub recent: Vec<String> }

impl Audit {
    /// Opens the trail in `path`, continuing its hash chain; the file is created with the first entry.
    pub fn new(path: &str) -> Audit {
        let user = "N/A".to_string();
        let mut recent = Vec::new();
        let mut last = "GENESIS".to_string();
        if let Ok(s) = std::fs::read_to_string(path) {
            for l in s.lines() {
                if let Some(h) = l.rsplit('|').next() { last = h.to_string(); }
                recent.push(l.to_string());
            }
        }
        Audit { path: path.into(), user, last, recent }
    }
    /// Appends an entry. The characters `|` and line breaks in `detail` are replaced.
    pub fn log(&mut self, action: &str, detail: &str) {
        let body = format!("{}|{}|{}|{}", iso_now(), self.user, action, detail.replace('|', "/").replace('\n', " "));
        let hash = sha256_hex(format!("{}|{}", self.last, body).as_bytes());
        let line = format!("{}|{}", body, hash);
        if let Some(dir) = std::path::Path::new(&self.path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let ok = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)
            .and_then(|mut f| writeln!(f, "{}", line));
        if ok.is_err() { self.recent.push("!! AUDIT WRITE FAILED !!".into()); }
        self.last = hash;
        self.recent.push(line);
    }
}

/// Re-computes the hash chain of an audit file. Ok(entries) if intact.
pub fn verify(path: &str) -> Result<usize, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))?;
    let mut last = "GENESIS".to_string();
    let mut n = 0;
    for (i, l) in s.lines().enumerate() {
        if l.trim().is_empty() { continue; }
        let (body, hash) = l.rsplit_once('|').ok_or_else(|| format!("line {}: malformed", i + 1))?;
        if sha256_hex(format!("{}|{}", last, body).as_bytes()) != hash {
            return Err(format!("line {}: hash mismatch - audit trail altered or corrupted", i + 1));
        }
        last = hash.to_string();
        n += 1;
    }
    Ok(n)
}
