//! Scientific-notation display: 100 -> 10², 1234.5 -> 1.2345×10³, 0.05 -> 5×10¯².
//!
//! The superscript minus sign (U+207B) is missing from the fonts that ship with egui and was
//! drawn as an empty square. A macron (U+00AF, part of Latin-1 and present in every font) is used
//! instead: it sits high next to the exponent digits like a superscript minus (`10¯²`).
const SUP: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];

/// Superscript digits for an exponent; a negative sign becomes a macron (see the module documentation).
fn sup(n: i32) -> String {
    let mut s = String::new();
    if n < 0 { s.push('¯'); }
    for c in n.abs().to_string().chars() { s.push(SUP[c.to_digit(10).unwrap_or(0) as usize]); }
    s
}

/// Compact scientific notation (3 significant digits) for axis ticks.
#[must_use]
pub fn sci3(x: f64) -> String { sci_prec(x, 3) }

/// m/z values: four decimals, plain notation: 286.14384 -> "286.1438".
#[must_use]
pub fn mz4(x: f64) -> String { format!("{:.4}", x) }

/// Percent axis labels: 40.0 -> "40%".
#[must_use]
pub fn pct0(x: f64) -> String { format!("{:.0}%", x) }

/// Retention time in minutes, two decimals: 7.2 -> "7.20".
#[must_use]
pub fn rt2(x: f64) -> String { format!("{:.2}", x) }

/// Up to 6 significant digits, trailing zeros trimmed.
#[must_use]
pub fn sci(x: f64) -> String { sci_prec(x, 6) }

/// Scientific notation with `sig` significant digits (used for m/z so that high-resolution values keep their decimals).
#[must_use]
pub fn sci_prec(x: f64, sig: usize) -> String {
    if x.is_nan() { return "NaN".into(); }
    if x.is_infinite() { return if x > 0.0 { "∞".into() } else { "-∞".into() }; }
    if x == 0.0 { return "0".into(); }
    let s = format!("{:.*e}", sig.max(1) - 1, x);
    let (m, e) = match s.split_once('e') { Some(p) => p, None => return s };
    let exp: i32 = e.parse().unwrap_or(0);
    let m = if m.contains('.') { m.trim_end_matches('0').trim_end_matches('.') } else { m };
    match m {
        "1" => format!("10{}", sup(exp)),
        "-1" => format!("-10{}", sup(exp)),
        _ => format!("{}×10{}", m, sup(exp)),
    }
}

/// Maps a superscript character back to the ASCII character it stands for.
fn unsup(c: char) -> Option<char> {
    match c {
        '⁰' => Some('0'), '¹' => Some('1'), '²' => Some('2'), '³' => Some('3'), '⁴' => Some('4'),
        '⁵' => Some('5'), '⁶' => Some('6'), '⁷' => Some('7'), '⁸' => Some('8'), '⁹' => Some('9'),
        '¯' | '⁻' => Some('-'), '⁺' => Some('+'), _ => None,
    }
}

/// Accepts plain numbers ("12.5", "1e3"), and the display form ("1.5×10²", "10³", "5x10^-2").
#[must_use]
pub fn parse_sci(s: &str) -> Option<f64> {
    let t = s.trim().replace(',', ".");
    if t.is_empty() { return None; }
    if let Ok(v) = t.parse::<f64>() { return Some(v); }
    let t = t.replace('x', "×").replace('*', "×").replace('·', "×");
    let (mant, rest) = match t.split_once('×') {
        Some((m, r)) => (m.trim().to_string(), r.trim().to_string()),
        None => ("1".to_string(), t.clone()),
    };
    let mut m: f64 = mant.parse().ok()?;
    let mut rest = rest;
    if let Some(r) = rest.strip_prefix('-') { m = -m; rest = r.to_string(); }
    let rest = rest.strip_prefix("10")?.trim_start_matches('^').to_string();
    let exp_s: String = rest.chars().map(|c| unsup(c).unwrap_or(c)).collect();
    let e: i32 = if exp_s.is_empty() { 1 } else { exp_s.parse().ok()? };
    Some(m * 10f64.powi(e))
}
