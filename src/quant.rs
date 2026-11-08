//! Chromatography mathematics.
//!
//! Extraction of ion chromatograms from spectra, peak detection and integration, calibration by
//! weighted regression, and retention-time alignment. Retention times are in minutes, areas in
//! intensity times seconds.
/// A target: a compound defined by its m/z values and, optionally, its retention time.
#[derive(Clone, Debug, PartialEq)]
pub struct Analyte {
    pub name: String,
    pub rt: Option<f64>,
    /// MS2 product (diagnostic) ions; the first one is the quantifier.
    pub ions: Vec<f64>,
    /// optional MS1 precursor m/z, used when quantifying on the MS1 signal.
    pub precursor: Option<f64>,
}

/// CSV/TSV: name, RT (min or n/a), ions (extra columns, or separated by ; | space). First ion = quantifier.
pub fn parse_table(text: &str) -> Result<Vec<Analyte>, String> {
    let mut out: Vec<Analyte> = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let f: Vec<&str> = if line.contains('\t') { line.split('\t').collect() } else { line.split(',').collect() };
        if f.len() < 3 { if out.is_empty() { continue; } return Err(format!("line {}: need name, RT, ions", ln + 1)); }
        let name = f[0].trim().trim_matches('"').to_string();
        let rt_s = f[1].trim().trim_matches('"');
        let rt = rt_s.parse::<f64>().ok();
        // product ions are plain numbers; an optional MS1 precursor is written "p:286.1438"
        let mut ions: Vec<f64> = Vec::new();
        let mut precursor: Option<f64> = None;
        for t in f[2..].iter().flat_map(|s| s.split(|c: char| c.is_whitespace() || c == ';' || c == '|')) {
            let t = t.trim_matches('"');
            if let Some(r) = t.strip_prefix("p:").or_else(|| t.strip_prefix("P:")) { precursor = r.parse::<f64>().ok(); }
            else if let Ok(v) = t.parse::<f64>() { ions.push(v); }
        }
        let rt_na = rt_s.is_empty() || rt_s == "-" || rt_s.eq_ignore_ascii_case("n/a") || rt_s.eq_ignore_ascii_case("na");
        if ions.is_empty() || (rt.is_none() && !rt_na) {
            if out.is_empty() { continue; } // header row
            return Err(format!("line {}: cannot parse RT/ions", ln + 1));
        }
        out.push(Analyte { name, rt, ions, precursor });
    }
    if out.is_empty() { Err("no analytes found".into()) } else { Ok(out) }
}

/// Summed intensity within +-tol of mz for every MS2 scan -> (rt, intensity)
#[must_use]
pub fn xic(spectra: &[crate::mzml::Spectrum], mz: f64, tol: f64) -> Vec<[f64; 2]> {
    spectra.iter().map(|s| {
        let mut i = s.mz.partition_point(|&x| x < mz - tol);
        let mut sum = 0.0f64;
        while i < s.mz.len() && s.mz[i] <= mz + tol { sum += s.inten[i] as f64; i += 1; }
        [s.rt, sum]
    }).collect()
}

/// Intensity of the trace `x` at time `t`, interpolated linearly; 0 outside the trace.
fn y_at(x: &[[f64; 2]], t: f64) -> f64 {
    for w in x.windows(2) {
        if t >= w[0][0] && t <= w[1][0] && w[1][0] > w[0][0] {
            return w[0][1] + (w[1][1] - w[0][1]) * (t - w[0][0]) / (w[1][0] - w[0][0]);
        }
    }
    0.0
}

/// Trapezoidal area over [a,b] in intensity*seconds, boundaries interpolated exactly.
#[must_use]
pub fn integrate(x: &[[f64; 2]], a: f64, b: f64, baseline: bool) -> f64 {
    let mut area = 0.0;
    for w in x.windows(2) {
        let (x0, y0, x1, y1) = (w[0][0], w[0][1], w[1][0], w[1][1]);
        if x1 <= a || x0 >= b || x1 <= x0 { continue; }
        let (xa, xb) = (x0.max(a), x1.min(b));
        let ya = y0 + (y1 - y0) * (xa - x0) / (x1 - x0);
        let yb = y0 + (y1 - y0) * (xb - x0) / (x1 - x0);
        area += 0.5 * (ya + yb) * (xb - xa);
    }
    if baseline { area -= 0.5 * (y_at(x, a) + y_at(x, b)) * (b - a); }
    area * 60.0
}

/// Outline of the area under the trace between `a` and `b`, for drawing the integrated region.
#[must_use]
pub fn shade(x: &[[f64; 2]], a: f64, b: f64) -> Vec<[f64; 2]> {
    let mut p = vec![[a, 0.0], [a, y_at(x, a)]];
    for q in x { if q[0] > a && q[0] < b { p.push(*q); } }
    p.push([b, y_at(x, b)]);
    p.push([b, 0.0]);
    p
}

/// Initial automatic boundaries around the tallest point (within hint +- win if RT known).
#[must_use]
pub fn auto_peak(x: &[[f64; 2]], hint: Option<f64>, win: f64) -> Option<(f64, f64)> {
    let (mut ai, mut best) = (None, 0.0);
    for (i, p) in x.iter().enumerate() {
        if let Some(h) = hint { if (p[0] - h).abs() > win { continue; } }
        if p[1] > best { best = p[1]; ai = Some(i); }
    }
    let a = ai?;
    let mut l = a; let mut rises = 0;
    while l > 0 {
        let prev = x[l - 1][1];
        if prev < 0.05 * best { l -= 1; break; }
        if prev > x[l][1] { rises += 1; if rises >= 2 { l += 1; break; } } else { rises = 0; }
        l -= 1;
    }
    let mut r = a; rises = 0;
    while r + 1 < x.len() {
        let next = x[r + 1][1];
        if next < 0.05 * best { r += 1; break; }
        if next > x[r][1] { rises += 1; if rises >= 2 { r -= 1; break; } } else { rises = 0; }
        r += 1;
    }
    if r <= l { return None; }
    Some((x[l][0], x[r][0]))
}

/// Result of a calibration regression: `area = m * conc + b`.
#[derive(Clone, Copy)]
pub struct Fit { pub m: f64, pub b: f64, pub r2: f64, pub n: usize, pub lo: f64, pub hi: f64 }

/// Weighted linear regression area = m*conc + b. weighting: 0 none, 1 = 1/x, 2 = 1/x^2. pts = (conc, area)
pub fn fit(pts: &[(f64, f64)], weighting: u8) -> Option<Fit> {
    let mut lv: Vec<f64> = pts.iter().map(|p| p.0).collect();
    lv.sort_by(|a, b| a.total_cmp(b));
    lv.dedup();
    if lv.len() < 2 { return None; }
    let (mut sw, mut swx, mut swy, mut swxx, mut swxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(x, y) in pts {
        let w = match weighting { 1 => 1.0 / x, 2 => 1.0 / (x * x), _ => 1.0 };
        sw += w; swx += w * x; swy += w * y; swxx += w * x * x; swxy += w * x * y;
    }
    let den = sw * swxx - swx * swx;
    if den.abs() < 1e-300 { return None; }
    let m = (sw * swxy - swx * swy) / den;
    let b = (swy - m * swx) / sw;
    let ybar = pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64;
    let ssr: f64 = pts.iter().map(|p| (p.1 - (m * p.0 + b)).powi(2)).sum();
    let sst: f64 = pts.iter().map(|p| (p.1 - ybar).powi(2)).sum();
    Some(Fit { m, b, r2: if sst > 0.0 { 1.0 - ssr / sst } else { 1.0 }, n: pts.len(), lo: lv[0], hi: lv[lv.len() - 1] })
}

// ---------------- retention-time alignment ----------------
/// Resamples a trace on a regular time grid by linear interpolation; zero outside the trace.
fn resample(x: &[[f64; 2]], t0: f64, step: f64, n: usize) -> Vec<f64> {
    let mut out = vec![0.0; n];
    let mut j = 0usize;
    for i in 0..n {
        let t = t0 + i as f64 * step;
        if t < x[0][0] || t > x[x.len() - 1][0] { continue; }
        while j + 1 < x.len() && x[j + 1][0] < t { j += 1; }
        if j + 1 >= x.len() { out[i] = x[j][1]; continue; }
        let (x0, y0, x1, y1) = (x[j][0], x[j][1], x[j + 1][0], x[j + 1][1]);
        out[i] = if x1 > x0 { y0 + (y1 - y0) * (t - x0) / (x1 - x0) } else { y0 };
    }
    out
}

/// Removes the slowly varying baseline: subtracts a moving average of half-width `w` points.
fn highpass(v: &mut Vec<f64>, w: usize) {
    let n = v.len();
    let mut pre = vec![0.0; n + 1];
    for i in 0..n { pre[i + 1] = pre[i] + v[i]; }
    let orig = v.clone();
    for i in 0..n {
        let lo = i.saturating_sub(w);
        let hi = (i + w + 1).min(n);
        v[i] = orig[i] - (pre[hi] - pre[lo]) / (hi - lo) as f64;
    }
}

/// Global RT shift (minutes) to ADD to x's retention times so that it best overlays r.
/// Cross-correlation of baseline-removed traces on a 0.01 min grid, lags within +-max.
#[must_use]
pub fn estimate_shift(r: &[[f64; 2]], x: &[[f64; 2]], max: f64) -> f64 {
    if r.len() < 5 || x.len() < 5 { return 0.0; }
    let step = 0.01;
    let t0 = r[0][0].min(x[0][0]) - max;
    let t1 = r[r.len() - 1][0].max(x[x.len() - 1][0]) + max;
    let n = ((t1 - t0) / step) as usize + 1;
    if n < 10 { return 0.0; }
    let mut a = resample(r, t0, step, n);
    let mut b = resample(x, t0, step, n);
    highpass(&mut a, 50);
    highpass(&mut b, 50);
    let lags = (max / step).round() as i64;
    let (mut best, mut best_lag) = (f64::MIN, 0i64);
    for lag in -lags..=lags {
        let mut s = 0.0;
        for i in 0..n {
            let k = i as i64 - lag;
            if k < 0 || k >= n as i64 { continue; }
            s += a[i] * b[k as usize];
        }
        if s > best { best = s; best_lag = lag; }
    }
    best_lag as f64 * step
}

/// Summed intensity of all peaks within +-tol of `target` in one spectrum (m/z must be ascending).
#[must_use]
pub fn intensity_near(mz: &[f64], inten: &[f32], target: f64, tol: f64) -> f64 {
    let mut i = mz.partition_point(|&x| x < target - tol);
    let mut sum = 0.0f64;
    while i < mz.len() && mz[i] <= target + tol { sum += inten[i] as f64; i += 1; }
    sum
}
