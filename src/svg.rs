//! Vector export of a plot (chromatogram or MS2 spectrum) as a stand-alone SVG file.

/// One curve or set of markers of an SVG figure.
pub struct SvgSeries {
    pub name: String,
    pub rgb: (u8, u8, u8),
    pub width: f32,
    pub points: Vec<[f64; 2]>,
    /// draw the points as filled circles instead of a connected line (calibration points)
    pub marker: bool,
}
/// Text label placed above a data point (e.g. the m/z of the most intense peaks of a spectrum).
pub struct SvgLabel { pub x: f64, pub y: f64, pub text: String, pub rgb: (u8, u8, u8) }
/// A vertical reference line, for example an integration boundary.
pub struct SvgVLine { pub x: f64, pub rgb: (u8, u8, u8), pub label: String }

/// Everything needed to draw one figure.
pub struct SvgPlot {
    pub title: String,
    pub x_label: String,
    pub y_label: String,
    pub series: Vec<SvgSeries>,
    pub vlines: Vec<SvgVLine>,
    pub labels: Vec<SvgLabel>,
    /// text lines in a box in the top-left corner of the plot (e.g. regression formula and R²)
    pub notes: Vec<String>,
    /// visible range: [x_min, x_max, y_min, y_max]
    pub bounds: [f64; 4],
    pub x_fmt: fn(f64) -> String,
    pub y_fmt: fn(f64) -> String,
}

/// "Nice" tick positions (1-2-5 steps) inside [min, max].
#[must_use]
pub fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    let range = max - min;
    if !(range.is_finite() && range > 0.0) { return vec![min]; }
    let raw = range / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = mag * if norm < 1.5 { 1.0 } else if norm < 3.0 { 2.0 } else if norm < 7.0 { 5.0 } else { 10.0 };
    let start = (min / step).ceil();
    let mut v = vec![];
    let mut i = 0.0;
    while (start + i) * step <= max + step * 1e-9 && v.len() < 200 { v.push((start + i) * step); i += 1.0; }
    v
}

/// Escapes the characters that are special in XML.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Renders the figure as a stand-alone SVG document.
#[must_use]
pub fn render(p: &SvgPlot) -> String {
    const W: f64 = 1200.0;
    const H: f64 = 620.0;
    let (l, r, t, b) = (130.0, 280.0, 50.0, 70.0);
    let (pw, ph) = (W - l - r, H - t - b);
    let [x0, mut x1, y0, mut y1] = p.bounds;
    if !(x1 > x0) { x1 = x0 + 1.0; }
    if !(y1 > y0) { y1 = y0 + 1.0; }
    let fx = |x: f64| l + (x - x0) / (x1 - x0) * pw;
    let fy = |y: f64| t + ph - (y - y0) / (y1 - y0) * ph;
    let col = |c: (u8, u8, u8)| format!("rgb({},{},{})", c.0, c.1, c.2);
    let mut s = String::new();
    s += &format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\" viewBox=\"0 0 {W} {H}\" font-family=\"Arial, Helvetica, sans-serif\" font-size=\"13\">\n");
    s += &format!("<rect width=\"{W}\" height=\"{H}\" fill=\"white\"/>\n");
    s += &format!("<clipPath id=\"plotarea\"><rect x=\"{l}\" y=\"{t}\" width=\"{pw}\" height=\"{ph}\"/></clipPath>\n");
    s += &format!("<text x=\"{l}\" y=\"30\" font-size=\"16\" font-weight=\"bold\">{}</text>\n", esc(&p.title));
    for v in nice_ticks(x0, x1, 8) {
        let x = fx(v);
        s += &format!("<line x1=\"{x:.2}\" y1=\"{t}\" x2=\"{x:.2}\" y2=\"{:.2}\" stroke=\"#e0e0e0\"/>\n", t + ph);
        s += &format!("<text x=\"{x:.2}\" y=\"{:.2}\" text-anchor=\"middle\">{}</text>\n", t + ph + 20.0, esc(&(p.x_fmt)(v)));
    }
    for v in nice_ticks(y0, y1, 6) {
        let y = fy(v);
        s += &format!("<line x1=\"{l}\" y1=\"{y:.2}\" x2=\"{:.2}\" y2=\"{y:.2}\" stroke=\"#e0e0e0\"/>\n", l + pw);
        s += &format!("<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"end\">{}</text>\n", l - 8.0, y + 4.0, esc(&(p.y_fmt)(v)));
    }
    s += &format!("<rect x=\"{l}\" y=\"{t}\" width=\"{pw}\" height=\"{ph}\" fill=\"none\" stroke=\"#444\"/>\n");
    s += "<g clip-path=\"url(#plotarea)\" fill=\"none\">\n";
    for v in &p.vlines {
        s += &format!("<line x1=\"{x:.2}\" y1=\"{t}\" x2=\"{x:.2}\" y2=\"{:.2}\" stroke=\"{}\" stroke-width=\"1.2\" stroke-dasharray=\"5,3\"/>\n", t + ph, col(v.rgb), x = fx(v.x));
    }
    for sr in &p.series {
        if sr.marker {
            for q in sr.points.iter().filter(|q| q[0].is_finite() && q[1].is_finite()) {
                s += &format!("<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"4.5\" fill=\"{}\" fill-opacity=\"0.85\" stroke=\"#222\" stroke-width=\"0.8\"/>\n", fx(q[0]), fy(q[1]), col(sr.rgb));
            }
            continue;
        }
        let mut d = String::new();
        for (i, q) in sr.points.iter().filter(|q| q[0].is_finite() && q[1].is_finite()).enumerate() {
            d += &format!("{}{:.2},{:.2} ", if i == 0 { "M" } else { "L" }, fx(q[0]), fy(q[1]));
        }
        if d.is_empty() { continue; }
        s += &format!("<path d=\"{}\" stroke=\"{}\" stroke-width=\"{}\" stroke-linejoin=\"round\"/>\n", d.trim_end(), col(sr.rgb), sr.width);
    }
    s += "</g>\n";
    if !p.notes.is_empty() {
        let w = p.notes.iter().map(|n| n.chars().count()).max().unwrap_or(0) as f64 * 8.4 + 20.0;
        let h = p.notes.len() as f64 * 20.0 + 10.0;
        s += &format!("<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"white\" fill-opacity=\"0.9\" stroke=\"#999\"/>\n", l + 8.0, t + 8.0, w, h);
        for (i, n) in p.notes.iter().enumerate() {
            s += &format!("<text x=\"{:.2}\" y=\"{:.2}\" font-size=\"14\">{}</text>\n", l + 18.0, t + 28.0 + i as f64 * 20.0, esc(n));
        }
    }
    for lb in &p.labels {
        s += &format!("<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"middle\" font-size=\"11\" fill=\"{}\">{}</text>\n", fx(lb.x), fy(lb.y) - 5.0, col(lb.rgb), esc(&lb.text));
    }
    s += &format!("<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"middle\" font-size=\"14\">{}</text>\n", l + pw / 2.0, H - 18.0, esc(&p.x_label));
    s += &format!("<text transform=\"translate(26,{:.2}) rotate(-90)\" text-anchor=\"middle\" font-size=\"14\">{}</text>\n", t + ph / 2.0, esc(&p.y_label));
    let mut ly = t + 12.0;
    let lx = l + pw + 20.0;
    for sr in p.series.iter().take(30) {
        if sr.marker {
            s += &format!("<circle cx=\"{}\" cy=\"{ly}\" r=\"4.5\" fill=\"{}\" stroke=\"#222\" stroke-width=\"0.8\"/>\n", lx + 12.0, col(sr.rgb));
        } else {
            s += &format!("<line x1=\"{lx}\" y1=\"{ly}\" x2=\"{}\" y2=\"{ly}\" stroke=\"{}\" stroke-width=\"3\"/>\n", lx + 24.0, col(sr.rgb));
        }
        s += &format!("<text x=\"{}\" y=\"{}\">{}</text>\n", lx + 32.0, ly + 4.0, esc(&sr.name));
        ly += 20.0;
    }
    for v in &p.vlines {
        if v.label.is_empty() { continue; }
        s += &format!("<line x1=\"{lx}\" y1=\"{ly}\" x2=\"{}\" y2=\"{ly}\" stroke=\"{}\" stroke-width=\"2\" stroke-dasharray=\"5,3\"/>\n", lx + 24.0, col(v.rgb));
        s += &format!("<text x=\"{}\" y=\"{}\">{}</text>\n", lx + 32.0, ly + 4.0, esc(&v.label));
        ly += 20.0;
    }
    s += "</svg>\n";
    s
}
