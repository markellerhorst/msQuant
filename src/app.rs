//! The msQuant application: user interface, analysis state and their coordination.
//!
//! [`App`] owns the imported files, the targets, the calibration and the undo history, and draws
//! the interface with egui. Heavy work (reading raw files, saving projects) runs in background
//! threads that report back through [`Msg`]; every change of the analysis state happens on the UI
//! thread.
use crate::audit::{self, Audit};
use crate::fmt;
use crate::history::History;
use crate::mzml::{self, MzData, Spectrum};
use crate::paths;
use crate::png;
use crate::project::{self, BlobSource, CellRec, ProjectFile, RawSource, SampleRec};
use crate::quant::{self, Analyte, Fit};
use crate::rawconv;
use crate::svg::{self, SvgLabel, SvgPlot, SvgSeries, SvgVLine};
use eframe::egui::{self, Color32, Key, Modifiers, PointerButton, RichText};
use egui_plot::{
    Legend, Line, Plot, PlotBounds, PlotPoint, PlotPoints, Points, Polygon, Text, VLine,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// Version shown in the Help menu and written to result exports.
const VERSION: &str = "0.1.0";
/// Labels of the regression weightings, indexed like `App::weighting`.
const WEIGHTS: [&str; 3] = ["none", "1/x", "1/x²"];
/// Text of the README, embedded so that it can be shown offline from the Help menu.
const README: &str = include_str!("../README.md");
/// Number of undo (and of redo) steps that are kept.
const MAX_UNDO: usize = 10;

/// Scientific-notation styling for every plot (axes + hover text).
macro_rules! sciplot {
    ($p:expr) => {
        $p.x_axis_formatter(|m, _| fmt::sci(m.value))
            .y_axis_formatter(|m, _| fmt::sci3(m.value))
            .y_axis_min_width(85.0)
            .label_formatter(|name, v| {
                let head = if name.is_empty() { String::new() } else { format!("{}\n", name) };
                format!("{}x = {}\ny = {}", head, fmt::sci(v.x), fmt::sci(v.y))
            })
    };
}

/// Chromatogram plots: retention-time axis with two decimals; y axis in compact scientific notation, or 0-100 % when normalised.
/// The y axis gets a minimum width so that the axis title never collides with the tick labels.
macro_rules! rtplot {
    ($p:expr, $norm:expr) => {{
        let mut p = $p
            .x_axis_formatter(|m, _| format!("{:.2}", m.value))
            .y_axis_formatter(move |m, _| if $norm { fmt::pct0(m.value) } else { fmt::sci3(m.value) })
            .y_axis_min_width(85.0)
            .show_x(false)
            .show_y(false);
        if $norm { p = p.include_y(0.0).include_y(100.0).set_margin_fraction(egui::vec2(0.0, 0.05)); }
        p
    }};
}

/// Spectrum plots: m/z with four decimals; no default cursor label (the snapped peak is drawn by hand).
macro_rules! mzplot {
    ($p:expr, $norm:expr) => {{
        let mut p = $p
            .x_axis_formatter(|m, _| fmt::mz4(m.value))
            .y_axis_formatter(move |m, _| if $norm { fmt::pct0(m.value) } else { fmt::sci3(m.value) })
            .y_axis_min_width(85.0)
            .show_x(false)
            .show_y(false);
        if $norm { p = p.include_y(0.0).include_y(100.0).set_margin_fraction(egui::vec2(0.0, 0.05)); }
        p
    }};
}

/// Draws the traces, finds the point nearest to the cursor and annotates it with m/z, RT, scan (and precursor) and intensity.
/// `$cap`: true while a PNG capture is pending (no hover annotation in the picture).
macro_rules! draw_series {
    ($pu:ident, $series:expr, $hover:ident, $scan_of:ident, $cap:expr) => {{
        for s in $series.iter() {
            let d: Vec<[f64; 2]> = s.data.iter().map(|p| [p[0], p[1] * s.scale]).collect();
            let mut l = Line::new(PlotPoints::from(d)).name(s.name.clone()).color(pal(s.ci));
            if s.cur { l = l.width(2.5_f32); }
            $pu.line(l);
        }
        if !$cap {
            if let Some(p) = $pu.pointer_coordinate() {
                let b = $pu.plot_bounds();
                if $pu.response().hovered() && in_view(&p, &b) { $hover = nearest_hover($series, p.x, p.y, b.max()[0] - b.min()[0]); }
            }
        }
        if let Some(h) = &$hover {
            let b = $pu.plot_bounds();
            let right = h.rt > (b.min()[0] + b.max()[0]) / 2.0;
            let pct = if h.scale != 1.0 { format!(" ({:.1}%)", h.y * h.scale) } else { String::new() };
            let txt = format!("{}\n{}\nRT {:.2} min · {}\nintensity {}{}", h.fname, h.desc, h.rt, $scan_of(h.file, h.level, h.idx), fmt::sci(h.y), pct);
            $pu.points(Points::new(vec![[h.rt, h.y * h.scale]]).radius(5.0_f32).color(Color32::WHITE));
            $pu.text(Text::new(PlotPoint::new(h.rt, h.y * h.scale),
                RichText::new(txt).color(Color32::WHITE).background_color(Color32::from_black_alpha(200)))
                .anchor(if right { egui::Align2::RIGHT_BOTTOM } else { egui::Align2::LEFT_BOTTOM }));
        }
    }};
}

/// Stores a calibration-plot export request (kind, figure, plot area, suggested file name) in `slot`.
fn export_req_cal(slot: &mut Option<(ExportKind, SvgPlot, egui::Rect, String)>, k: ExportKind, plot: SvgPlot, rect: egui::Rect, stem: String) {
    *slot = Some((k, plot, rect, stem));
}

/// Zoom window: the rectangle between the drag start and the pointer, if it is large enough to be intended (>1 % of the view in both directions).
fn zoom_box(start: [f64; 2], end: [f64; 2], xw: f64, yw: f64) -> Option<([f64; 2], [f64; 2])> {
    let (x0, x1) = (start[0].min(end[0]), start[0].max(end[0]));
    let (y0, y1) = (start[1].min(end[1]), start[1].max(end[1]));
    if x1 - x0 > 0.01 * xw && y1 - y0 > 0.01 * yw { Some(([x0, y0], [x1, y1])) } else { None }
}

/// One of the two integration boundaries of a peak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    Start,
    End,
}

/// Integration boundary closest to the pointer, if one is within `max_px` pixels.
///
/// `px_per_unit` converts the distance in plot units (minutes) to screen pixels.
fn nearest_boundary(
    px: f64,
    start: f64,
    end: f64,
    px_per_unit: f64,
    max_px: f64,
) -> Option<Boundary> {
    let (ds, de) = ((px - start).abs() * px_per_unit, (px - end).abs() * px_per_unit);
    if ds.min(de) > max_px {
        None
    } else if ds <= de {
        Some(Boundary::Start)
    } else {
        Some(Boundary::End)
    }
}

/// True if the point lies inside the visible data area of a plot (not on the axes or outside the plot).
fn in_view(p: &PlotPoint, b: &PlotBounds) -> bool {
    p.x >= b.min()[0] && p.x <= b.max()[0] && p.y >= b.min()[1] && p.y <= b.max()[1]
}

/// Scales the interval [lo, hi] by `factor` around `pivot` (factor < 1 zooms in); the pivot keeps its relative position.
fn scale_range(lo: f64, hi: f64, pivot: f64, factor: f64) -> (f64, f64) {
    (pivot - (pivot - lo) * factor, pivot + (hi - pivot) * factor)
}

/// Mouse navigation of a plot: drag a box with the left button to zoom to it, Shift+left-drag or middle-drag pans,
/// Alt+wheel scales the y axis and Shift+Alt+wheel the x axis (about the cursor). Everything only reacts while the pointer is
/// inside the data area of the plot. `$blocked`: true while the left button is busy with something else (moving a boundary).
/// Evaluates to true when the view was zoomed by a box.
macro_rules! zoom_pan {
    ($pu:ident, $r:ident, $zoom:ident, $blocked:expr) => {{
        let (alt, shift) = $r.ctx.input(|i| (i.modifiers.alt, i.modifiers.shift));
        let mut zoomed = false;
        let inside = $r.hovered() && $pu.pointer_coordinate().map(|p| in_view(&p, &$pu.plot_bounds())).unwrap_or(false);
        if alt && inside {
            let raw = $r.ctx.input(|i| i.raw_scroll_delta);
            let d = (if raw.y != 0.0 { raw.y } else { raw.x }) as f64;
            if d != 0.0 {
                if let Some(p) = $pu.pointer_coordinate() {
                    let b = $pu.plot_bounds();
                    let f = (-d * 0.003).exp();
                    let (mut x0, mut x1, mut y0, mut y1) = (b.min()[0], b.max()[0], b.min()[1], b.max()[1]);
                    if shift { let (n0, n1) = scale_range(x0, x1, p.x, f); x0 = n0; x1 = n1; }
                    else { let (n0, n1) = scale_range(y0, y1, p.y, f); y0 = n0; y1 = n1; }
                    $pu.set_plot_bounds(PlotBounds::from_min_max([x0, y0], [x1, y1]));
                }
            }
        }
        if $r.dragged_by(PointerButton::Middle) || ($r.dragged_by(PointerButton::Primary) && shift && !alt) {
            let d = $pu.pointer_coordinate_drag_delta();
            $pu.translate_bounds(-d);
        } else if let Some(p) = $pu.pointer_coordinate() {
            let b0 = $pu.plot_bounds();
            let cp = [p.x.max(b0.min()[0]).min(b0.max()[0]), p.y.max(b0.min()[1]).min(b0.max()[1])];
            if $r.drag_started_by(PointerButton::Primary) && !$blocked && in_view(&p, &b0) { $zoom = Some([p.x, p.y]); }
            if $r.dragged_by(PointerButton::Primary) {
                if let Some(s0) = $zoom {
                    let (x0, x1) = (s0[0].min(cp[0]), s0[0].max(cp[0]));
                    let (y0, y1) = (s0[1].min(cp[1]), s0[1].max(cp[1]));
                    $pu.polygon(Polygon::new(PlotPoints::from(vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]))
                        .fill_color(Color32::from_rgba_unmultiplied(100, 150, 255, 45))
                        .stroke(egui::Stroke::new(1.5_f32, Color32::from_rgb(100, 150, 255))));
                }
            }
            if $r.drag_stopped() {
                if let Some(s0) = $zoom.take() {
                    let (xw, yw) = (b0.max()[0] - b0.min()[0], b0.max()[1] - b0.min()[1]);
                    if let Some((lo, hi)) = zoom_box(s0, cp, xw, yw) {
                        $pu.set_plot_bounds(PlotBounds::from_min_max(lo, hi));
                        zoomed = true;
                    }
                }
            }
        }
        zoomed
    }};
}

/// Right-click menu of a plot: reset zoom, save as PNG / SVG, or one trace as a two-column CSV.
macro_rules! export_menu {
    ($resp:expr, $what:expr, $out:ident, $names:expr) => {
        $resp.context_menu(|ui| {
            if ui.button("Reset zoom").clicked() { $out = Some(ExportKind::Reset); ui.close_menu(); }
            ui.weak("drag a box = zoom · Shift/middle-drag = pan");
            ui.weak("Alt+wheel = scale y · Shift+Alt+wheel = scale x");
            ui.separator();
            if ui.button(format!("Save {} as PNG…", $what)).clicked() { $out = Some(ExportKind::Png); ui.close_menu(); }
            if ui.button(format!("Save {} as SVG…", $what)).clicked() { $out = Some(ExportKind::Svg); ui.close_menu(); }
            ui.separator();
            for (i, n) in $names.iter().enumerate().take(20) {
                if ui.button(format!("Save '{}' as CSV (2 columns)…", n)).clicked() { $out = Some(ExportKind::Csv(i)); ui.close_menu(); }
            }
        })
    };
}

/// What the user chose in the right-click menu of a plot.
#[derive(Clone, Copy, PartialEq)]
enum ExportKind { Png, Svg, Csv(usize), Reset }

/// A pending PNG capture of one plot (taken from the window screenshot after the context menu has closed).
struct Capture { rect: egui::Rect, path: PathBuf, frames: u8, sent: bool }

/// Converts an egui color to an RGB triple for SVG output.
fn color_rgb(c: Color32) -> (u8, u8, u8) { (c.r(), c.g(), c.b()) }

/// Replaces everything except ASCII letters, digits, `-` and `.` by `_`, so that the text can be used in a file name.
fn safe_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

/// Converts plot series to SVG series, applying the display scale (for example 100/max when normalized).
fn svg_series(series: &[Series]) -> Vec<SvgSeries> {
    series.iter().map(|s| SvgSeries {
        name: s.name.clone(), rgb: color_rgb(pal(s.ci)), width: if s.cur { 2.5 } else { 1.5 },
        points: s.data.iter().map(|p| [p[0], p[1] * s.scale]).collect(), marker: false,
    }).collect()
}

/// Two-column CSV text.
fn csv_two(h1: &str, h2: &str, pts: impl Iterator<Item = (f64, f64)>) -> String {
    let mut s = format!("{},{}\n", h1, h2);
    for (x, y) in pts { s += &format!("{},{}\n", x, y); }
    s
}

/// Precursor m/z for display: four decimals or "n/a".
fn prec_str(p: Option<f64>) -> String { p.map(fmt::mz4).unwrap_or_else(|| "n/a".into()) }

/// The `n` most intense peaks (m/z, intensity), strongest first.
fn top_peaks(mz: &[f64], inten: &[f32], n: usize) -> Vec<(f64, f64)> {
    let mut v: Vec<(f64, f64)> = mz.iter().cloned().zip(inten.iter().map(|&i| i as f64)).collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v.truncate(n);
    v
}

/// Peak (m/z, scaled intensity) closest to the cursor: plot-normalised distance from the cursor to the peak's stem.
fn nearest_peak(mz: &[f64], inten: &[f32], scale: f64, px: f64, py: f64, xw: f64, yw: f64) -> Option<(f64, f64)> {
    if mz.is_empty() || xw <= 0.0 || yw <= 0.0 { return None; }
    let lo = mz.partition_point(|&m| m < px - 0.08 * xw);
    let mut best: Option<(f64, f64, f64)> = None;
    for i in lo..mz.len() {
        if mz[i] > px + 0.08 * xw { break; }
        let y = inten[i] as f64 * scale;
        let dx = (mz[i] - px).abs() / xw;
        let dy0 = if py > y { py - y } else if py < 0.0 { -py } else { 0.0 };
        let dy = dy0 / yw;
        let d = (dx * dx + dy * dy).sqrt();
        if best.map(|b| d < b.0).unwrap_or(true) { best = Some((d, mz[i], y)); }
    }
    best.map(|b| (b.1, b.2))
}

/// "MS2 scan 123 · precursor m/z 286.1438" (or "n/a"), "MS1 scan 45".
fn scan_info(d: &MzData, l: u8, j: usize) -> String {
    if l == 1 { format!("MS1 scan {}", d.ms1.get(j).map(|x| x.scan).unwrap_or(0)) }
    else {
        match d.spectra.get(j) {
            Some(x) => format!("MS2 scan {} · precursor m/z {}", x.scan, prec_str(x.precursor)),
            None => "MS2 scan ?".into(),
        }
    }
}

/// One trace of a chromatogram plot.
struct Series {
    name: String, data: Vec<[f64; 2]>, ci: usize, cur: bool, file: usize,
    /// 1 = points index the MS1 scan list, 2 = the MS2 scan list.
    level: u8, fname: String, desc: String, scale: f64,
}

/// The data point nearest to the cursor in a chromatogram plot.
struct Hover { file: usize, rt: f64, y: f64, scale: f64, idx: usize, level: u8, fname: String, desc: String }

/// Point of any series closest to the cursor: nearest in time, then the trace whose intensity is closest to the cursor.
fn nearest_hover(series: &[Series], px: f64, py: f64, width: f64) -> Option<Hover> {
    let mut best: Option<(f64, Hover)> = None;
    for s in series {
        if s.data.is_empty() { continue; }
        let j = s.data.partition_point(|p| p[0] < px);
        let (c0, c1) = (j.saturating_sub(1), j.min(s.data.len() - 1));
        let bj = if (s.data[c1][0] - px).abs() < (s.data[c0][0] - px).abs() { c1 } else { c0 };
        let p = s.data[bj];
        if width > 0.0 && (p[0] - px).abs() > 0.05 * width { continue; }
        let dy = (p[1] * s.scale - py).abs();
        if best.as_ref().map(|b| dy < b.0).unwrap_or(true) {
            best = Some((dy, Hover { file: s.file, rt: p[0], y: p[1], scale: s.scale, idx: bj, level: s.level, fname: s.fname.clone(), desc: s.desc.clone() }));
        }
    }
    best.map(|b| b.1)
}

/// Provenance of a freshly imported file, sent from the loading thread to the UI thread.
struct LoadInfo { path: String, raw_sha: String, detail: String, source: RawSource }
/// Messages from background threads to the UI thread.
enum Msg {
    Loaded(MzData, LoadInfo),
    Err(String),
    Finished,
    ProjectSaved { path: PathBuf, result: Result<Vec<Option<(u64, u64)>>, String>, src_paths: Vec<String>, missing: Vec<String> },
}

/// Text of the credits window.
const CREDITS: &str = "Thermo .raw files are read through ThermoRawFileParser (CompOmics, Apache-2.0), run as an external program; nothing is bundled or linked: https://github.com/compomics/ThermoRawFileParser\n\
Hulstaert N, Shofstahl J, Sachsenberg T, Walzer M, Barsnes H, Martens L, Perez-Riverol Y. J Proteome Res 2020;19:537-542 (doi:10.1021/acs.jproteome.9b00328).\n\
It uses Thermo Fisher Scientific's RawFileReader library (Copyright Thermo Fisher Scientific, Inc.), subject to Thermo's license.\n\
The external-converter approach follows mzmine: https://github.com/mzmine/mzmine (MIT).\n\
GUI: egui / eframe / egui_plot (MIT or Apache-2.0), https://github.com/emilk/egui; file dialogs: rfd (MIT).\n\
Project: https://github.com/markellerhorst/lcms-quant\n\
Built with Claude Sonnet 5.5 (Medium); app icon generated with Gemini.";

/// Role of a file in the analysis.
#[derive(Clone, Copy, PartialEq)]
enum Kind { Blank, Standard, QC, Unknown }
impl Kind {
    /// All kinds, in the order in which they are offered in menus.
    const ALL: [Kind; 4] = [Kind::Blank, Kind::Standard, Kind::QC, Kind::Unknown];
    /// Display name of the kind; it is also what a project file stores.
    fn label(self) -> &'static str {
        match self { Kind::Blank => "Blank", Kind::Standard => "Standard", Kind::QC => "QC", Kind::Unknown => "Unknown" }
    }
    /// Parses a stored label; unknown labels become [`Kind::Unknown`].
    fn from_label(s: &str) -> Kind {
        match s { "Blank" => Kind::Blank, "Standard" => Kind::Standard, "QC" => Kind::QC, _ => Kind::Unknown }
    }
}
/// How results are reported.
#[derive(Clone, Copy, PartialEq)]
enum Mode { Areas, Curve }
/// Which chromatogram the chromatogram tab shows.
#[derive(Clone, Copy, PartialEq)]
enum View { Xic, Tic, Bpc, Quick }
/// Which traces the target view of the chromatogram tab shows.
#[derive(Clone, Copy, PartialEq)]
enum Scope { Ions, AllTargets, Files }
/// Tabs of the central panel.
#[derive(Clone, Copy, PartialEq)]
enum Tab { Chrom, Matrix, Setup }
/// Which signal of a target is integrated and quantified.
#[derive(Clone, Copy, PartialEq)]
enum Basis { Ms2, Ms1 }

/// Integration state of one target in one file (one cell of the results matrix).
#[derive(Clone)]
struct Cell { start: f64, end: f64, found: bool, manual: bool, areas: Vec<f64>, logged: (f64, f64), include: bool, conc: f64 }
impl Cell {
    /// Creates an empty cell: no peak, no standard concentration, included in calibrations.
    fn new() -> Cell { Cell { start: 0.0, end: 0.0, found: false, manual: false, areas: vec![], logged: (0.0, 0.0), include: true, conc: 0.0 } }
}

/// An imported file together with its analysis state.
struct Sample {
    data: Arc<MzData>, src_path: String, raw_sha: String, source: RawSource, kind: Kind,
    xics: Vec<Vec<Vec<[f64; 2]>>>, cells: Vec<Cell>,
    show: bool, rt_shift: f64, shift_ui: f64,
}

/// Everything undo/redo needs. Raw data is shared (Arc), derived chromatograms are recomputed on restore.
#[derive(Clone)]
struct SampleState {
    data: Arc<MzData>, src_path: String, raw_sha: String, source: RawSource, kind: Kind,
    cells: Vec<Cell>, show: bool, rt_shift: f64,
}
/// Complete analysis state at one point in time, used for undo and redo.
///
/// The raw data is shared with the live state through [`Arc`] instead of being copied: a snapshot
/// must keep the data of a removed file alive without duplicating hundreds of megabytes. The
/// extracted chromatograms are not part of a snapshot, they are recomputed on restore.
#[derive(Clone)]
struct Snapshot {
    label: String, analytes: Vec<Analyte>, samples: Vec<SampleState>, sel_s: usize, sel_a: usize,
    params: Vec<(String, String)>,
}

/// Bookkeeping while a project is opened and its files are re-imported in the background.
struct Pending { recs: Vec<SampleRec>, mismatches: Vec<String> }

/// Application state and user interface of msQuant.
pub struct App {
    analytes: Vec<Analyte>,
    samples: Vec<Sample>,
    fits: Vec<Option<Fit>>,
    sel_s: usize,
    sel_a: usize,
    anchor: usize,
    tol_ppm: bool,
    tol: f64,
    rt_win: f64,
    weighting: u8,
    baseline: bool,
    ratio_tol: f64,
    units: String,
    reason: String,
    settings_dirty: bool,
    out_path: String,
    data_dir: Option<PathBuf>,
    status: String,
    show_audit: bool,
    show_credits: bool,
    show_readme: bool,
    show_targets: bool,
    grab: Option<Boundary>,
    editing: bool,
    audit: Audit,
    user_logged: String,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    loading: usize,
    trfp_path: String,
    mode: Mode,
    view: View,
    scope: Scope,
    tab: Tab,
    tic_level: u8,
    normalize: bool,
    matrix_conc: bool,
    align_on: bool,
    align_max: f64,
    align_ref: usize,
    history: History<Snapshot>,
    last_undo_label: String,
    rows: Vec<[String; 4]>,
    rows_dirty: bool,
    table_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
    pending: Option<Pending>,
    zoom: f32,
    title: String,
    basis: Basis,
    show_spectrum: bool,
    spec_autofit: bool,
    follow_hover: bool,
    pinned_scan: Option<(usize, usize)>,
    hover: Option<(usize, f64)>,
    col_w: [f32; 4],
    embed_raw: bool,
    saving: usize,
    capture: Option<Capture>,
    show_prefs: bool,
    reset_cal: bool,
    zoom_chrom: Option<[f64; 2]>,
    zoom_spec: Option<[f64; 2]>,
    reset_chrom: bool,
    reset_spec: bool,
    spec_last: Option<(usize, usize)>,
    assign_target: usize,
    assign_conc: f64,
    quick_mz: f64,
    quick_level: u8,
}

/// Quotes a CSV field if it contains a comma, a quote or a line break.
fn csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// Recomputes the areas of all traces of `c` from its integration boundaries.
fn calc_areas(xics: &[Vec<[f64; 2]>], c: &mut Cell, baseline: bool) {
    c.areas = xics.iter().map(|x| if c.found { quant::integrate(x, c.start, c.end, baseline) } else { 0.0 }).collect();
}

/// Returns the `i`-th color of the plot palette, starting over after the last one.
fn pal(i: usize) -> Color32 {
    const P: [(u8, u8, u8); 10] = [(31, 119, 180), (214, 39, 40), (44, 160, 44), (255, 127, 14), (148, 103, 189),
                                   (140, 86, 75), (227, 119, 194), (127, 127, 127), (188, 189, 34), (23, 190, 207)];
    let c = P[i % 10];
    Color32::from_rgb(c.0, c.1, c.2)
}

/// Which summary chromatogram of a file to show.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SummaryTrace {
    TotalIon,
    BasePeak,
}

/// Total-ion or base-peak chromatogram of the MS1 (`level` 1) or MS2 scans of a file.
fn chrom(d: &MzData, level: u8, kind: SummaryTrace) -> &[[f64; 2]] {
    match (level, kind) {
        (1, SummaryTrace::TotalIon) => &d.tic1,
        (1, SummaryTrace::BasePeak) => &d.bpc1,
        (_, SummaryTrace::TotalIon) => &d.tic2,
        (_, SummaryTrace::BasePeak) => &d.bpc2,
    }
}

/// Trace used to align retention times: the MS1 total-ion chromatogram if there is one.
fn align_trace(d: &MzData) -> &[[f64; 2]] {
    if d.tic1.len() >= 5 {
        &d.tic1
    } else {
        &d.tic2
    }
}

/// DragValue that shows and parses scientific notation.
fn dv(v: &mut f64) -> egui::DragValue<'_> {
    egui::DragValue::new(v).custom_formatter(|n, _| fmt::sci(n)).custom_parser(fmt::parse_sci)
}

/// DragValue for retention times / durations in minutes: two decimals ("7.20").
fn dv_rt(v: &mut f64) -> egui::DragValue<'_> {
    egui::DragValue::new(v).custom_formatter(|n, _| format!("{:.2}", n)).custom_parser(fmt::parse_sci)
}

/// DragValue for m/z values: four decimals.
fn dv_mz(v: &mut f64) -> egui::DragValue<'_> {
    egui::DragValue::new(v).custom_formatter(|n, _| fmt::mz4(n)).custom_parser(fmt::parse_sci)
}

/// DragValue for standard concentrations: "–" means "not a standard for this target".
fn dv_conc(v: &mut f64) -> egui::DragValue<'_> {
    egui::DragValue::new(v)
        .custom_formatter(|n, _| if n == 0.0 { "–".to_string() } else { fmt::sci(n) })
        .custom_parser(|s| if s.trim().is_empty() || s.trim() == "–" || s.trim() == "-" { Some(0.0) } else { fmt::parse_sci(s) })
}

/// Parses one row of the target editor; returns `None` if a field is invalid.
fn parse_row(r: &[String; 4]) -> Option<Analyte> {
    let name = r[0].trim().to_string();
    if name.is_empty() { return None; }
    let na = |t: &str| t.is_empty() || t == "-" || t.eq_ignore_ascii_case("n/a") || t.eq_ignore_ascii_case("na");
    let t = r[1].trim();
    let rt = if na(t) { None } else { Some(t.parse::<f64>().ok()?) };
    let t = r[2].trim();
    let precursor = if na(t) { None } else { Some(t.parse::<f64>().ok()?) };
    let mut ions = vec![];
    for tok in r[3].split(|c: char| c == ';' || c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()) { ions.push(tok.parse::<f64>().ok()?); }
    if ions.is_empty() { return None; }
    Some(Analyte { name, rt, ions, precursor })
}

impl Default for App {
    /// Same as [`App::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Starts msQuant.
    ///
    /// Creates the default folder, removes temporary files of an interrupted run and opens the audit
    /// trail in the default folder.
    pub fn new() -> App {
        paths::ensure_default_dir();
        paths::clean_temp();
        App::new_with_audit(&paths::audit_file().to_string_lossy())
    }

    /// Creates the application with the audit trail in `audit_path` (tests use a private file).
    pub fn new_with_audit(audit_path: &str) -> App {
        let (tx, rx) = mpsc::channel();
        let mut app = App {
            analytes: vec![], samples: vec![], fits: vec![], sel_s: 0, sel_a: 0, anchor: 0,
            tol_ppm: true, tol: 10.0, rt_win: 0.3, weighting: 2, baseline: true, ratio_tol: 30.0,
            units: "ng/mL".into(), reason: String::new(), settings_dirty: false,
            out_path: String::new(), data_dir: None,
            status: "Drop a .csv target table (optional) and Thermo .raw / mzML files onto the window, or use the File menu.".into(),
            show_audit: false, show_credits: false, show_readme: false, show_targets: false,
            grab: None, editing: false, audit: Audit::new(audit_path), user_logged: "N/A".into(), tx, rx, loading: 0,
            trfp_path: String::new(),
            mode: Mode::Curve, view: View::Xic, scope: Scope::Ions, tab: Tab::Chrom, tic_level: 1, normalize: false, matrix_conc: true,
            align_on: false, align_max: 1.0, align_ref: 0,
            history: History::new(MAX_UNDO), last_undo_label: String::new(),
            rows: vec![], rows_dirty: true, table_path: None, project_path: None, pending: None,
            zoom: 1.0, title: String::new(),
            basis: Basis::Ms2, show_spectrum: true, spec_autofit: true, follow_hover: false, pinned_scan: None, hover: None,
            col_w: [0.0; 4], embed_raw: true, saving: 0, capture: None,
            show_prefs: false, reset_cal: false, zoom_chrom: None, zoom_spec: None, reset_chrom: false, reset_spec: false, spec_last: None, assign_target: 0, assign_conc: 1.0, quick_mz: 100.0, quick_level: 1,
        };
        app.load_settings();
        app
    }

    // ---------------- settings (per machine) ----------------
    /// Reads the user settings from the default folder, if there are any.
    fn load_settings(&mut self) {
        if let Ok(s) = std::fs::read_to_string(paths::settings_file()) {
            for l in s.lines() {
                if let Some(v) = l.strip_prefix("trfp=") { self.trfp_path = v.to_string(); }
                if let Some(v) = l.strip_prefix("user=") { if !v.trim().is_empty() { self.audit.user = v.trim().to_string(); self.user_logged = self.audit.user.clone(); } }
                if let Some(v) = l.strip_prefix("data_dir=") {
                    let v = v.trim();
                    self.data_dir = if v.is_empty() { None } else { Some(PathBuf::from(v)) };
                }
                if let Some(v) = l.strip_prefix("embed=") { self.embed_raw = v.trim() != "false"; }
                if let Some(v) = l.strip_prefix("zoom=") { if let Ok(z) = v.parse::<f32>() { self.zoom = z.clamp(0.5, 3.0); } }
            }
        }
    }
    /// Writes the user settings to the default folder.
    fn write_settings(&self) {
        paths::ensure_default_dir();
        let text = format!(
            "trfp={}\nuser={}\nzoom={}\nembed={}\ndata_dir={}\n",
            self.trfp_path.trim(),
            self.audit.user.trim(),
            self.zoom,
            self.embed_raw,
            self.data_dir.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        );
        let _ = std::fs::write(paths::settings_file(), text);
    }
    /// Stores the location of the converter in the settings and in the audit trail.
    fn save_trfp(&mut self) {
        self.write_settings();
        self.audit.log("CONVERTER_PATHS", &format!("trfp={}", self.trfp_path.trim()));
    }

    /// Standard folder for projects, results and figures, without creating it.
    fn standard_dir_path(&self) -> PathBuf {
        match &self.data_dir {
            Some(dir) => dir.clone(),
            None => paths::default_dir_path(),
        }
    }

    /// Standard folder for projects, results and figures, created if necessary.
    fn standard_dir(&self) -> PathBuf {
        let dir = self.standard_dir_path();
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A file dialog that starts in the standard folder.
    fn dialog(&self) -> rfd::FileDialog {
        rfd::FileDialog::new().set_directory(self.standard_dir())
    }

    /// File the analysis results are exported to: the chosen path, or `results.csv` in the standard folder.
    fn export_path(&self) -> PathBuf {
        if self.out_path.trim().is_empty() {
            self.standard_dir().join("results.csv")
        } else {
            PathBuf::from(self.out_path.trim())
        }
    }

    /// Converts the m/z tolerance setting to Da for the given m/z.
    fn tol_da(&self, mz: f64) -> f64 { if self.tol_ppm { mz * self.tol * 1e-6 } else { self.tol } }

    // ---------------- undo / redo ----------------
    /// Analysis parameters as key/value pairs, for undo snapshots and project files.
    fn params_vec(&self) -> Vec<(String, String)> {
        let kv = |k: &str, v: String| (k.to_string(), v);
        vec![
            kv("tol_ppm", self.tol_ppm.to_string()), kv("tol", self.tol.to_string()), kv("rt_win", self.rt_win.to_string()),
            kv("weighting", self.weighting.to_string()), kv("baseline", self.baseline.to_string()), kv("ratio_tol", self.ratio_tol.to_string()),
            kv("mode", if self.mode == Mode::Curve { "curve".into() } else { "areas".into() }), kv("units", self.units.clone()), kv("basis", if self.basis == Basis::Ms1 { "ms1".into() } else { "ms2".into() }),
            kv("align_on", self.align_on.to_string()), kv("align_max", self.align_max.to_string()), kv("align_ref", self.align_ref.to_string()),
        ]
    }
    /// Applies key/value pairs written by [`App::params_vec`]; unknown keys are ignored.
    fn apply_params(&mut self, kv: &[(String, String)]) {
        for (k, v) in kv {
            match k.as_str() {
                "tol_ppm" => if let Ok(x) = v.parse() { self.tol_ppm = x },
                "tol" => if let Ok(x) = v.parse() { self.tol = x },
                "rt_win" => if let Ok(x) = v.parse() { self.rt_win = x },
                "weighting" => if let Ok(x) = v.parse::<u8>() { self.weighting = x.min(2) },
                "baseline" => if let Ok(x) = v.parse() { self.baseline = x },
                "ratio_tol" => if let Ok(x) = v.parse() { self.ratio_tol = x },
                "mode" => self.mode = if v == "areas" { Mode::Areas } else { Mode::Curve },
                "units" => self.units = v.clone(),
                "basis" => self.basis = if v == "ms1" { Basis::Ms1 } else { Basis::Ms2 },
                "align_on" => if let Ok(x) = v.parse() { self.align_on = x },
                "align_max" => if let Ok(x) = v.parse() { self.align_max = x },
                "align_ref" => if let Ok(x) = v.parse() { self.align_ref = x },
                _ => {}
            }
        }
    }

    /// Captures the analysis state for undo and redo.
    fn snapshot(&self, label: &str) -> Snapshot {
        Snapshot {
            label: label.to_string(), analytes: self.analytes.clone(),
            samples: self.samples.iter().map(|s| SampleState {
                data: s.data.clone(), src_path: s.src_path.clone(), raw_sha: s.raw_sha.clone(), source: s.source.clone(), kind: s.kind,
                cells: s.cells.clone(), show: s.show, rt_shift: s.rt_shift,
            }).collect(),
            sel_s: self.sel_s, sel_a: self.sel_a, params: self.params_vec(),
        }
    }

    /// Replaces the analysis state by a snapshot and recomputes all chromatograms.
    fn restore(&mut self, s: Snapshot) {
        self.analytes = s.analytes;
        self.apply_params(&s.params);
        self.samples = s.samples.into_iter().map(|st| Sample {
            data: st.data, src_path: st.src_path, raw_sha: st.raw_sha, source: st.source, kind: st.kind, xics: vec![], cells: st.cells,
            show: st.show, rt_shift: st.rt_shift, shift_ui: st.rt_shift,
        }).collect();
        self.sel_s = s.sel_s.min(self.samples.len().saturating_sub(1));
        self.sel_a = s.sel_a.min(self.analytes.len().saturating_sub(1));
        self.anchor = self.sel_s;
        self.rows_dirty = true;
        self.editing = false;
        self.pinned_scan = None;
        self.recompute_all();
        self.fix_selection();
    }

    /// Records the current state as an undo step called `label`; call this before changing the state.
    fn push_undo(&mut self, label: &str) {
        let s = self.snapshot(label);
        self.history.record(s);
        self.last_undo_label = label.to_string();
    }

    /// Reverts the last change, if there is one.
    fn undo(&mut self) {
        let label = match self.history.peek_undo() { Some(s) => s.label.clone(), None => return };
        let cur = self.snapshot(&label);
        if let Some(prev) = self.history.undo(cur) {
            self.restore(prev);
            self.status = format!("Undid: {}", label);
            self.audit.log("UNDO", &label);
        }
    }

    /// Re-applies the last undone change, if there is one.
    fn redo(&mut self) {
        let label = match self.history.peek_redo() { Some(s) => s.label.clone(), None => return };
        let cur = self.snapshot(&label);
        if let Some(next) = self.history.redo(cur) {
            self.restore(next);
            self.status = format!("Redid: {}", label);
            self.audit.log("REDO", &label);
        }
    }

    // ---------------- selection ----------------
    /// Index of the current file; `None` when no file is selected.
    fn cur(&self) -> Option<usize> {
        if self.sel_s < self.samples.len() && self.samples[self.sel_s].show { Some(self.sel_s) } else { None }
    }
    /// Keeps the current-file index and the selection anchor valid after files were added, removed or deselected.
    fn fix_selection(&mut self) {
        let n = self.samples.len();
        if n == 0 { self.sel_s = 0; self.anchor = 0; return; }
        if self.sel_s >= n { self.sel_s = n - 1; }
        if self.anchor >= n { self.anchor = n - 1; }
        if !self.samples[self.sel_s].show {
            if let Some(k) = (0..n).rev().find(|&k| self.samples[k].show) { self.sel_s = k; }
        }
    }
    /// Indices of all selected files.
    fn shown(&self) -> Vec<usize> { (0..self.samples.len()).filter(|&i| self.samples[i].show).collect() }

    /// Explorer-style: click = only this, Ctrl+click = toggle, Shift+click = range from anchor (Ctrl+Shift adds the range).
    fn apply_click(&mut self, i: usize, ctrl: bool, shift: bool) {
        if i >= self.samples.len() { return; }
        if shift {
            let a = self.anchor.min(self.samples.len() - 1);
            let (lo, hi) = (a.min(i), a.max(i));
            if !ctrl { for s in self.samples.iter_mut() { s.show = false; } }
            for k in lo..=hi { self.samples[k].show = true; }
            self.sel_s = i;
        } else if ctrl {
            self.samples[i].show = !self.samples[i].show;
            self.anchor = i;
            if self.samples[i].show { self.sel_s = i; }
        } else {
            for s in self.samples.iter_mut() { s.show = false; }
            self.samples[i].show = true;
            self.sel_s = i;
            self.anchor = i;
        }
        self.fix_selection();
    }

    /// Removes a file from the project; the file on disk is not touched.
    fn remove_sample(&mut self, i: usize) {
        if i >= self.samples.len() { return; }
        self.push_undo("remove file");
        let s = self.samples.remove(i);
        self.pinned_scan = None;
        self.audit.log("DATA_REMOVED", &format!("{} sha256={} (file on disk untouched)", s.data.name, s.data.sha256));
        if self.align_ref > i { self.align_ref -= 1; } else if self.align_ref == i { self.align_ref = 0; }
        self.fix_selection();
        for a in 0..self.analytes.len() { self.refit(a); }
        self.status = format!("Removed {} from the project (file on disk untouched)", s.data.name);
    }

    // ---------------- loading ----------------
    /// Imports a Thermo `.raw` file in a background thread.
    ///
    /// Returns `false`, after setting a status message, if the import cannot be started.
    fn start_load(&mut self, ctx: &egui::Context, path: &str) -> bool {
        let raw = PathBuf::from(path);
        if !raw.is_file() { self.status = format!("Only single Thermo .raw files are supported / file not found: {}", path); return false; }
        let trfp = match rawconv::find_trfp(&self.trfp_path) {
            Some(p) => p,
            None => { self.show_prefs = true; self.status = "ThermoRawFileParser not found: set its location under Edit → Preferences.".into(); return false; }
        };
        self.loading += 1;
        self.status = format!("Reading {} (held in memory only)...", path);
        let (tx, ctx, p) = (self.tx.clone(), ctx.clone(), path.to_string());
        std::thread::spawn(move || {
            let res = rawconv::hash_file(&raw).and_then(|h| rawconv::convert_in_memory(&trfp, &raw).map(|c| (h, c)));
            match res {
                Ok((h, c)) => {
                    let name = raw.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    match mzml::parse_bytes(&name, &c.mzml) {
                        Ok(d) => { let _ = tx.send(Msg::Loaded(d, LoadInfo { path: p, raw_sha: h, detail: c.detail, source: RawSource::Disk(raw.clone()) })); }
                        Err(e) => { let _ = tx.send(Msg::Err(e)); }
                    }
                }
                Err(e) => { let _ = tx.send(Msg::Err(format!("{}: {}", p, e))); }
            }
            let _ = tx.send(Msg::Finished);
            ctx.request_repaint();
        });
        true
    }

    /// Re-imports a raw file that is stored inside a project: it is copied to a temporary file (ThermoRawFileParser can only read
    /// from a path), converted in memory, and the temporary copy is deleted again right away.
    fn start_load_embedded(&mut self, ctx: &egui::Context, rec_path: String, project: PathBuf, offset: u64, len: u64) -> bool {
        if rec_path.to_lowercase().ends_with(".mzml") {
            // embedded mzML: parse straight from the project file, no converter and no temporary file
            self.loading += 1;
            self.status = format!("Reading {} from the project...", rec_path);
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                match project::read_blob(&project, offset, len) {
                    Ok(bytes) => {
                        let name = Path::new(&rec_path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "embedded.mzML".into());
                        match mzml::parse_bytes(&name, &bytes) {
                            Ok(d) => {
                                let h = d.sha256.clone();
                                let _ = tx.send(Msg::Loaded(d, LoadInfo { path: rec_path, raw_sha: h, detail: "format=mzML embedded_in_project".into(), source: RawSource::Embedded { project, offset, len } }));
                            }
                            Err(e) => { let _ = tx.send(Msg::Err(e)); }
                        }
                    }
                    Err(e) => { let _ = tx.send(Msg::Err(format!("{}: {}", rec_path, e))); }
                }
                let _ = tx.send(Msg::Finished);
                ctx.request_repaint();
            });
            return true;
        }
        let trfp = match rawconv::find_trfp(&self.trfp_path) {
            Some(p) => p,
            None => { self.show_prefs = true; self.status = "ThermoRawFileParser not found: set its location under Edit → Preferences.".into(); return false; }
        };
        self.loading += 1;
        self.status = format!("Reading {} from the project...", rec_path);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let dir = paths::temp_dir().join(format!("embedded_{}_{}", std::process::id(), nanos));
            let fname = Path::new(&rec_path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "embedded.raw".into());
            let tmp = dir.join(&fname);
            let res = std::fs::create_dir_all(&dir).map_err(|e| e.to_string())
                .and_then(|_| project::extract_blob(&project, offset, len, &tmp))
                .and_then(|h| rawconv::convert_in_memory(&trfp, &tmp).map(|c| (h, c)));
            let _ = std::fs::remove_file(&tmp);
            let _ = std::fs::remove_dir(&dir);
            match res {
                Ok((h, c)) => match mzml::parse_bytes(&fname, &c.mzml) {
                    Ok(d) => {
                        let detail = format!("embedded_in_project={} temp_copy_deleted=true {}", project.display(), c.detail);
                        let _ = tx.send(Msg::Loaded(d, LoadInfo { path: rec_path, raw_sha: h, detail, source: RawSource::Embedded { project, offset, len } }));
                    }
                    Err(e) => { let _ = tx.send(Msg::Err(e)); }
                },
                Err(e) => { let _ = tx.send(Msg::Err(format!("{}: {}", rec_path, e))); }
            }
            let _ = tx.send(Msg::Finished);
            ctx.request_repaint();
        });
        true
    }

    /// Imports a dropped or selected file according to its extension: `.raw`, `.mzML`, `.quant` or a target table.
    fn load_path(&mut self, ctx: &egui::Context, path: &str) {
        let lower = path.to_lowercase();
        if lower.ends_with(".mzml") {
            self.start_load_mzml(ctx, path);
        } else if lower.ends_with(".raw") {
            self.start_load(ctx, path);
        } else if lower.ends_with(".quant") {
            self.open_project(ctx, Path::new(path));
        } else if lower.ends_with(".csv") || lower.ends_with(".tsv") || lower.ends_with(".txt") {
            self.load_table(Path::new(path));
        } else {
            self.status = "Unsupported file: drop Thermo .raw or .mzML files, a .csv target table or a .quant project.".into();
        }
    }

    /// Loads a target table and recomputes all chromatograms.
    fn load_table(&mut self, path: &Path) {
        match std::fs::read(path) {
            Ok(bytes) => match quant::parse_table(&String::from_utf8_lossy(&bytes)) {
                Ok(a) => {
                    self.push_undo("load target table");
                    self.audit.log("TABLE_LOADED", &format!("{} sha256={} analytes={}", path.display(), crate::sha256::sha256_hex(&bytes), a.len()));
                    self.analytes = a;
                    self.sel_a = 0;
                    self.table_path = Some(path.to_path_buf());
                    self.rows_dirty = true;
                    for s in self.samples.iter_mut() { s.cells.clear(); }
                    self.recompute_all();
                    self.status = format!("Loaded {} targets", self.analytes.len());
                }
                Err(e) => self.status = format!("Table error: {}", e),
            },
            Err(e) => self.status = format!("{}: {}", path.display(), e),
        }
    }

    /// Adds a freshly loaded file.
    ///
    /// While a project is being opened the stored state of the file is applied; otherwise the file is
    /// aligned (if enabled) and all targets are integrated.
    fn add_sample(&mut self, d: MzData, info: LoadInfo) {
        self.audit.log("DATA_LOADED", &format!("{} sha256={} ms2_scans={} ms1_scans={} total_scans={} source_raw={} raw_sha256={} {}",
            d.name, d.sha256, d.spectra.len(), d.tic1.len(), d.n_total, info.path, info.raw_sha, info.detail));
        let in_project = self.pending.is_some();
        if !in_project { self.push_undo("import file"); }
        let na = self.analytes.len();
        let mut sm = Sample {
            data: Arc::new(d), src_path: info.path.clone(), raw_sha: info.raw_sha.clone(), source: info.source.clone(), kind: Kind::Unknown,
            xics: vec![], cells: vec![], show: true, rt_shift: 0.0, shift_ui: 0.0,
        };
        if let Some(p) = self.pending.as_mut() {
            if let Some(rec) = p.recs.iter().find(|r| r.path == info.path) {
                sm.kind = Kind::from_label(&rec.kind);
                sm.rt_shift = rec.rt_shift;
                sm.shift_ui = rec.rt_shift;
                sm.show = rec.show;
                sm.cells = (0..na).map(|_| Cell::new()).collect();
                for c in &rec.cells {
                    if c.analyte < na {
                        let cell = &mut sm.cells[c.analyte];
                        cell.found = c.found; cell.manual = c.manual; cell.start = c.start; cell.end = c.end;
                        cell.logged = (c.start, c.end); cell.include = c.include; cell.conc = c.conc.unwrap_or(rec.conc);
                    }
                }
                if !rec.raw_sha256.is_empty() && rec.raw_sha256 != info.raw_sha { p.mismatches.push(sm.data.name.clone()); }
            }
        }
        self.samples.push(sm);
        let i = self.samples.len() - 1;
        if !in_project {
            self.sel_s = i;
            self.anchor = i;
            if self.align_on && self.align_ref < i {
                if let Some(sh) = self.compute_shift(i) {
                    self.samples[i].rt_shift = sh;
                    self.samples[i].shift_ui = sh;
                    let msg = format!("file={} sha256={} reference={} shift_min={:.4} max_search_min={}",
                        self.samples[i].data.name, self.samples[i].data.sha256, self.samples[self.align_ref].data.name, sh, self.align_max);
                    self.audit.log("RT_ALIGNMENT", &msg);
                }
            }
        }
        self.recompute_sample(i);
        for a in 0..self.analytes.len() { self.refit(a); }
        self.status = format!("Loaded {}", self.samples[i].data.name);
    }

    // ---------------- computation ----------------
    /// Extracts the chromatograms of all targets from file `s`, detects the peaks (except where
    /// boundaries were set by hand) and integrates them.
    fn recompute_sample(&mut self, s: usize) {
        let (na, rt_win, baseline) = (self.analytes.len(), self.rt_win, self.baseline);
        let basis = self.basis;
        let traces: Vec<Vec<f64>> = (0..na).map(|a| self.trace_mz(a)).collect();
        let tols: Vec<Vec<f64>> = traces.iter().map(|v| v.iter().map(|&m| self.tol_da(m)).collect()).collect();
        let sm = &mut self.samples[s];
        let shift = sm.rt_shift;
        sm.xics = traces.iter().enumerate()
            .map(|(ai, mzs)| mzs.iter().enumerate().map(|(ii, &mz)| {
                let sp: &[Spectrum] = if basis == Basis::Ms1 { &sm.data.ms1 } else { &sm.data.spectra };
                let mut x = quant::xic(sp, mz, tols[ai][ii]);
                if shift != 0.0 { for p in x.iter_mut() { p[0] += shift; } }
                x
            }).collect()).collect();
        if sm.cells.len() != na { sm.cells = (0..na).map(|_| Cell::new()).collect(); }
        for a in 0..na {
            let c = &mut sm.cells[a];
            if !c.manual {
                match quant::auto_peak(&sm.xics[a][0], self.analytes[a].rt, rt_win) {
                    Some((st, en)) => { c.start = st; c.end = en; c.found = true; c.logged = (st, en); }
                    None => c.found = false,
                }
            }
            calc_areas(&sm.xics[a], c, baseline);
        }
    }

    /// Recompute a single (new or edited) target in every file; its integrations are reset.
    fn recompute_analyte(&mut self, a: usize) {
        let na = self.analytes.len();
        if a >= na { return; }
        let (rt_win, baseline) = (self.rt_win, self.baseline);
        let basis = self.basis;
        let mzs = self.trace_mz(a);
        let tols: Vec<f64> = mzs.iter().map(|&m| self.tol_da(m)).collect();
        for s in 0..self.samples.len() {
            let sm = &mut self.samples[s];
            if sm.xics.len() < na { sm.xics.resize(na, vec![]); }
            if sm.cells.len() < na { sm.cells.resize_with(na, Cell::new); }
            let shift = sm.rt_shift;
            sm.xics[a] = mzs.iter().enumerate().map(|(ii, &mz)| {
                let sp: &[Spectrum] = if basis == Basis::Ms1 { &sm.data.ms1 } else { &sm.data.spectra };
                let mut x = quant::xic(sp, mz, tols[ii]);
                if shift != 0.0 { for p in x.iter_mut() { p[0] += shift; } }
                x
            }).collect();
            let mut c = Cell::new();
            if let Some((st, en)) = quant::auto_peak(&sm.xics[a][0], self.analytes[a].rt, rt_win) {
                c.start = st; c.end = en; c.found = true; c.logged = (st, en);
            }
            calc_areas(&sm.xics[a], &mut c, baseline);
            sm.cells[a] = c;
        }
        if self.fits.len() < na { self.fits.resize(na, None); }
        self.refit(a);
    }

    /// Recomputes every file and refits all calibrations.
    fn recompute_all(&mut self) {
        self.fits = vec![None; self.analytes.len()];
        for s in 0..self.samples.len() { self.recompute_sample(s); }
        for a in 0..self.analytes.len() { self.refit(a); }
    }

    /// Refits the calibration of target `a` from its included standards.
    fn refit(&mut self, a: usize) {
        if a >= self.analytes.len() { return; }
        if self.fits.len() != self.analytes.len() { self.fits = vec![None; self.analytes.len()]; }
        let pts: Vec<(f64, f64)> = self.samples.iter()
            .filter(|s| s.kind == Kind::Standard && s.cells.len() > a && s.cells[a].conc > 0.0 && s.cells[a].found && s.cells[a].include)
            .map(|s| (s.cells[a].conc, s.cells[a].areas[0])).collect();
        self.fits[a] = quant::fit(&pts, self.weighting);
    }

    /// Applies hand-set boundaries to a cell; one editing gesture is one undo step.
    fn apply_edit(&mut self, s: usize, a: usize, st: f64, en: f64) {
        if !self.editing { self.push_undo("manual integration"); self.editing = true; }
        let baseline = self.baseline;
        let sm = &mut self.samples[s];
        let c = &mut sm.cells[a];
        c.start = st; c.end = en; c.found = true; c.manual = true;
        calc_areas(&sm.xics[a], c, baseline);
        self.refit(a);
    }

    /// Ends a boundary edit and writes the change, with old and new values, to the audit trail.
    fn commit(&mut self, s: usize, a: usize) {
        self.editing = false;
        let (st, en, area, logged) = {
            let c = &self.samples[s].cells[a];
            (c.start, c.end, c.areas.first().copied().unwrap_or(0.0), c.logged)
        };
        if logged != (st, en) {
            let msg = format!("file={} sha256={} analyte={} start {:.4}->{:.4} end {:.4}->{:.4} area={:.1} reason={}",
                self.samples[s].data.name, self.samples[s].data.sha256, self.analytes[a].name,
                logged.0, st, logged.1, en, area,
                if self.reason.trim().is_empty() { "NONE GIVEN" } else { self.reason.trim() });
            self.samples[s].cells[a].logged = (st, en);
            self.audit.log("MANUAL_INTEGRATION", &msg);
        }
    }

    /// Applies changed extraction settings (tolerance, RT window, weighting, baseline) to all files.
    fn apply_settings(&mut self) {
        self.audit.log("PARAMETERS", &format!("tol={}{} rt_window={} weighting={} baseline={} ion_ratio_tol={}%",
            self.tol, if self.tol_ppm { "ppm" } else { "Da" }, self.rt_win, WEIGHTS[self.weighting as usize], self.baseline, self.ratio_tol));
        self.recompute_all();
        self.settings_dirty = false;
    }

    // ---------------- retention-time alignment ----------------
    /// Retention-time shift that aligns file `s` to the reference file; `None` if one of them has too few scans.
    fn compute_shift(&self, s: usize) -> Option<f64> {
        let r = self.samples.get(self.align_ref)?;
        let x = &self.samples[s];
        let (tr, tx) = (align_trace(&r.data), align_trace(&x.data));
        if tr.len() < 5 || tx.len() < 5 { return None; }
        Some(quant::estimate_shift(tr, tx, self.align_max) + r.rt_shift)
    }

    /// Sets the retention-time shift of file `s`.
    ///
    /// Its manual integrations are reset because they refer to the shifted times.
    fn set_shift(&mut self, s: usize, shift: f64, why: &str) {
        let old = self.samples[s].rt_shift;
        {
            let sm = &mut self.samples[s];
            sm.rt_shift = shift;
            sm.shift_ui = shift;
            for c in sm.cells.iter_mut() { c.manual = false; }
        }
        self.recompute_sample(s);
        for a in 0..self.analytes.len() { self.refit(a); }
        let msg = format!("file={} sha256={} shift {:.4}->{:.4} min reason={} (manual integrations of this file reset)",
            self.samples[s].data.name, self.samples[s].data.sha256, old, shift, why);
        self.audit.log("RT_SHIFT", &msg);
    }

    /// Aligns every file to the reference file.
    fn align_all(&mut self) {
        let n = self.samples.len();
        if n < 2 { self.status = "Need at least two files to align.".into(); return; }
        if self.align_ref >= n { self.align_ref = 0; }
        self.push_undo("align all");
        for s in 0..n {
            if s == self.align_ref { continue; }
            if let Some(sh) = self.compute_shift(s) { self.set_shift(s, sh, "align all (TIC cross-correlation)"); }
        }
        self.status = format!("Aligned {} files to {}", n - 1, self.samples[self.align_ref].data.name);
    }

    /// Removes all retention-time shifts.
    fn clear_alignment(&mut self) {
        self.push_undo("clear alignment");
        for s in 0..self.samples.len() { if self.samples[s].rt_shift != 0.0 { self.set_shift(s, 0.0, "clear alignment"); } }
        self.status = "Alignment cleared".into();
    }

    // ---------------- results ----------------
    /// Concentration of target `a` in file `s` according to the calibration.
    ///
    /// Returns `None` in areas-only mode, without calibration and without a peak.
    fn conc_of(&self, s: usize, a: usize) -> Option<f64> {
        if self.mode == Mode::Areas { return None; }
        let f = self.fits.get(a).copied().flatten()?;
        let c = self.samples.get(s)?.cells.get(a)?;
        if !c.found || f.m == 0.0 { return None; }
        Some((c.areas[0] - f.b) / f.m)
    }

    /// Mean ratio of qualifier ion `ion` to the quantifier ion over the included standards of target `a`.
    fn ref_ratio(&self, a: usize, ion: usize) -> Option<f64> {
        let v: Vec<f64> = self.samples.iter()
            .filter(|s| s.kind == Kind::Standard && s.cells.len() > a && s.cells[a].conc > 0.0 && s.cells[a].found && s.cells[a].include && s.cells[a].areas[0] > 0.0)
            .map(|s| s.cells[a].areas[ion] / s.cells[a].areas[0]).collect();
        if v.is_empty() { None } else { Some(v.iter().sum::<f64>() / v.len() as f64) }
    }

    /// Quality flags of one result (not detected, manual, ion-ratio failures, calibration range, accuracy) as text.
    fn flags(&self, s: usize, a: usize) -> String {
        let sm = &self.samples[s];
        let c = match sm.cells.get(a) { Some(c) => c, None => return String::new() };
        let mut f: Vec<String> = vec![];
        if !c.found || c.areas[0] <= 0.0 { return "ND".into(); }
        if c.manual { f.push("manual".into()); }
        if sm.kind == Kind::Blank { f.push("PEAK IN BLANK".into()); }
        if let (Some(fit), Some(cc)) = (self.fits.get(a).copied().flatten(), self.conc_of(s, a)) {
            if sm.kind != Kind::Standard && (cc < fit.lo || cc > fit.hi) { f.push("outside cal range".into()); }
            if sm.kind == Kind::Standard && c.conc > 0.0 && ((cc - c.conc) / c.conc).abs() > 0.15 { f.push("accuracy >15%".into()); }
        }
        for i in 1..c.areas.len() {
            if let Some(r) = self.ref_ratio(a, i) {
                let obs = c.areas[i] / c.areas[0];
                if r > 0.0 && ((obs - r) / r).abs() * 100.0 > self.ratio_tol { f.push(format!("ion{} ratio fail", i + 1)); }
            }
        }
        f.join("; ")
    }

    /// Writes the results table, including the calibration parameters, as CSV.
    fn export(&mut self) -> Result<(), String> {
        let curve = self.mode == Mode::Curve;
        let mut o = String::new();
        o += &format!("# msQuant v{} export {} user={}\n", VERSION, audit::iso_now(), self.audit.user);
        o += &format!("# quantification={}\n", if curve { "standard curve" } else { "integrated areas only (arbitrary units)" });
        o += &format!("# quantified_signal={}\n", if self.basis == Basis::Ms1 { "MS1 precursor area" } else { "MS2 product-ion area" });
        o += &format!("# tol={}{} rt_window_min={} weighting={} baseline_subtraction={} ion_ratio_tol_pct={} conc_units={}\n",
            self.tol, if self.tol_ppm { "ppm" } else { "Da" }, self.rt_win, WEIGHTS[self.weighting as usize], self.baseline, self.ratio_tol, self.units);
        o += &format!("# rt_alignment_on_import={} max_shift_min={} reference={}\n", self.align_on, self.align_max,
            self.samples.get(self.align_ref).map(|s| s.data.name.clone()).unwrap_or_default());
        if curve {
            for (a, an) in self.analytes.iter().enumerate() {
                match self.fits.get(a).copied().flatten() {
                    Some(f) => o += &format!("# CAL {}: area = {:.6e}*conc + {:.6e}; R2={:.5}; n={}; range {}-{}\n", an.name, f.m, f.b, f.r2, f.n, f.lo, f.hi),
                    None => o += &format!("# CAL {}: no valid calibration\n", an.name),
                }
            }
        }
        o += "file,sha256,type,level,rt_shift_min,analyte,start_min,end_min,area_quant_(int*s),manual,ion_ratios,calc_conc,flags\n";
        for s in 0..self.samples.len() {
            for a in 0..self.analytes.len() {
                let sm = &self.samples[s];
                let c = &sm.cells[a];
                let ratios: Vec<String> = (1..c.areas.len()).map(|i| if c.areas[0] > 0.0 { format!("{:.3}", c.areas[i] / c.areas[0]) } else { "n/a".into() }).collect();
                let conc = self.conc_of(s, a).map(|v| format!("{:.4}", v)).unwrap_or("n/a".into());
                o += &format!("{},{},{},{},{:.4},{},{:.4},{:.4},{:.1},{},{},{},{}\n", csv(&sm.data.name), sm.data.sha256, sm.kind.label(),
                    if sm.kind == Kind::Standard && curve && c.conc > 0.0 { c.conc.to_string() } else { String::new() }, sm.rt_shift, csv(&self.analytes[a].name),
                    c.start, c.end, c.areas[0], c.manual, ratios.join(" "), conc, csv(&self.flags(s, a)));
            }
        }
        let path = self.export_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, o).map_err(|e| e.to_string())?;
        self.audit.log("EXPORT", &path.display().to_string());
        Ok(())
    }

    /// Lets the user pick the results file in a file dialog.
    fn choose_out_path(&mut self) {
        let current = self.export_path();
        let mut d = self.dialog().set_title("Select analysis output file").add_filter("CSV", &["csv"]);
        if let Some(dir) = current.parent() {
            if dir.is_dir() { d = d.set_directory(dir); }
        }
        d = d.set_file_name(current.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "results.csv".into()));
        if let Some(path) = d.save_file() { self.out_path = path.to_string_lossy().to_string(); }
    }

    // ---------------- target table editing ----------------
    /// Rebuilds the text rows of the target editor from the targets.
    fn sync_rows(&mut self) {
        self.rows = self.analytes.iter().map(|a| [
            a.name.clone(),
            a.rt.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
            a.precursor.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
            a.ions.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("; "),
        ]).collect();
        self.rows_dirty = false;
    }

    /// Applies an edited row of the target editor to its target and re-integrates it.
    fn apply_row(&mut self, i: usize) {
        if i >= self.rows.len() || i >= self.analytes.len() { return; }
        if let Some(an) = parse_row(&self.rows[i]) {
            if an != self.analytes[i] {
                self.push_undo("edit target");
                self.audit.log("TARGET_EDIT", &format!("#{} {} -> {} rt={:?} ions={:?}", i + 1, self.analytes[i].name, an.name, an.rt, an.ions));
                self.analytes[i] = an;
                self.recompute_analyte(i);
            }
        }
    }

    /// Appends a placeholder target.
    fn add_target(&mut self) {
        self.push_undo("add target");
        let n = self.analytes.len();
        self.analytes.push(Analyte { name: format!("Target {}", n + 1), rt: None, ions: vec![100.0], precursor: None });
        self.fits.push(None);
        self.recompute_analyte(n);
        self.rows_dirty = true;
        self.audit.log("TARGET_ADDED", &format!("#{}", n + 1));
    }

    /// Deletes a target and its results from all files.
    fn delete_target(&mut self, i: usize) {
        if i >= self.analytes.len() { return; }
        self.push_undo("delete target");
        let name = self.analytes.remove(i).name;
        for sm in self.samples.iter_mut() {
            if i < sm.xics.len() { sm.xics.remove(i); }
            if i < sm.cells.len() { sm.cells.remove(i); }
        }
        if i < self.fits.len() { self.fits.remove(i); }
        self.sel_a = self.sel_a.min(self.analytes.len().saturating_sub(1));
        self.rows_dirty = true;
        self.audit.log("TARGET_DELETED", &name);
    }

    /// The target table as CSV text.
    fn targets_csv(&self) -> String {
        let mut o = String::from("name,rt_min,ions\n");
        for a in &self.analytes {
            let mut ions = a.ions.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(";");
            if let Some(p) = a.precursor { ions += &format!(";p:{}", p); }
            o += &format!("{},{},{}\n", csv(&a.name), a.rt.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()), ions);
        }
        o
    }

    /// Writes the target table to `path` and remembers the path.
    fn write_targets(&mut self, path: &Path) {
        let t = self.targets_csv();
        match std::fs::write(path, &t) {
            Ok(_) => {
                self.audit.log("TARGETS_SAVED", &format!("{} sha256={}", path.display(), crate::sha256::sha256_hex(t.as_bytes())));
                self.table_path = Some(path.to_path_buf());
                self.status = format!("Saved target table {}", path.display());
            }
            Err(e) => self.status = format!("Save failed: {}", e),
        }
    }

    /// Saves the target table to a path chosen in a file dialog.
    fn save_targets_as(&mut self) {
        let mut d = self.dialog().set_title("Save target table").add_filter("CSV", &["csv"]).set_file_name("targets.csv");
        if let Some(p) = &self.table_path {
            if let Some(dir) = p.parent() { d = d.set_directory(dir); }
            if let Some(n) = p.file_name() { d = d.set_file_name(n.to_string_lossy().to_string()); }
        }
        if let Some(path) = d.save_file() { self.write_targets(&path); }
    }

    /// Loads a target table chosen in a file dialog.
    fn open_targets_dialog(&mut self) {
        if let Some(p) = self.dialog().set_title("Open target table").add_filter("Target table", &["csv", "tsv", "txt"]).pick_file() { self.load_table(&p); }
    }

    // ---------------- projects ----------------
    /// Collects everything a project stores (all of it except the raw data, which is copied separately).
    fn project_file(&self) -> ProjectFile {
        ProjectFile {
            app_version: VERSION.into(), user: self.audit.user.clone(), params: self.params_vec(), analytes: self.analytes.clone(),
            samples: self.samples.iter().map(|s| SampleRec {
                path: s.src_path.clone(), raw_sha256: s.raw_sha.clone(), kind: s.kind.label().into(), conc: 0.0, rt_shift: s.rt_shift, show: s.show, embedded: false,
                cells: s.cells.iter().enumerate().map(|(i, c)| CellRec { analyte: i, found: c.found, manual: c.manual, start: c.start, end: c.end, include: c.include, conc: Some(c.conc) }).collect(),
            }).collect(),
        }
    }

    /// Saves the project in a background thread; with `force_dialog` it always asks for a path.
    fn save_project(&mut self, ctx: &egui::Context, force_dialog: bool) {
        let path = match (&self.project_path, force_dialog) {
            (Some(p), false) => p.clone(),
            _ => match self.dialog().set_title("Save project").add_filter("msQuant project", &["quant"]).set_file_name("project.quant").save_file() {
                Some(p) => if p.extension().is_none() { p.with_extension("quant") } else { p },
                None => return,
            },
        };
        let pf = self.project_file();
        let mut blobs: Vec<Option<BlobSource>> = vec![];
        let mut missing: Vec<String> = vec![];
        for s in &self.samples {
            let b = if !self.embed_raw { None } else {
                match &s.source {
                    RawSource::Disk(p) if p.is_file() => Some(BlobSource::File(p.clone())),
                    RawSource::Embedded { project, offset, len } if project.is_file() => Some(BlobSource::Range { path: project.clone(), offset: *offset, len: *len }),
                    _ => { missing.push(s.data.name.clone()); None }
                }
            };
            blobs.push(b);
        }
        let src_paths: Vec<String> = self.samples.iter().map(|s| s.src_path.clone()).collect();
        self.saving += 1;
        self.status = format!("Saving project {} ...", path.display());
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let tmp_dir = paths::temp_dir();
        std::thread::spawn(move || {
            let result = project::write_container(&path, &pf, &blobs, &tmp_dir);
            let _ = tx.send(Msg::ProjectSaved { path, result, src_paths, missing });
            ctx.request_repaint();
        });
    }

    /// Opens a project chosen in a file dialog.
    fn open_project_dialog(&mut self, ctx: &egui::Context) {
        if let Some(p) = self.dialog().set_title("Open project").add_filter("msQuant project", &["quant"]).pick_file() { self.open_project(ctx, &p); }
    }

    /// Opens a project: restores targets and settings, then re-imports the files in background threads.
    fn open_project(&mut self, ctx: &egui::Context, path: &Path) {
        let (pf, offs) = match project::read_container(path) { Ok(x) => x, Err(e) => { self.status = format!("Project error: {}", e); return; } };
        self.push_undo("open project");
        self.audit.log("PROJECT_OPENED", &format!("{} text_sha256={} saved_by={} embedded_raw_files={}", path.display(),
            crate::sha256::sha256_hex(pf.to_text().as_bytes()), pf.user, offs.iter().filter(|o| o.is_some()).count()));
        self.samples.clear();
        self.analytes = pf.analytes.clone();
        self.fits = vec![None; self.analytes.len()];
        self.apply_params(&pf.params);
        self.sel_s = 0; self.sel_a = 0; self.anchor = 0;
        self.rows_dirty = true;
        self.project_path = Some(path.to_path_buf());
        if pf.samples.is_empty() { self.status = format!("Opened project {}", path.display()); return; }
        self.pending = Some(Pending { recs: pf.samples.clone(), mismatches: vec![] });
        let mut started = 0;
        for (rec, off) in pf.samples.iter().zip(offs.iter()) {
            let ok = match off {
                Some((o, l)) => self.start_load_embedded(ctx, rec.path.clone(), path.to_path_buf(), *o, *l),
                None => self.start_load_any(ctx, &rec.path),
            };
            if ok { started += 1; }
        }
        if started == 0 { self.pending = None; }
        else { self.status = format!("Opening project: re-importing {} raw file(s)...", started); }
    }

    /// Completes opening a project after all file loads have finished: restores the file order and reports problems.
    fn finish_project(&mut self, p: Pending) {
        let recs = &p.recs;
        self.samples.sort_by_key(|s| recs.iter().position(|r| r.path == s.src_path).unwrap_or(usize::MAX));
        let n = self.samples.len();
        self.fix_selection();
        for a in 0..self.analytes.len() { self.refit(a); }
        if self.align_ref >= n { self.align_ref = 0; }
        let mut msg = format!("Project loaded: {} of {} raw files", n, recs.len());
        if !p.mismatches.is_empty() {
            msg += &format!("  ⚠ SHA-256 differs from the saved project for: {}", p.mismatches.join(", "));
            self.audit.log("PROJECT_HASH_MISMATCH", &p.mismatches.join(", "));
        }
        if n < recs.len() { msg += "  ⚠ some files could not be re-imported (moved or converter missing)"; }
        self.status = msg;
    }

    /// Clears the workspace; the step can be undone.
    fn new_project(&mut self) {
        self.push_undo("new project");
        self.audit.log("PROJECT_NEW", "");
        self.samples.clear();
        self.analytes.clear();
        self.fits.clear();
        self.sel_s = 0; self.sel_a = 0; self.anchor = 0;
        self.project_path = None;
        self.table_path = None;
        self.pending = None;
        self.rows_dirty = true;
        self.status = "New project".into();
    }

    // ---------------- UI ----------------
    /// Selected target, all traces, in file `s`, with draggable integration boundaries and hover read-out.
    fn ui_plot(&mut self, ui: &mut egui::Ui, s: usize) {
        let a = self.sel_a;
        let mut grab = self.grab;
        let mut edit: Option<(f64, f64)> = None;
        let mut stopped = false;
        let clicked;
        let mut hover: Option<Hover> = None;
        let mut ex: Option<ExportKind> = None;
        let mut export_req: Option<(ExportKind, SvgPlot, egui::Rect, String)> = None;
        let mut csv_req: Option<(String, String)> = None;
        let tmz = self.trace_mz(a);
        let capturing = self.capture.is_some();
        let reset_now = std::mem::take(&mut self.reset_chrom);
        let alt = ui.input(|i| i.modifiers.alt);
        let mut zoom_c = self.zoom_chrom;
        let mut reset_req = false;
        {
            let sm = &self.samples[s];
            let c = &sm.cells[a];
            let norm = self.normalize;
            let level: u8 = if self.basis == Basis::Ms1 { 1 } else { 2 };
            let series: Vec<Series> = sm.xics[a].iter().enumerate().map(|(i, x)| {
                let mx = x.iter().fold(0.0f64, |m, p| m.max(p[1]));
                let d = format!("m/z {}", fmt::mz4(tmz[i]));
                Series { name: d.clone(), data: x.clone(), ci: i, cur: false, file: s, level, fname: sm.data.name.clone(),
                         desc: format!("{} · {}", self.analytes[a].name, d), scale: if norm && mx > 0.0 { 100.0 / mx } else { 1.0 } }
            }).collect();
            let scan_of = |f: usize, l: u8, j: usize| -> String { scan_info(&self.samples[f].data, l, j) };
            let mut plot = rtplot!(Plot::new("xic").height(300.0).allow_drag(false).allow_boxed_zoom(false).allow_scroll(!alt)
                .legend(Legend::default()).x_axis_label("Retention time (min)  |  drag a box = zoom · grab a boundary line to move it · click = pin the scan")
                .y_axis_label(if norm { "Intensity (% of maximum)" } else { "Intensity" }), norm);
            if reset_now { plot = plot.reset(); }
            let pr = plot
                .show(ui, |pu| {
                    draw_series!(pu, &series, hover, scan_of, capturing);
                    if c.found && !norm {
                        pu.polygon(Polygon::new(PlotPoints::from(quant::shade(&sm.xics[a][0], c.start, c.end)))
                            .fill_color(Color32::from_rgba_unmultiplied(31, 119, 180, 60)).name("integrated"));
                    }
                    if c.found {
                        pu.vline(VLine::new(c.start).color(Color32::DARK_GREEN));
                        pu.vline(VLine::new(c.end).color(Color32::DARK_RED));
                    }
                    let r = pu.response().clone();
                    let bnd = pu.plot_bounds();
                    let pxw = r.rect.width() as f64 / (bnd.max()[0] - bnd.min()[0]).max(1e-12);
                    if let Some(p) = pu.pointer_coordinate() {
                        // a boundary line is only moved when it is grabbed (within 8 px); anywhere else a drag draws the zoom window
                        let near = if c.found && in_view(&p, &bnd) { nearest_boundary(p.x, c.start, c.end, pxw, 8.0) } else { None };
                        if near.is_some() && r.hovered() { r.ctx.set_cursor_icon(egui::CursorIcon::ResizeHorizontal); }
                        if r.drag_started_by(PointerButton::Primary) { grab = near; }
                        if r.dragged_by(PointerButton::Primary) {
                            if let Some(boundary) = grab {
                                let (mut st, mut en) = (c.start, c.end);
                                match boundary {
                                    Boundary::Start => st = p.x.min(en - 1e-4),
                                    Boundary::End => en = p.x.max(st + 1e-4),
                                }
                                edit = Some((st, en));
                            }
                        }
                    }
                    let _ = zoom_pan!(pu, r, zoom_c, grab.is_some());
                    if r.drag_stopped() { stopped = true; }
                });
            clicked = pr.response.clicked() && !capturing;
            let rect = pr.response.rect;
            let (x0, x1, y0, y1) = { let b = pr.transform.bounds(); (b.min()[0], b.max()[0], b.min()[1], b.max()[1]) };
            let names: Vec<String> = series.iter().map(|s| s.name.clone()).collect();
            export_menu!(pr.response, "chromatogram", ex, &names);
            if let Some(k) = ex {
                let (file, target) = (safe_name(&sm.data.name), safe_name(&self.analytes[a].name));
                match k {
                    ExportKind::Reset => { reset_req = true; }
                    ExportKind::Csv(i) => {
                        if let Some(sr) = series.get(i) {
                            csv_req = Some((format!("XIC_{}_mz{}_{}", target, fmt::mz4(tmz[i]), file),
                                            csv_two("RT (min)", "Intensity", sr.data.iter().map(|p| (p[0], p[1])))));
                        }
                    }
                    _ => {
                        let mut vl = vec![];
                        if c.found {
                            vl.push(SvgVLine { x: c.start, rgb: (0, 100, 0), label: "integration start".into() });
                            vl.push(SvgVLine { x: c.end, rgb: (139, 0, 0), label: "integration end".into() });
                        }
                        let yf: fn(f64) -> String = if norm { fmt::pct0 } else { fmt::sci3 };
                        export_req = Some((k, SvgPlot {
                            title: format!("{} · {}", sm.data.name, self.analytes[a].name),
                            x_label: "Retention time (min)".into(),
                            y_label: if norm { "Intensity (% of maximum)".into() } else { "Intensity".into() },
                            series: svg_series(&series), vlines: vl, labels: vec![], notes: vec![], bounds: [x0, x1, y0, y1], x_fmt: fmt::rt2, y_fmt: yf,
                        }, rect, format!("XIC_{}_{}", target, file)));
                    }
                }
            }
        }
        if let Some(h) = &hover {
            self.hover = Some((h.file, h.rt));
            if clicked { self.pin_scan(h.file, h.rt); }
        }
        self.zoom_chrom = zoom_c;
        if reset_req { self.reset_chrom = true; ui.ctx().request_repaint(); }
        if let Some((k, plot, rect, stem)) = export_req { self.do_export(k, plot, rect, &stem); }
        if let Some((stem, text)) = csv_req { self.save_csv(&stem, &text); }
        if let Some((st, en)) = edit { self.apply_edit(s, a, st, en); }
        if stopped { self.commit(s, a); grab = None; }
        self.grab = grab;

        let (found, mut st, mut en) = { let c = &self.samples[s].cells[a]; (c.found, c.start, c.end) };
        let mut changed = false; let mut done = false;
        ui.horizontal(|ui| {
            if found {
                ui.label("Start (min)");
                let r1 = ui.add(dv_rt(&mut st).speed(0.01));
                ui.label("End (min)");
                let r2 = ui.add(dv_rt(&mut en).speed(0.01));
                changed = r1.changed() || r2.changed();
                done = r1.drag_stopped() || r1.lost_focus() || r2.drag_stopped() || r2.lost_focus();
            } else if ui.button("No peak found - define manually").clicked() {
                let x = &self.samples[s].xics[a][0];
                let mid = self.analytes[a].rt.unwrap_or_else(|| x.get(x.len() / 2).map(|p| p[0]).unwrap_or(1.0));
                st = mid - 0.1; en = mid + 0.1; changed = true; done = true;
            }
            ui.label("Reason for manual change:");
            ui.add(egui::TextEdit::singleline(&mut self.reason).desired_width(260.0));
        });
        if changed && st < en { self.apply_edit(s, a, st, en); }
        if done { self.commit(s, a); }
        if found && ui.button("Reset to automatic boundaries").clicked() {
            self.push_undo("reset integration");
            self.audit.log("MANUAL_RESET", &format!("file={} analyte={}", self.samples[s].data.name, self.analytes[a].name));
            self.samples[s].cells[a].manual = false;
            self.recompute_sample(s);
            self.refit(a);
        }
        let c = &self.samples[s].cells[a];
        if c.found {
            let mut t = String::new();
            for (i, ar) in c.areas.iter().enumerate() {
                t += &format!("{} {} (m/z {}): area {}   ", if self.basis == Basis::Ms1 { "precursor" } else { "ion" }, i + 1, fmt::mz4(tmz[i]), fmt::sci(*ar));
                if i > 0 && c.areas[0] > 0.0 { t += &format!("[ratio {}]   ", fmt::sci(ar / c.areas[0])); }
            }
            ui.monospace(t);
            match self.conc_of(s, a) {
                Some(v) => ui.strong(format!("Calculated: {} {}   {}", fmt::sci(v), self.units, self.flags(s, a))),
                None => if self.mode == Mode::Curve {
                    ui.label("No calibration available for this analyte")
                } else {
                    ui.strong(format!("Integrated area: {} a.u. (intensity·s)   {}", fmt::sci(c.areas[0]), self.flags(s, a)))
                },
            };
        }
    }

    /// m/z chromatogram of the "custom m/z" view for file `f` (aligned retention times).
    fn quick_trace(&self, f: usize) -> Vec<[f64; 2]> {
        let sm = &self.samples[f];
        let sp: &[Spectrum] = if self.quick_level == 1 { &sm.data.ms1 } else { &sm.data.spectra };
        let mut x = quant::xic(sp, self.quick_mz, self.tol_da(self.quick_mz));
        if sm.rt_shift != 0.0 { for p in x.iter_mut() { p[0] += sm.rt_shift; } }
        x
    }

    /// Read-only overlays: all targets of the current file, one target / a custom m/z across selected files, or TIC/BPC of selected files.
    fn ui_overlay(&mut self, ui: &mut egui::Ui, cur: Option<usize>) {
        let a = self.sel_a;
        let norm = self.normalize;
        let shown = self.shown();
        let level: u8 = if self.basis == Basis::Ms1 { 1 } else { 2 };
        let mut series: Vec<Series> = vec![];
        match (self.view, self.scope) {
            (View::Xic, Scope::AllTargets) => {
                if let Some(s) = cur {
                    let sm = &self.samples[s];
                    for (ai, an) in self.analytes.iter().enumerate() {
                        if let Some(x) = sm.xics.get(ai).and_then(|v| v.first()) {
                            let desc = format!("{} · m/z {}", an.name, fmt::mz4(self.trace_mz(ai)[0]));
                            series.push(Series { name: an.name.clone(), data: x.clone(), ci: ai, cur: false, file: s, level, fname: sm.data.name.clone(), desc, scale: 1.0 });
                        }
                    }
                }
            }
            (View::Quick, _) => {
                let desc = format!("custom m/z {} (MS{})", fmt::mz4(self.quick_mz), self.quick_level);
                for &f in &shown {
                    let sm = &self.samples[f];
                    series.push(Series { name: sm.data.name.clone(), data: self.quick_trace(f), ci: f, cur: Some(f) == cur, file: f,
                                         level: self.quick_level, fname: sm.data.name.clone(), desc: desc.clone(), scale: 1.0 });
                }
            }
            (View::Xic, _) => {
                let desc = format!("{} · m/z {}", self.analytes[a].name, fmt::mz4(self.trace_mz(a)[0]));
                for &f in &shown {
                    let sm = &self.samples[f];
                    if let Some(x) = sm.xics.get(a).and_then(|v| v.first()) {
                        series.push(Series { name: sm.data.name.clone(), data: x.clone(), ci: f, cur: Some(f) == cur, file: f, level, fname: sm.data.name.clone(), desc: desc.clone(), scale: 1.0 });
                    }
                }
            }
            (v, _) => {
                let bpc = v == View::Bpc;
                for &f in &shown {
                    let sm = &self.samples[f];
                    let sh = sm.rt_shift;
                    let d: Vec<[f64; 2]> = chrom(
                        &sm.data,
                        self.tic_level,
                        if bpc { SummaryTrace::BasePeak } else { SummaryTrace::TotalIon },
                    ).iter().map(|p| [p[0] + sh, p[1]]).collect();
                    series.push(Series { name: sm.data.name.clone(), data: d, ci: f, cur: Some(f) == cur, file: f, level: self.tic_level, fname: sm.data.name.clone(),
                                         desc: format!("{} (MS{})", if bpc { "BPC" } else { "TIC" }, self.tic_level), scale: 1.0 });
                }
            }
        }
        series.retain(|x| !x.data.is_empty());
        if shown.is_empty() { ui.label("No file selected."); return; }
        if series.is_empty() {
            ui.label(if self.view == View::Xic { "No chromatogram data." } else { "The selected file(s) have no scans at this MS level." });
            return;
        }
        if norm {
            for sr in series.iter_mut() {
                let m = sr.data.iter().fold(0.0f64, |m, p| m.max(p[1]));
                if m > 0.0 { sr.scale = 100.0 / m; }
            }
        }
        let rt_hint = if self.view == View::Xic && !self.analytes.is_empty() { self.analytes[a].rt } else { None };
        let id = (self.view as u8, self.scope as u8, self.tic_level);
        let mut hover: Option<Hover> = None;
        let clicked;
        let mut ex: Option<ExportKind> = None;
        let mut export_req: Option<(ExportKind, SvgPlot, egui::Rect, String)> = None;
        let mut csv_req: Option<(String, String)> = None;
        let capturing = self.capture.is_some();
        let reset_now = std::mem::take(&mut self.reset_chrom);
        let alt = ui.input(|i| i.modifiers.alt);
        let mut zoom_c = self.zoom_chrom;
        let mut reset_req = false;
        {
            let scan_of = |f: usize, l: u8, j: usize| -> String { scan_info(&self.samples[f].data, l, j) };
            let mut plot = rtplot!(Plot::new(id).height(380.0).allow_drag(false).allow_boxed_zoom(false).allow_scroll(!alt).legend(Legend::default())
                .x_axis_label("Retention time (min)  |  drag a box = zoom · click = pin the nearest MS2 scan")
                .y_axis_label(if norm { "Intensity (% of maximum)" } else { "Intensity" }), norm);
            if reset_now { plot = plot.reset(); }
            let pr = plot
                .show(ui, |pu| {
                    draw_series!(pu, &series, hover, scan_of, capturing);
                    if let Some(h) = rt_hint { pu.vline(VLine::new(h).color(Color32::GRAY).name("expected RT")); }
                    let r = pu.response().clone();
                    let _ = zoom_pan!(pu, r, zoom_c, false);
                });
            clicked = pr.response.clicked() && !capturing;
            let rect = pr.response.rect;
            let (x0, x1, y0, y1) = { let b = pr.transform.bounds(); (b.min()[0], b.max()[0], b.min()[1], b.max()[1]) };
            let names: Vec<String> = series.iter().map(|s| s.name.clone()).collect();
            export_menu!(pr.response, "chromatogram", ex, &names);
            if let Some(k) = ex {
                // the file name suggestion says what is exported: TIC / BPC / XIC, MS level, target or m/z, file
                let filepart = if shown.len() == 1 { safe_name(&self.samples[shown[0]].data.name) } else { format!("{}files", shown.len()) };
                let (prefix, stem_all, title): (String, String, String) = match self.view {
                    View::Xic => if self.scope == Scope::AllTargets {
                        ("XIC".into(), format!("XIC_all-targets_{}", filepart), "Extracted ion chromatograms (quantifier ions of all targets)".into())
                    } else {
                        let t = safe_name(&self.analytes[a].name);
                        (format!("XIC_{}", t), format!("XIC_{}_{}", t, filepart), format!("Extracted ion chromatogram · {}", self.analytes[a].name))
                    },
                    View::Tic => (format!("TIC_MS{}", self.tic_level), format!("TIC_MS{}_{}", self.tic_level, filepart), format!("Total ion chromatogram (MS{})", self.tic_level)),
                    View::Bpc => (format!("BPC_MS{}", self.tic_level), format!("BPC_MS{}_{}", self.tic_level, filepart), format!("Base peak chromatogram (MS{})", self.tic_level)),
                    View::Quick => {
                        let p = format!("XIC_custom_mz{}_MS{}", fmt::mz4(self.quick_mz), self.quick_level);
                        (p.clone(), format!("{}_{}", p, filepart), format!("Extracted ion chromatogram · m/z {} (MS{})", fmt::mz4(self.quick_mz), self.quick_level))
                    }
                };
                match k {
                    ExportKind::Reset => { reset_req = true; }
                    ExportKind::Csv(i) => {
                        if let Some(sr) = series.get(i) {
                            let stem = if self.view == View::Xic && self.scope == Scope::AllTargets { format!("XIC_{}_{}", safe_name(&sr.name), safe_name(&sr.fname)) }
                                       else { format!("{}_{}", prefix, safe_name(&sr.name)) };
                            csv_req = Some((stem, csv_two("RT (min)", "Intensity", sr.data.iter().map(|p| (p[0], p[1])))));
                        }
                    }
                    _ => {
                        let yf: fn(f64) -> String = if norm { fmt::pct0 } else { fmt::sci3 };
                        let vl: Vec<SvgVLine> = rt_hint.map(|h| SvgVLine { x: h, rgb: (128, 128, 128), label: "expected RT".into() }).into_iter().collect();
                        export_req = Some((k, SvgPlot {
                            title, x_label: "Retention time (min)".into(),
                            y_label: if norm { "Intensity (% of maximum)".into() } else { "Intensity".into() },
                            series: svg_series(&series), vlines: vl, labels: vec![], notes: vec![], bounds: [x0, x1, y0, y1], x_fmt: fmt::rt2, y_fmt: yf,
                        }, rect, stem_all));
                    }
                }
            }
        }
        if let Some(h) = &hover {
            self.hover = Some((h.file, h.rt));
            if clicked { self.pin_scan(h.file, h.rt); }
        }
        if let Some((k, plot, rect, stem)) = export_req { self.do_export(k, plot, rect, &stem); }
        self.zoom_chrom = zoom_c;
        if reset_req { self.reset_chrom = true; ui.ctx().request_repaint(); }
        if let Some((stem, text)) = csv_req { self.save_csv(&stem, &text); }
    }

    /// Retention-time shift control of the current file.
    fn ui_shift(&mut self, ui: &mut egui::Ui, s: usize) {
        let mut v = self.samples[s].shift_ui;
        let mut commit = false;
        ui.horizontal(|ui| {
            ui.label(format!("RT shift for {} (min):", self.samples[s].data.name));
            let r = ui.add(dv_rt(&mut v).speed(0.01));
            commit = r.drag_stopped() || r.lost_focus();
            ui.weak("applies to all chromatograms of this file; resets its manual integrations");
        });
        self.samples[s].shift_ui = v;
        if commit && (v - self.samples[s].rt_shift).abs() > 1e-9 {
            self.push_undo("RT shift");
            self.set_shift(s, v, "manual");
        }
    }

    /// Table of all targets for the current file.
    fn ui_results(&mut self, ui: &mut egui::Ui, cur: Option<usize>) {
        let curve = self.mode == Mode::Curve;
        egui::ScrollArea::vertical().id_salt("res").max_height(220.0).show(ui, |ui| {
            egui::Grid::new("restab").striped(true).show(ui, |ui| {
                for h in ["Target", "Start", "End", "Area (a.u.)"] { ui.strong(h); }
                if curve { ui.strong("Conc"); }
                ui.strong("Flags");
                ui.end_row();
                for a in 0..self.analytes.len() {
                    if ui.selectable_label(self.sel_a == a, &self.analytes[a].name).clicked() { self.sel_a = a; }
                    match cur.and_then(|s| self.samples[s].cells.get(a)) {
                        Some(c) => {
                            ui.label(format!("{:.2}", c.start)); ui.label(format!("{:.2}", c.end));
                            ui.label(fmt::sci(c.areas.first().copied().unwrap_or(0.0)));
                            if curve { ui.label(cur.and_then(|s| self.conc_of(s, a)).map(fmt::sci).unwrap_or_else(|| "-".into())); }
                            ui.label(cur.map(|s| self.flags(s, a)).unwrap_or_default());
                        }
                        None => { for _ in 0..(if curve { 5 } else { 4 }) { ui.label("-"); } }
                    }
                    ui.end_row();
                }
            });
        });
    }

    /// Calibration of the selected target: levels with their replicates, the curve plot and its export.
    fn ui_cal(&mut self, ui: &mut egui::Ui) {
        let a = self.sel_a;
        let fit = self.fits.get(a).copied().flatten();
        match fit {
            Some(f) => ui.label(format!("{}: area = {}·conc + {}   R² = {}   n = {}   weighting {}{}", self.analytes[a].name, fmt::sci(f.m), fmt::sci(f.b), fmt::sci(f.r2),
                fmt::sci(f.n as f64), WEIGHTS[self.weighting as usize], if f.n < 5 { "   ⚠ fewer than 5 points" } else { "" })),
            None => ui.label("No calibration for this target: assign standard concentrations in the 'Calibration setup' tab (≥2 levels)."),
        };
        // standards of THIS target, grouped into concentration levels (replicates share a level)
        let mut std_idx: Vec<usize> = (0..self.samples.len()).filter(|&i| {
            let s = &self.samples[i];
            s.kind == Kind::Standard && s.cells.get(a).map(|c| c.conc > 0.0).unwrap_or(false)
        }).collect();
        std_idx.sort_by(|&x, &y| {
            let (cx, cy) = (self.samples[x].cells[a].conc, self.samples[y].cells[a].conc);
            cx.partial_cmp(&cy).unwrap_or(std::cmp::Ordering::Equal).then_with(|| self.samples[x].data.name.cmp(&self.samples[y].data.name))
        });
        let mut toggled: Option<usize> = None;
        let mut use_all = false;
        ui.horizontal(|ui| {
            if ui.button("Use all replicates").clicked() { use_all = true; }
            ui.weak(format!("{} standard file(s) assigned to this target; untick a replicate to exclude it from the curve", std_idx.len()));
        });
        let reset_cal_now = std::mem::take(&mut self.reset_cal);
        let mut cal_req: Option<(ExportKind, SvgPlot, egui::Rect, String)> = None;
        let mut cal_reset = false;
        ui.columns(2, |cols| {
            cols[0].push_id("calt", |ui| {
                egui::ScrollArea::vertical().id_salt("calscroll").max_height(240.0).show(ui, |ui| {
                    egui::Grid::new("caltab").striped(true).show(ui, |ui| {
                        for h in ["Level / file", "Conc", "Area", "Accuracy %", "Use"] { ui.strong(h); }
                        ui.end_row();
                        let mut k = 0;
                        while k < std_idx.len() {
                            let c0 = self.samples[std_idx[k]].cells[a].conc;
                            let mut m = k;
                            while m < std_idx.len() && (self.samples[std_idx[m]].cells[a].conc - c0).abs() <= 1e-9 * c0.max(1.0) { m += 1; }
                            let group = &std_idx[k..m];
                            let used: Vec<f64> = group.iter().filter(|&&i| { let c = &self.samples[i].cells[a]; c.found && c.include })
                                .map(|&i| self.samples[i].cells[a].areas[0]).collect();
                            let mean = if used.is_empty() { 0.0 } else { used.iter().sum::<f64>() / used.len() as f64 };
                            let rsd = if used.len() >= 2 && mean > 0.0 {
                                let var = used.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (used.len() as f64 - 1.0);
                                100.0 * var.sqrt() / mean
                            } else { 0.0 };
                            ui.strong(format!("Level {} {}", fmt::sci(c0), self.units));
                            ui.label(format!("n = {}/{}", used.len(), group.len()));
                            ui.label(format!("mean {}", fmt::sci(mean)));
                            ui.label(if used.len() >= 2 { format!("RSD {} %", fmt::sci(rsd)) } else { String::new() });
                            ui.label("");
                            ui.end_row();
                            for &si in group {
                                let sm = &self.samples[si];
                                let c = &sm.cells[a];
                                ui.label(format!("   {}", sm.data.name));
                                ui.label(fmt::sci(c.conc));
                                ui.label(fmt::sci(c.areas[0]));
                                ui.label(match fit { Some(f) if f.m != 0.0 => fmt::sci(100.0 * ((c.areas[0] - f.b) / f.m) / c.conc), _ => "-".into() });
                                let mut inc = c.include;
                                if ui.checkbox(&mut inc, "").changed() { toggled = Some(si); }
                                ui.end_row();
                            }
                            k = m;
                        }
                    });
                });
            });
            let (mut inc_pts, mut exc_pts) = (vec![], vec![]);
            for &si in &std_idx {
                let c = &self.samples[si].cells[a];
                if !c.found { continue; }
                let p = [c.conc, c.areas[0]];
                if c.include { inc_pts.push(p) } else { exc_pts.push(p) }
            }
            let units = self.units.clone();
            let line_pts: Option<Vec<[f64; 2]>> = fit.map(|f| vec![[0.0, f.b], [f.hi * 1.05, f.m * f.hi * 1.05 + f.b]]);
            // regression formula and R² are written into the plot itself (and therefore into PNG / SVG exports)
            let notes: Vec<String> = match fit {
                Some(f) => vec![
                    format!("area = {}·conc + {}", fmt::sci_prec(f.m, 4), fmt::sci_prec(f.b, 4)),
                    format!("R² = {}   n = {}", fmt::sci_prec(f.r2, 5), fmt::sci(f.n as f64)),
                    format!("weighting {}", WEIGHTS[self.weighting as usize]),
                ],
                None => vec![],
            };
            let (inc_c, exc_c) = (inc_pts.clone(), exc_pts.clone());
            let mut plot = sciplot!(Plot::new("cal").height(220.0).x_axis_label(format!("Concentration ({})", units)).y_axis_label("Area"));
            if reset_cal_now { plot = plot.reset(); }
            let pr = plot.show(&mut cols[1], |pu| {
                pu.points(Points::new(inc_pts).radius(4.0_f32).name("used"));
                pu.points(Points::new(exc_pts).radius(4.0_f32).color(Color32::GRAY).name("excluded"));
                if let Some(lp) = &line_pts { pu.line(Line::new(PlotPoints::from(lp.clone())).name("fit")); }
            });
            let rect = pr.response.rect;
            let (x0, x1, y0, y1) = { let b = pr.transform.bounds(); (b.min()[0], b.max()[0], b.min()[1], b.max()[1]) };
            // The formula and R² are painted on top of the plot instead of being a plot item: an item at the corner of the
            // visible area feeds back into the automatic bounds and made the plot grow with every repaint.
            if !notes.is_empty() {
                let painter = cols[1].painter().with_clip_rect(rect);
                let galley = painter.layout_no_wrap(notes.join("\n"), egui::FontId::proportional(13.0), Color32::WHITE);
                let pos = rect.min + egui::vec2(95.0, 8.0);
                let frame = egui::Rect::from_min_size(pos - egui::vec2(5.0, 3.0), galley.size() + egui::vec2(10.0, 6.0));
                painter.rect_filled(frame, 3.0_f32, Color32::from_black_alpha(190));
                painter.galley(pos, galley, Color32::WHITE);
            }
            let names: Vec<String> = vec![];
            let mut ex: Option<ExportKind> = None;
            export_menu!(pr.response, "calibration curve", ex, &names);
            if let Some(k) = ex {
                match k {
                    ExportKind::Reset => { cal_reset = true; }
                    ExportKind::Csv(_) => {}
                    _ => {
                        let mut ss = vec![SvgSeries { name: "used".into(), rgb: (31, 119, 180), width: 1.0, points: inc_c, marker: true }];
                        if !exc_c.is_empty() { ss.push(SvgSeries { name: "excluded".into(), rgb: (150, 150, 150), width: 1.0, points: exc_c, marker: true }); }
                        if let Some(lp) = line_pts { ss.push(SvgSeries { name: "fit".into(), rgb: (214, 39, 40), width: 2.0, points: lp, marker: false }); }
                        export_req_cal(&mut cal_req, k, SvgPlot {
                            title: format!("Calibration curve · {}", self.analytes[a].name),
                            x_label: format!("Concentration ({})", units), y_label: "Area (intensity·s)".into(),
                            series: ss, vlines: vec![], labels: vec![], notes, bounds: [x0, x1, y0, y1], x_fmt: fmt::sci3, y_fmt: fmt::sci3,
                        }, rect, format!("Calibration_curve_{}", safe_name(&self.analytes[a].name)));
                    }
                }
            }
        });
        if cal_reset { self.reset_cal = true; ui.ctx().request_repaint(); }
        if let Some((k, plot, rect, stem)) = cal_req { self.do_export(k, plot, rect, &stem); }
        if use_all {
            self.push_undo("use all replicates");
            for &si in &std_idx { self.samples[si].cells[a].include = true; }
            self.refit(a);
        }
        if let Some(si) = toggled {
            self.push_undo("calibration point");
            let inc = !self.samples[si].cells[a].include;
            self.samples[si].cells[a].include = inc;
            let msg = format!("file={} analyte={} include={} reason={}", self.samples[si].data.name, self.analytes[a].name, inc,
                if self.reason.trim().is_empty() { "NONE GIVEN" } else { self.reason.trim() });
            self.audit.log("CAL_POINT_TOGGLE", &msg);
            self.refit(a);
        }
    }

    /// Areas or concentrations of every target in every imported file.
    fn ui_matrix(&mut self, ui: &mut egui::Ui) {
        if self.analytes.is_empty() { ui.label("Load a target table to see the results matrix."); return; }
        let curve = self.mode == Mode::Curve;
        let use_conc = curve && self.matrix_conc;
        ui.horizontal(|ui| {
            ui.label("Values:");
            ui.selectable_value(&mut self.matrix_conc, false, "Areas (a.u.)");
            if curve { ui.selectable_value(&mut self.matrix_conc, true, format!("Concentrations ({})", self.units)); }
            if ui.button("Copy as TSV").clicked() {
                let mut t = String::from("file\ttype");
                for a in &self.analytes { t += &format!("\t{}", a.name); }
                t.push('\n');
                for s in 0..self.samples.len() {
                    t += &format!("{}\t{}", self.samples[s].data.name, self.samples[s].kind.label());
                    for a in 0..self.analytes.len() {
                        let c = self.samples[s].cells.get(a);
                        let v = match c { Some(c) if c.found && c.areas[0] > 0.0 => if use_conc { self.conc_of(s, a) } else { Some(c.areas[0]) }, _ => None };
                        t += &match v { Some(v) => format!("\t{:.6e}", v), None => "\tND".to_string() };
                    }
                    t.push('\n');
                }
                ui.ctx().copy_text(t);
                self.status = "Results matrix copied to the clipboard (tab-separated)".into();
            }
            ui.weak("yellow = flagged (see Flags in the chromatogram tab), ND = not detected");
        });
        egui::ScrollArea::both().id_salt("matrix").show(ui, |ui| {
            egui::Grid::new("matrix").striped(true).show(ui, |ui| {
                ui.strong("File"); ui.strong("Type");
                for a in &self.analytes { ui.strong(&a.name); }
                ui.end_row();
                for s in 0..self.samples.len() {
                    ui.label(&self.samples[s].data.name);
                    ui.label(self.samples[s].kind.label());
                    for a in 0..self.analytes.len() {
                        match self.samples[s].cells.get(a) {
                            Some(c) if c.found && c.areas[0] > 0.0 => {
                                let v = if use_conc { self.conc_of(s, a).map(fmt::sci).unwrap_or_else(|| "-".into()) } else { fmt::sci(c.areas[0]) };
                                let fl = self.flags(s, a);
                                if fl.is_empty() || fl == "manual" { ui.label(v); } else { ui.colored_label(Color32::YELLOW, v).on_hover_text(fl); }
                            }
                            _ => { ui.label("ND"); }
                        }
                    }
                    ui.end_row();
                }
            });
        });
    }

    /// The chromatogram tab: view selection, plot, results table and calibration.
    fn ui_chrom(&mut self, ui: &mut egui::Ui) {
        let cur = self.cur();
        let mut add_q = false;
        ui.horizontal(|ui| {
            ui.label("View:");
            ui.selectable_value(&mut self.view, View::Xic, "Target XICs");
            ui.selectable_value(&mut self.view, View::Tic, "TIC");
            ui.selectable_value(&mut self.view, View::Bpc, "BPC");
            ui.selectable_value(&mut self.view, View::Quick, "Custom m/z (quick XIC)");
            ui.separator();
            if self.view == View::Xic {
                egui::ComboBox::new("scope", "").width(330.0).selected_text(match self.scope {
                    Scope::Ions => "Selected target, all ions, current file (edit integration)",
                    Scope::AllTargets => "All targets (quant ions), current file",
                    Scope::Files => "Selected target (quant ion), all selected files",
                }).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.scope, Scope::Ions, "Selected target, all ions, current file (edit integration)");
                    ui.selectable_value(&mut self.scope, Scope::AllTargets, "All targets (quant ions), current file");
                    ui.selectable_value(&mut self.scope, Scope::Files, "Selected target (quant ion), all selected files");
                });
            } else if self.view == View::Quick {
                ui.label("m/z");
                ui.add(dv_mz(&mut self.quick_mz).speed(0.01).range(0.0..=100000.0));
                ui.label(format!("± {}{}", fmt::sci(self.tol), if self.tol_ppm { " ppm" } else { " Da" }));
                ui.label("level");
                ui.selectable_value(&mut self.quick_level, 1u8, "MS1");
                ui.selectable_value(&mut self.quick_level, 2u8, "MS2");
                if ui.button("Add as target").clicked() { add_q = true; }
            } else {
                ui.label("Scans:");
                ui.selectable_value(&mut self.tic_level, 1u8, "MS1");
                ui.selectable_value(&mut self.tic_level, 2u8, "MS2");
            }
            ui.checkbox(&mut self.normalize, "Normalize");
            if !self.analytes.is_empty() && self.view == View::Xic && self.scope != Scope::AllTargets {
                ui.separator();
                ui.label("Target:");
                egui::ComboBox::new("an", "").selected_text(self.analytes[self.sel_a].name.clone()).show_ui(ui, |ui| {
                    for i in 0..self.analytes.len() { let n = self.analytes[i].name.clone(); ui.selectable_value(&mut self.sel_a, i, n); }
                });
            }
            ui.label(format!("Current file: {}", cur.map(|s| self.samples[s].data.name.clone()).unwrap_or_else(|| "none".into())));
        });
        if add_q { self.add_quick_target(); }
        let needs_cur = self.view == View::Xic && self.scope != Scope::Files;
        if self.view == View::Xic && self.analytes.is_empty() {
            ui.label("Load a target table (.csv) to see target XICs. TIC and BPC views work without one.");
        } else if needs_cur && cur.is_none() {
            ui.label("No file selected.");
        } else if self.view == View::Xic && self.scope == Scope::Ions {
            if let Some(s) = cur { self.ui_plot(ui, s); }
        } else {
            self.ui_overlay(ui, cur);
        }
        if let Some(s) = cur { self.ui_shift(ui, s); }
        if !self.analytes.is_empty() {
            ui.separator();
            self.ui_results(ui, cur);
            if self.mode == Mode::Curve {
                ui.separator();
                self.ui_cal(ui);
            }
        }
    }

    /// Distributes the editable columns of the target editor over `avail` pixels.
    fn fit_cols(&mut self, avail: f32) {
        let w = (avail - 28.0 - 4.0 * 6.0 - 130.0).max(260.0);
        self.col_w = [w * 0.22, w * 0.10, w * 0.14, w * 0.54];
    }

    /// Floating window for editing the target table; edits are applied when a field loses focus.
    fn ui_targets_window(&mut self, ctx: &egui::Context) {
        if !self.show_targets { return; }
        let mut open = true;
        let (mut commit, mut delete): (Option<usize>, Option<usize>) = (None, None);
        let (mut add, mut save, mut save_as, mut open_t) = (false, false, false, false);
        egui::Window::new("Target table (live editable)").open(&mut open).default_size([1000.0, 440.0]).resizable(true).show(ctx, |ui| {
            if self.rows_dirty || self.rows.len() != self.analytes.len() { self.sync_rows(); }
            let avail = ui.available_width();
            if self.col_w[0] < 1.0 { self.fit_cols(avail); }
            ui.horizontal(|ui| {
                if ui.button("Add target").clicked() { add = true; }
                if ui.button("Open…").clicked() { open_t = true; }
                if ui.button("Save").clicked() { save = true; }
                if ui.button("Save as…").clicked() { save_as = true; }
                if ui.button("Fit columns to window").clicked() { self.fit_cols(avail); }
                ui.weak(match &self.table_path { Some(p) => p.display().to_string(), None => "(not saved yet)".into() });
            });
            ui.weak("Drag the column borders to resize; the window scrolls sideways if needed. Changes apply when you leave a field and re-integrate immediately. Precursor m/z is only needed for MS1 quantification.");
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_sized([28.0, 18.0], egui::Label::new(RichText::new("#").strong()));
                    let heads = ["Name", "RT (min)", "Precursor m/z (MS1)", "Product ion m/z (MS2), separated by ;"];
                    for k in 0..4 {
                        ui.add_sized([self.col_w[k], 18.0], egui::Label::new(RichText::new(heads[k]).strong()));
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(6.0, 18.0), egui::Sense::drag());
                        ui.painter().vline(rect.center().x, rect.y_range(), ui.visuals().widgets.noninteractive.bg_stroke);
                        if resp.hovered() || resp.dragged() { ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal); }
                        if resp.dragged() { self.col_w[k] = (self.col_w[k] + resp.drag_delta().x).max(40.0); }
                    }
                });
                for i in 0..self.rows.len() {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.add_sized([28.0, 20.0], egui::Label::new((i + 1).to_string()));
                        for k in 0..4 {
                            let w = self.col_w[k];
                            let r = ui.add_sized([w, 20.0], egui::TextEdit::singleline(&mut self.rows[i][k]));
                            if r.lost_focus() { commit = Some(i); }
                            ui.add_space(6.0);
                        }
                        if parse_row(&self.rows[i]).is_none() { ui.colored_label(Color32::RED, "invalid"); }
                        ui.add_space(8.0);
                        if ui.small_button("Delete").clicked() { delete = Some(i); }
                    });
                }
            });
        });
        self.show_targets = open;
        if let Some(i) = commit { self.apply_row(i); }
        if let Some(i) = delete { self.delete_target(i); }
        if add { self.add_target(); }
        if open_t { self.open_targets_dialog(); }
        if save_as { self.save_targets_as(); }
        if save {
            match self.table_path.clone() { Some(p) => self.write_targets(&p), None => self.save_targets_as() }
        }
    }

    /// The menu bar.
    fn menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New project").shortcut_text("Ctrl+N")).clicked() { self.new_project(); ui.close_menu(); }
                if ui.add(egui::Button::new("Open project…").shortcut_text("Ctrl+O")).clicked() { self.open_project_dialog(ctx); ui.close_menu(); }
                if ui.add(egui::Button::new("Save project").shortcut_text("Ctrl+S")).clicked() { self.save_project(ctx, false); ui.close_menu(); }
                if ui.button("Save project as…").clicked() { self.save_project(ctx, true); ui.close_menu(); }
                ui.checkbox(&mut self.embed_raw, "Embed raw files when saving projects");
                ui.separator();
                if ui.button("Import .raw / .mzML files…").clicked() {
                    if let Some(files) = self.dialog().set_title("Import Thermo .raw or mzML files").add_filter("Thermo RAW / mzML", &["raw", "mzML", "mzml"]).pick_files() {
                        for f in files { self.start_load_any(ctx, &f.to_string_lossy()); }
                    }
                    ui.close_menu();
                }
                if ui.button("Open target table…").clicked() { self.open_targets_dialog(); ui.close_menu(); }
                if ui.button("Save target table as…").clicked() { self.save_targets_as(); ui.close_menu(); }
                ui.separator();
                if ui.button("Export analysis results…").clicked() {
                    self.choose_out_path();
                    self.status = match self.export() { Ok(_) => format!("Exported {}", self.export_path().display()), Err(e) => format!("Export failed: {}", e) };
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Exit").clicked() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
            });
            ui.menu_button("Edit", |ui| {
                let (nu, nr) = self.history.lens();
                let ul = self.history.peek_undo().map(|s| format!("Undo {}", s.label)).unwrap_or_else(|| "Undo".into());
                let rl = self.history.peek_redo().map(|s| format!("Redo {}", s.label)).unwrap_or_else(|| "Redo".into());
                if ui.add_enabled(nu > 0, egui::Button::new(ul).shortcut_text("Ctrl+Z")).clicked() { self.undo(); ui.close_menu(); }
                if ui.add_enabled(nr > 0, egui::Button::new(rl).shortcut_text("Ctrl+Y")).clicked() { self.redo(); ui.close_menu(); }
                ui.weak(format!("history: {} back / {} forward (max {})", nu, nr, MAX_UNDO));
                ui.separator();
                if ui.button("Edit target table…").clicked() { self.show_targets = true; ui.close_menu(); }
                ui.separator();
                if ui.button("Preferences…").clicked() { self.show_prefs = true; ui.close_menu(); }
            });
            ui.menu_button("View", |ui| {
                if ui.add(egui::Button::new("Zoom in").shortcut_text("Ctrl++")).clicked() { self.zoom = (self.zoom + 0.1).min(3.0); }
                if ui.add(egui::Button::new("Zoom out").shortcut_text("Ctrl+-")).clicked() { self.zoom = (self.zoom - 0.1).max(0.5); }
                if ui.add(egui::Button::new("Reset zoom").shortcut_text("Ctrl+0")).clicked() { self.zoom = 1.0; }
                ui.separator();
                if ui.selectable_label(self.tab == Tab::Chrom, "Chromatograms").clicked() { self.tab = Tab::Chrom; ui.close_menu(); }
                if ui.selectable_label(self.tab == Tab::Matrix, "Results matrix (all files)").clicked() { self.tab = Tab::Matrix; ui.close_menu(); }
                if ui.selectable_label(self.tab == Tab::Setup, "Calibration setup").clicked() { self.tab = Tab::Setup; ui.close_menu(); }
                ui.separator();
                ui.checkbox(&mut self.show_spectrum, "MS2 spectrum panel");
                if ui.button("Audit trail").clicked() { self.show_audit = true; ui.close_menu(); }
            });
            ui.menu_button("Analysis", |ui| {
                if ui.button("Re-integrate all (apply settings)").clicked() { self.apply_settings(); ui.close_menu(); }
                if ui.button("Align retention times now").clicked() { self.align_all(); ui.close_menu(); }
                if ui.button("Clear retention-time alignment").clicked() { self.clear_alignment(); ui.close_menu(); }
                ui.separator();
                if ui.selectable_label(self.mode == Mode::Curve, "Quantify with standard curve").clicked() { self.mode = Mode::Curve; ui.close_menu(); }
                if ui.selectable_label(self.mode == Mode::Areas, "Report integrated areas only").clicked() { self.mode = Mode::Areas; ui.close_menu(); }
                ui.separator();
                if ui.selectable_label(self.basis == Basis::Ms2, "Quantify on MS2 product-ion area").clicked() { self.set_basis(Basis::Ms2); ui.close_menu(); }
                if ui.selectable_label(self.basis == Basis::Ms1, "Quantify on MS1 precursor area").clicked() { self.set_basis(Basis::Ms1); ui.close_menu(); }
                ui.separator();
                if ui.button("Verify audit trail integrity").clicked() {
                    self.status = match audit::verify(&self.audit.path) {
                        Ok(n) => format!("Audit trail intact: {} entries, hash chain verified", n),
                        Err(e) => format!("AUDIT TRAIL PROBLEM: {}", e),
                    };
                    ui.close_menu();
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("README (offline)").clicked() { self.show_readme = true; ui.close_menu(); }
                if ui.button("Credits & licenses").clicked() { self.show_credits = true; ui.close_menu(); }
                ui.separator();
                ui.label(format!("msQuant v{}", VERSION));
            });
        });
    }

    /// Handles keyboard shortcuts: zoom, undo, redo and the project commands.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let (zin, zout, zreset) = ctx.input_mut(|i| (
            i.consume_key(Modifiers::COMMAND, Key::Plus) || i.consume_key(Modifiers::COMMAND, Key::Equals),
            i.consume_key(Modifiers::COMMAND, Key::Minus),
            i.consume_key(Modifiers::COMMAND, Key::Num0),
        ));
        if zin { self.zoom = (self.zoom + 0.1).min(3.0); }
        if zout { self.zoom = (self.zoom - 0.1).max(0.5); }
        if zreset { self.zoom = 1.0; }
        if (self.zoom - ctx.zoom_factor()).abs() > 1e-4 { ctx.set_zoom_factor(self.zoom); self.write_settings(); }
        if ctx.wants_keyboard_input() { return; }
        let cs = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        let (redo, undo, save, open, new) = ctx.input_mut(|i| (
            i.consume_key(cs, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y),
            i.consume_key(Modifiers::COMMAND, Key::Z),
            i.consume_key(Modifiers::COMMAND, Key::S),
            i.consume_key(Modifiers::COMMAND, Key::O),
            i.consume_key(Modifiers::COMMAND, Key::N),
        ));
        if redo { self.redo(); }
        if undo { self.undo(); }
        if save { self.save_project(ctx, false); }
        if open { self.open_project_dialog(ctx); }
        if new { self.new_project(); }
    }
}

impl App {
    /// m/z values traced for target `a`: the product ions (MS2 basis) or the precursor (MS1 basis).
    fn trace_mz(&self, a: usize) -> Vec<f64> {
        match self.basis {
            Basis::Ms2 => self.analytes[a].ions.clone(),
            Basis::Ms1 => vec![self.analytes[a].precursor.unwrap_or(0.0)],
        }
    }

    /// Switches the quantified signal (undoable) and recomputes everything.
    fn set_basis(&mut self, b: Basis) {
        if self.basis == b { return; }
        self.push_undo("quantification signal");
        self.basis = b;
        self.audit.log("QUANT_BASIS", if b == Basis::Ms1 { "MS1 precursor area" } else { "MS2 product-ion area" });
        self.recompute_all();
        if b == Basis::Ms1 && self.analytes.iter().any(|a| a.precursor.is_none()) {
            self.status = "Some targets have no precursor m/z (add p:<m/z> in the table); they cannot be quantified on the MS1 signal.".into();
        }
    }

    /// MS2 scan (index into `spectra`) closest in time to `rt` (aligned minutes) in file `f`.
    fn nearest_ms2(&self, f: usize, rt: f64) -> Option<usize> {
        let sm = self.samples.get(f)?;
        let sp = &sm.data.spectra;
        if sp.is_empty() { return None; }
        let t = rt - sm.rt_shift;
        let j = sp.partition_point(|x| x.rt < t);
        let (c0, c1) = (j.saturating_sub(1), j.min(sp.len() - 1));
        Some(if (sp[c1].rt - t).abs() < (sp[c0].rt - t).abs() { c1 } else { c0 })
    }

    /// MS2 scan inside the integration window with the strongest quantifier product ion.
    fn apex_scan(&self, s: usize, a: usize) -> Option<usize> {
        let sm = self.samples.get(s)?;
        let c = sm.cells.get(a)?;
        if !c.found { return None; }
        let ion = *self.analytes.get(a)?.ions.first()?;
        let tol = self.tol_da(ion);
        let (t0, t1) = (c.start - sm.rt_shift, c.end - sm.rt_shift);
        let sp = &sm.data.spectra;
        let (mut best, mut top) = (None, 0.0f64);
        for j in sp.partition_point(|x| x.rt < t0)..sp.len() {
            if sp[j].rt > t1 { break; }
            let v = quant::intensity_near(&sp[j].mz, &sp[j].inten, ion, tol);
            if v > top { top = v; best = Some(j); }
        }
        best
    }

    /// Pins the MS2 scan closest to `rt` in file `f` in the spectrum panel.
    fn pin_scan(&mut self, f: usize, rt: f64) {
        if let Some(i) = self.nearest_ms2(f, rt) { self.pinned_scan = Some((f, i)); }
    }

    /// Which MS2 scan the spectrum panel shows: hovered > pinned > apex of the selected target in the current file.
    fn spectrum_pick(&self) -> Option<(usize, usize, &'static str)> {
        if self.follow_hover {
            if let Some((f, rt)) = self.hover { if let Some(i) = self.nearest_ms2(f, rt) { return Some((f, i, "hovered")); } }
        }
        if let Some((f, i)) = self.pinned_scan {
            if f < self.samples.len() && i < self.samples[f].data.spectra.len() { return Some((f, i, "pinned")); }
        }
        let s = self.cur()?;
        if self.sel_a < self.analytes.len() { return self.apex_scan(s, self.sel_a).map(|i| (s, i, "apex of the selected target")); }
        None
    }

    /// Second graph: MS2 spectrum (stems; target product ions highlighted). The cursor snaps to the nearest peak.
    fn ui_spectrum(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("MS2 spectrum");
            ui.checkbox(&mut self.follow_hover, "follow chromatogram hover");
            ui.checkbox(&mut self.spec_autofit, "auto-fit when the scan changes");
            if self.pinned_scan.is_some() && ui.button("Unpin scan").clicked() { self.pinned_scan = None; }
            ui.weak("drag a box = zoom · Shift/middle-drag = pan · Alt+wheel = scale y · Shift+Alt+wheel = scale x · right-click = reset zoom / save");
        });
        let (f, i, why) = match self.spectrum_pick() {
            Some(x) => x,
            None => { ui.label("No MS2 scan to show: select a file and a target with a detected peak, or click the chromatogram to pin a scan."); return; }
        };
        let mut export_req: Option<(ExportKind, SvgPlot, egui::Rect, String)> = None;
        let mut csv_req: Option<(String, String)> = None;
        // the view is re-fitted when the scan changes (if "auto-fit" is on) or when "Reset zoom" was chosen; otherwise a zoom window stays
        let reset_now = std::mem::take(&mut self.reset_spec) || (self.spec_autofit && self.spec_last != Some((f, i)));
        let alt = ui.input(|i| i.modifiers.alt);
        self.spec_last = Some((f, i));
        let mut zoom_s = self.zoom_spec;
        let mut zoomed_spec = false;
        let mut reset_req = false;
        {
            let sm = &self.samples[f];
            let sp = &sm.data.spectra[i];
            let norm = self.normalize;
            let capturing = self.capture.is_some();
            let mx = sp.inten.iter().cloned().fold(0.0f32, f32::max) as f64;
            let scale = if norm && mx > 0.0 { 100.0 / mx } else { 1.0 };
            let rt_scan = sp.rt + sm.rt_shift;
            let head = format!("{} · MS2 scan {} · RT {:.2} min · precursor m/z {} · {} peaks · shown: {}",
                sm.data.name, sp.scan, rt_scan, prec_str(sp.precursor), sp.mz.len(), why);
            ui.label(&head);
            let mut all: Vec<[f64; 2]> = Vec::with_capacity(sp.mz.len() * 3);
            for (m, y) in sp.mz.iter().zip(sp.inten.iter()) { let y = *y as f64 * scale; all.push([*m, 0.0]); all.push([*m, y]); all.push([*m, 0.0]); }
            let mut hl: Vec<(usize, f64, Vec<[f64; 2]>)> = vec![];
            if self.sel_a < self.analytes.len() {
                for (k, &ion) in self.analytes[self.sel_a].ions.iter().enumerate() {
                    let tol = self.tol_da(ion);
                    let mut v = vec![];
                    for (m, y) in sp.mz.iter().zip(sp.inten.iter()) {
                        if (*m - ion).abs() <= tol { let y = *y as f64 * scale; v.push([*m, 0.0]); v.push([*m, y]); v.push([*m, 0.0]); }
                    }
                    hl.push((k, ion, v));
                }
            }
            let top10 = top_peaks(&sp.mz, &sp.inten, 10);
            if !hl.is_empty() {
                ui.horizontal(|ui| {
                    for (k, ion, v) in &hl {
                        ui.colored_label(pal(*k), format!("■ target ion m/z {}{}", fmt::mz4(*ion), if v.is_empty() { " (not in this scan)" } else { "" }));
                    }
                });
            }
            let mut plot = mzplot!(Plot::new("spec").height(ui.available_height()).allow_drag(false).allow_boxed_zoom(false).allow_scroll(!alt).x_axis_label("m/z")
                .y_axis_label(if norm { "Intensity (% of maximum)" } else { "Intensity" }), norm);
            if reset_now { plot = plot.reset(); }
            let pr = plot.show(ui, |pu| {
                pu.line(Line::new(PlotPoints::from(all.clone())).color(Color32::GRAY));
                for (k, ion, v) in &hl {
                    pu.vline(VLine::new(*ion).color(pal(*k).gamma_multiply(0.5)));
                    if !v.is_empty() { pu.line(Line::new(PlotPoints::from(v.clone())).color(pal(*k)).width(2.5_f32)); }
                }
                if capturing {
                    // figure export: label the 10 most intense peaks with their m/z
                    for (m, y) in &top10 {
                        pu.text(Text::new(PlotPoint::new(*m, *y * scale),
                            RichText::new(fmt::mz4(*m)).size(11.0).color(Color32::BLACK).background_color(Color32::from_rgba_unmultiplied(255, 255, 255, 215)))
                            .anchor(egui::Align2::CENTER_BOTTOM));
                    }
                } else if let Some(p) = pu.pointer_coordinate().filter(|p| pu.response().hovered() && in_view(p, &pu.plot_bounds())) {
                    // snap to the peak closest to the cursor and show only its RT and m/z
                    let b = pu.plot_bounds();
                    let (xw, yw) = (b.max()[0] - b.min()[0], b.max()[1] - b.min()[1]);
                    if let Some((m, y)) = nearest_peak(&sp.mz, &sp.inten, scale, p.x, p.y, xw, yw) {
                        let right = m > (b.min()[0] + b.max()[0]) / 2.0;
                        pu.line(Line::new(PlotPoints::from(vec![[m, 0.0], [m, y]])).color(Color32::WHITE).width(3.0_f32));
                        pu.points(Points::new(vec![[m, y]]).radius(5.0_f32).color(Color32::WHITE));
                        pu.text(Text::new(PlotPoint::new(m, y),
                            RichText::new(format!("RT {:.2} min\nm/z {}", rt_scan, fmt::mz4(m))).color(Color32::WHITE).background_color(Color32::from_black_alpha(200)))
                            .anchor(if right { egui::Align2::RIGHT_BOTTOM } else { egui::Align2::LEFT_BOTTOM }));
                    }
                }
                let r = pu.response().clone();
                if zoom_pan!(pu, r, zoom_s, false) { zoomed_spec = true; }
            });
            let rect = pr.response.rect;
            let (x0, x1, y0, y1) = { let b = pr.transform.bounds(); (b.min()[0], b.max()[0], b.min()[1], b.max()[1]) };
            let names = vec!["MS2 spectrum (m/z, Intensity)".to_string()];
            let mut ex: Option<ExportKind> = None;
            export_menu!(pr.response, "MS2 spectrum", ex, &names);
            if let Some(k) = ex {
                let stem = format!("MS2_scan_{}_{}", sp.scan, safe_name(&sm.data.name));
                match k {
                    ExportKind::Reset => { reset_req = true; }
                    ExportKind::Csv(_) => {
                        csv_req = Some((stem, csv_two("m/z", "Intensity", sp.mz.iter().cloned().zip(sp.inten.iter().map(|&v| v as f64)))));
                    }
                    _ => {
                        let mut ss = vec![SvgSeries { name: "peaks".into(), rgb: (128, 128, 128), width: 1.0, points: all.clone(), marker: false }];
                        let mut vl = vec![];
                        for (ci, ion, v) in &hl {
                            if !v.is_empty() { ss.push(SvgSeries { name: format!("target ion m/z {}", fmt::mz4(*ion)), rgb: color_rgb(pal(*ci)), width: 2.0, points: v.clone(), marker: false }); }
                            vl.push(SvgVLine { x: *ion, rgb: color_rgb(pal(*ci)), label: String::new() });
                        }
                        let labels: Vec<SvgLabel> = top10.iter().map(|(m, y)| SvgLabel { x: *m, y: *y * scale, text: fmt::mz4(*m), rgb: (0, 0, 0) }).collect();
                        let yf: fn(f64) -> String = if norm { fmt::pct0 } else { fmt::sci3 };
                        export_req = Some((k, SvgPlot {
                            title: head.clone(), x_label: "m/z".into(),
                            y_label: if norm { "Intensity (% of maximum)".into() } else { "Intensity".into() },
                            series: ss, vlines: vl, labels, notes: vec![], bounds: [x0, x1, y0, y1], x_fmt: fmt::mz4, y_fmt: yf,
                        }, rect, stem));
                    }
                }
            }
        }
        if let Some((k, plot, rect, stem)) = export_req { self.do_export(k, plot, rect, &stem); }
        self.zoom_spec = zoom_s;
        if zoomed_spec { self.spec_autofit = false; }
        if reset_req { self.reset_spec = true; ui.ctx().request_repaint(); }
        if let Some((stem, text)) = csv_req { self.save_csv(&stem, &text); }
    }

    /// Asks for a file name, then writes the SVG at once or schedules a PNG capture of the plot rectangle.
    fn do_export(&mut self, kind: ExportKind, plot: SvgPlot, rect: egui::Rect, stem: &str) {
        let (ext, label) = if kind == ExportKind::Svg { ("svg", "SVG image") } else { ("png", "PNG image") };
        let d = self.dialog().set_title("Save figure").add_filter(label, &[ext]).set_file_name(format!("{}.{}", stem, ext));
        let path = match d.save_file() { Some(p) => p, None => return };
        match kind {
            ExportKind::Png => {
                self.status = format!("Capturing the plot to {} ...", path.display());
                self.capture = Some(Capture { rect, path, frames: 3, sent: false });
            }
            _ => match std::fs::write(&path, svg::render(&plot)) {
                Ok(_) => { self.audit.log("FIGURE_EXPORTED", &format!("{} ({})", path.display(), plot.title)); self.status = format!("Saved {}", path.display()); }
                Err(e) => self.status = format!("Saving the figure failed: {}", e),
            },
        }
    }

    /// Two-column data export (RT/Intensity or m/z/Intensity); the suggested name says what is exported.
    fn save_csv(&mut self, stem: &str, text: &str) {
        let d = self.dialog().set_title("Save data as CSV").add_filter("CSV", &["csv"]).set_file_name(format!("{}.csv", stem));
        let path = match d.save_file() { Some(p) => p, None => return };
        match std::fs::write(&path, text) {
            Ok(_) => { self.audit.log("DATA_EXPORTED", &path.display().to_string()); self.status = format!("Saved {}", path.display()); }
            Err(e) => self.status = format!("Saving the data failed: {}", e),
        }
    }

    /// Crops the window screenshot to the plot rectangle and writes it as PNG.
    fn save_png(&mut self, ctx: &egui::Context, img: &egui::ColorImage, cap: Capture) {
        let ppp = ctx.pixels_per_point();
        let (iw, ih) = (img.size[0], img.size[1]);
        let px = |v: f32, max: usize| ((v * ppp).round().max(0.0) as usize).min(max);
        let (x0, y0, x1, y1) = (px(cap.rect.min.x, iw), px(cap.rect.min.y, ih), px(cap.rect.max.x, iw), px(cap.rect.max.y, ih));
        if x1 <= x0 || y1 <= y0 { self.status = "Capture failed: the plot is not visible.".into(); return; }
        let mut rgb = Vec::with_capacity((x1 - x0) * (y1 - y0) * 3);
        for y in y0..y1 { for x in x0..x1 { let p = img.pixels[y * iw + x]; rgb.extend_from_slice(&[p.r(), p.g(), p.b()]); } }
        match png::encode_rgb((x1 - x0) as u32, (y1 - y0) as u32, &rgb) {
            Ok(bytes) => match std::fs::write(&cap.path, bytes) {
                Ok(_) => { self.audit.log("FIGURE_EXPORTED", &cap.path.display().to_string()); self.status = format!("Saved {}", cap.path.display()); }
                Err(e) => self.status = format!("Saving the figure failed: {}", e),
            },
            Err(e) => self.status = format!("PNG encoding failed: {}", e),
        }
    }

    /// Sets the sample type. If `file` is one of several selected files, ALL selected files get the type; otherwise only `file`.
    fn set_type(&mut self, file: usize, kind: Kind) {
        if file >= self.samples.len() { return; }
        let group: Vec<usize> = if self.samples[file].show && self.shown().len() > 1 { self.shown() } else { vec![file] };
        if group.iter().all(|&f| self.samples[f].kind == kind) { return; }
        self.push_undo("sample type");
        for &f in &group { self.samples[f].kind = kind; }
        for a in 0..self.analytes.len() { self.refit(a); }
        let names: Vec<String> = group.iter().map(|&f| self.samples[f].data.name.clone()).collect();
        self.audit.log("SAMPLE_TYPE", &format!("type={} files={}", kind.label(), names.join(",")));
        if group.len() > 1 { self.status = format!("{} selected files set to {}", group.len(), kind.label()); }
    }

    // ---------------- calibration setup (standards per target, replicates) ----------------
    /// Gives every selected file the concentration `conc` for `target` (conc = 0 removes it) and marks them as standards.
    fn assign_standards(&mut self, target: usize, conc: f64) {
        let files = self.shown();
        if files.is_empty() || target >= self.analytes.len() { return; }
        self.push_undo(if conc > 0.0 { "assign standards" } else { "remove concentrations" });
        for &f in &files {
            let sm = &mut self.samples[f];
            if let Some(c) = sm.cells.get_mut(target) { c.conc = conc; }
            if conc > 0.0 { sm.kind = Kind::Standard; }
        }
        self.refit(target);
        let names: Vec<String> = files.iter().map(|&f| self.samples[f].data.name.clone()).collect();
        self.audit.log("STANDARDS_ASSIGNED", &format!("target={} conc={} files={}", self.analytes[target].name, conc, names.join(",")));
        self.status = format!("{} file(s): {} = {} {}", files.len(), self.analytes[target].name, fmt::sci(conc), self.units);
    }

    /// Calibration setup tab: assign standard concentrations per target and file.
    fn ui_setup(&mut self, ui: &mut egui::Ui) {
        if self.analytes.is_empty() { ui.label("Load a target table first."); return; }
        ui.heading("Calibration setup");
        ui.weak("Each target has its own calibration. A standard file carries a concentration per target (blank = not a standard for that target). \
                 Replicates simply share the same concentration; all replicates are used unless you exclude one in the calibration table (Chromatograms tab).");
        if self.assign_target >= self.analytes.len() { self.assign_target = 0; }
        let n_sel = self.shown().len();
        let (mut assign, mut clear) = (false, false);
        ui.horizontal(|ui| {
            ui.label(format!("{} selected file(s) →", n_sel));
            ui.label("target");
            egui::ComboBox::new("assign_t", "").selected_text(self.analytes[self.assign_target].name.clone()).show_ui(ui, |ui| {
                for i in 0..self.analytes.len() { let n = self.analytes[i].name.clone(); ui.selectable_value(&mut self.assign_target, i, n); }
            });
            ui.label("concentration");
            ui.add(dv(&mut self.assign_conc).speed(0.1).range(0.0..=1e9));
            ui.label(self.units.clone());
            if ui.add_enabled(n_sel > 0 && self.assign_conc > 0.0, egui::Button::new("Assign as standards")).clicked() { assign = true; }
            if ui.add_enabled(n_sel > 0, egui::Button::new("Remove concentration")).clicked() { clear = true; }
        });
        ui.weak("Changing the type of a file that is part of a multi-selection sets all selected files to that type.");
        ui.weak("Only files of type Standard are listed below.");
        ui.weak("Tip: select the replicate files in the sample list (Ctrl/Shift+click), pick the target and the level, press Assign; repeat for the next level.");
        if assign { self.assign_standards(self.assign_target, self.assign_conc); }
        if clear { self.assign_standards(self.assign_target, 0.0); }
        ui.separator();
        let (na, ns) = (self.analytes.len(), self.samples.len());
        let std_files: Vec<usize> = (0..ns).filter(|&s| self.samples[s].kind == Kind::Standard).collect();
        if std_files.is_empty() { ui.label("No files are marked as Standard yet: select files in the sample list and press 'Assign as standards' (or set their type to Standard)."); }
        let mut change: Option<(usize, usize, f64)> = None;
        let mut kind_change: Option<(usize, Kind)> = None;
        egui::ScrollArea::both().id_salt("setup").show(ui, |ui| {
            egui::Grid::new("setupgrid").striped(true).show(ui, |ui| {
                ui.strong("File"); ui.strong("Type");
                for an in &self.analytes { ui.strong(format!("{} ({})", an.name, self.units)); }
                ui.end_row();
                for &s in &std_files {
                    ui.label(&self.samples[s].data.name);
                    let mut k = self.samples[s].kind;
                    egui::ComboBox::new(("setupkind", s), "").selected_text(k.label()).show_ui(ui, |ui| {
                        for kk in Kind::ALL { ui.selectable_value(&mut k, kk, kk.label()); }
                    });
                    if k != self.samples[s].kind { kind_change = Some((s, k)); }
                    for a in 0..na {
                        let old = self.samples[s].cells.get(a).map(|c| c.conc).unwrap_or(0.0);
                        let mut v = old;
                        if ui.add(dv_conc(&mut v).speed(0.1).range(0.0..=1e9)).changed() && v != old { change = Some((s, a, v)); }
                    }
                    ui.end_row();
                }
            });
        });
        let dragging = ui.ctx().input(|i| i.pointer.any_down());
        if let Some((s, a, v)) = change {
            if !(dragging && self.last_undo_label == "concentration") { self.push_undo("concentration"); }
            if let Some(c) = self.samples[s].cells.get_mut(a) { c.conc = v; }
            if v > 0.0 && self.samples[s].kind == Kind::Unknown { self.samples[s].kind = Kind::Standard; }
            self.refit(a);
            self.audit.log("CONCENTRATION_SET", &format!("file={} target={} conc={}", self.samples[s].data.name, self.analytes[a].name, v));
        }
        if let Some((s, k)) = kind_change { self.set_type(s, k); }
    }

    // ---------------- preferences ----------------
    /// Preferences window: converter location, project options and folders.
    fn ui_prefs(&mut self, ctx: &egui::Context) {
        if !self.show_prefs { return; }
        let mut open = true;
        let (mut browse, mut test, mut save) = (false, false, false);
        let (mut browse_dir, mut reset_dir, mut save_dirs) = (false, false, false);
        let default_dir_text = paths::default_dir_path().to_string_lossy().to_string();
        egui::Window::new("Preferences").open(&mut open).default_width(660.0).show(ctx, |ui| {
            ui.heading("Thermo .raw reader");
            ui.label("ThermoRawFileParser is only needed for Thermo .raw files; mzML files are read directly.");
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.trfp_path).desired_width(430.0)
                    .hint_text("path to ThermoRawFileParser.exe (empty = search automatically)"));
                if r.lost_focus() { save = true; }
                if ui.button("Browse…").clicked() { browse = true; }
                if ui.button("Test").clicked() { test = true; }
            });
            match rawconv::find_trfp(&self.trfp_path) {
                Some(p) => { ui.colored_label(Color32::GREEN, format!("found: {}", p.display())); }
                None => { ui.colored_label(Color32::RED, "not found"); }
            }
            ui.separator();
            ui.heading("Projects");
            ui.checkbox(&mut self.embed_raw, "Embed raw files when saving projects");
            ui.separator();
            ui.heading("Folders");
            ui.label("Standard folder for projects, results and figures (file dialogs start here):");
            ui.horizontal(|ui| {
                let mut text = self.data_dir.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(430.0).hint_text(default_dir_text.clone()));
                if r.changed() {
                    self.data_dir = if text.trim().is_empty() { None } else { Some(PathBuf::from(text.trim())) };
                }
                if r.lost_focus() { save_dirs = true; }
                if ui.button("Browse…").clicked() { browse_dir = true; }
                if ui.button("Use default").clicked() { reset_dir = true; }
            });
            ui.weak(format!("Default when empty: {}", default_dir_text));
            ui.weak("Settings, the audit trail and temporary files are always kept in the default folder; nothing else is written unless you save or export.");
        });
        let was_open = self.show_prefs;
        self.show_prefs = open;
        if browse {
            if let Some(p) = rfd::FileDialog::new().set_title("Locate ThermoRawFileParser").add_filter("ThermoRawFileParser", &["exe", "dll"]).pick_file() {
                self.trfp_path = p.to_string_lossy().to_string();
                save = true;
            }
        }
        if browse_dir {
            if let Some(p) = rfd::FileDialog::new().set_title("Choose the standard folder").set_directory(self.standard_dir_path()).pick_folder() {
                self.data_dir = Some(p);
                save_dirs = true;
            }
        }
        if reset_dir {
            self.data_dir = None;
            save_dirs = true;
        }
        if save_dirs {
            self.write_settings();
            self.audit.log("STANDARD_FOLDER", &self.standard_dir_path().display().to_string());
        }
        if save { self.save_trfp(); }
        if test {
            self.status = match rawconv::find_trfp(&self.trfp_path) {
                Some(p) => match rawconv::converter_version(&p) {
                    Ok(v) => format!("ThermoRawFileParser works: {}", v),
                    Err(e) => format!("ThermoRawFileParser test failed: {}", e),
                },
                None => "ThermoRawFileParser not found".into(),
            };
        }
        if was_open && !open { self.write_settings(); }
    }

    // ---------------- mzML import and quick XIC ----------------
    /// Imports `path` as mzML file or, otherwise, as Thermo `.raw` file.
    fn start_load_any(&mut self, ctx: &egui::Context, path: &str) -> bool {
        if path.to_lowercase().ends_with(".mzml") { self.start_load_mzml(ctx, path) } else { self.start_load(ctx, path) }
    }

    /// mzML files (from any vendor, e.g. converted with ProteoWizard msconvert) are parsed directly, in memory.
    fn start_load_mzml(&mut self, ctx: &egui::Context, path: &str) -> bool {
        let p = PathBuf::from(path);
        if !p.is_file() { self.status = format!("File not found: {}", path); return false; }
        self.loading += 1;
        self.status = format!("Reading {} (held in memory only)...", path);
        let (tx, ctx, ps) = (self.tx.clone(), ctx.clone(), path.to_string());
        std::thread::spawn(move || {
            match std::fs::read(&p) {
                Ok(bytes) => {
                    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    match mzml::parse_bytes(&name, &bytes) {
                        Ok(d) => {
                            let h = d.sha256.clone();
                            let _ = tx.send(Msg::Loaded(d, LoadInfo { path: ps, raw_sha: h, detail: "format=mzML (no converter)".into(), source: RawSource::Disk(p.clone()) }));
                        }
                        Err(e) => { let _ = tx.send(Msg::Err(e)); }
                    }
                }
                Err(e) => { let _ = tx.send(Msg::Err(format!("{}: {}", ps, e))); }
            }
            let _ = tx.send(Msg::Finished);
            ctx.request_repaint();
        });
        true
    }

    /// Turns the m/z of the "custom m/z" view into a permanent target (MS2 level: product ion, MS1 level: precursor).
    fn add_quick_target(&mut self) {
        let mz = self.quick_mz;
        if mz <= 0.0 { return; }
        self.push_undo("add target");
        let n = self.analytes.len();
        let prec = if self.quick_level == 1 { Some(mz) } else { None };
        self.analytes.push(Analyte { name: format!("m/z {}", fmt::mz4(mz)), rt: None, ions: vec![mz], precursor: prec });
        self.fits.push(None);
        self.recompute_analyte(n);
        self.rows_dirty = true;
        self.audit.log("TARGET_ADDED", &format!("#{} from quick XIC m/z {} MS{}", n + 1, fmt::mz4(mz), self.quick_level));
        self.status = format!("Added target m/z {} - edit its name and RT in Edit → Target table", fmt::mz4(mz));
    }
}

impl eframe::App for App {
    /// Draws one frame: processes messages of the background threads, shortcuts and screenshots,
    /// then lays out all panels.
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Loaded(d, info) => self.add_sample(d, info),
                Msg::Err(e) => self.status = format!("Load failed: {}", e),
                Msg::Finished => self.loading = self.loading.saturating_sub(1),
                Msg::ProjectSaved { path, result, src_paths, missing } => {
                    self.saving = self.saving.saturating_sub(1);
                    match result {
                        Ok(offs) => {
                            let (mut n, mut bytes) = (0usize, 0u64);
                            for (sp, o) in src_paths.iter().zip(offs.iter()) {
                                if let Some((off, len)) = o {
                                    n += 1;
                                    bytes += *len;
                                    for s in self.samples.iter_mut().filter(|s| &s.src_path == sp) {
                                        s.source = RawSource::Embedded { project: path.clone(), offset: *off, len: *len };
                                    }
                                }
                            }
                            self.audit.log("PROJECT_SAVED", &format!("{} embedded_raw_files={} embedded_bytes={}", path.display(), n, bytes));
                            let mut msg = format!("Saved project {} ({} raw file(s) embedded, {:.1} MB)", path.display(), n, bytes as f64 / 1e6);
                            if !missing.is_empty() { msg += &format!("  ⚠ not embedded (original file not found): {}", missing.join(", ")); }
                            self.status = msg;
                            self.project_path = Some(path);
                        }
                        Err(e) => self.status = format!("Saving the project failed: {}", e),
                    }
                }
            }
        }
        if self.loading == 0 { if let Some(p) = self.pending.take() { self.finish_project(p); } }
        for f in ctx.input(|i| i.raw.dropped_files.clone()) {
            if let Some(p) = f.path { self.load_path(ctx, &p.to_string_lossy()); }
        }
        let shot: Option<Arc<egui::ColorImage>> = ctx.input(|i| i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        }));
        match self.capture.as_ref().map(|c| (c.sent, c.frames)) {
            Some((true, _)) => if let Some(img) = shot { if let Some(cap) = self.capture.take() { self.save_png(ctx, &img, cap); } },
            Some((false, 0)) => {
                if let Some(c) = self.capture.as_mut() { c.sent = true; }
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
                ctx.request_repaint();
            }
            Some((false, _)) => { if let Some(c) = self.capture.as_mut() { c.frames -= 1; } ctx.request_repaint(); }
            None => {}
        }
        self.shortcuts(ctx);
        self.fix_selection();
        if self.sel_a >= self.analytes.len() { self.sel_a = 0; }
        if self.align_ref >= self.samples.len() { self.align_ref = 0; }

        let title = format!("msQuant - {}", self.project_path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "untitled project".into()));
        if title != self.title { ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone())); self.title = title; }

        egui::TopBottomPanel::top("menu").show(ctx, |ui| { self.menu(ui, ctx); });

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("User:");
                let r = ui.add(egui::TextEdit::singleline(&mut self.audit.user).desired_width(110.0));
                if r.lost_focus() {
                    if self.audit.user.trim().is_empty() { self.audit.user = "N/A".into(); }
                    if self.audit.user != self.user_logged {
                        let msg = format!("{} -> {}", self.user_logged, self.audit.user);
                        self.audit.log("USER_CHANGED", &msg);
                        self.user_logged = self.audit.user.clone();
                        self.write_settings();
                    }
                }
                ui.separator();
                ui.label("Results file:");
                let default_results = self.standard_dir_path().join("results.csv").to_string_lossy().to_string();
                let r = ui.add(egui::TextEdit::singleline(&mut self.out_path).desired_width(220.0).hint_text(default_results))
                    .on_hover_text("Click to choose the output file in the file browser (default: the standard folder from the preferences)");
                if r.clicked() { self.choose_out_path(); }
                if ui.button("Export CSV").clicked() {
                    self.status = match self.export() { Ok(_) => format!("Exported {}", self.export_path().display()), Err(e) => format!("Export failed: {}", e) };
                }
                if self.loading > 0 || self.saving > 0 { ui.spinner(); }
            });
            ui.horizontal(|ui| {
                let mut ch = false;
                egui::ComboBox::new("tolu", "").selected_text(if self.tol_ppm { "ppm" } else { "Da" }).show_ui(ui, |ui| {
                    ch |= ui.selectable_value(&mut self.tol_ppm, true, "ppm").changed();
                    ch |= ui.selectable_value(&mut self.tol_ppm, false, "Da").changed();
                });
                ui.label("m/z tol ±");
                ch |= ui.add(dv(&mut self.tol).speed(0.1).range(0.001..=1000.0)).changed();
                ui.label("RT window ± (min)");
                ch |= ui.add(dv_rt(&mut self.rt_win).speed(0.01).range(0.01..=60.0)).changed();
                ui.label("Weighting");
                egui::ComboBox::new("wt", "").selected_text(WEIGHTS[self.weighting as usize]).show_ui(ui, |ui| {
                    for (i, w) in WEIGHTS.iter().enumerate() { ch |= ui.selectable_value(&mut self.weighting, i as u8, *w).changed(); }
                });
                ch |= ui.checkbox(&mut self.baseline, "Linear baseline subtraction").changed();
                ui.label("Ion ratio tol ±%");
                ch |= ui.add(dv(&mut self.ratio_tol).range(1.0..=100.0)).changed();
                ui.label("Units");
                ui.add(egui::TextEdit::singleline(&mut self.units).desired_width(60.0));
                if ch { self.settings_dirty = true; }
                if ui.add_enabled(self.settings_dirty, egui::Button::new("Apply & re-integrate")).clicked() { self.apply_settings(); }
                if self.settings_dirty { ui.colored_label(Color32::YELLOW, "settings changed - not applied"); }
            });
            ui.horizontal(|ui| {
                ui.label("Quantification:");
                let before = self.mode;
                egui::ComboBox::new("mode", "").selected_text(if self.mode == Mode::Areas { "Integrated areas only (a.u.)" } else { "Standard curve" }).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mode, Mode::Areas, "Integrated areas only (a.u.)");
                    ui.selectable_value(&mut self.mode, Mode::Curve, "Standard curve");
                });
                if self.mode != before { self.audit.log("QUANT_MODE", if self.mode == Mode::Areas { "integrated areas only" } else { "standard curve" }); }
                ui.label("Signal:");
                let mut nb = self.basis;
                egui::ComboBox::new("basis", "").selected_text(if self.basis == Basis::Ms2 { "MS2 product-ion area" } else { "MS1 precursor area" }).show_ui(ui, |ui| {
                    ui.selectable_value(&mut nb, Basis::Ms2, "MS2 product-ion area");
                    ui.selectable_value(&mut nb, Basis::Ms1, "MS1 precursor area");
                });
                if nb != self.basis { self.set_basis(nb); }
                ui.separator();
                if ui.checkbox(&mut self.align_on, "Align RT on import").changed() { self.audit.log("ALIGN_SETTING", &format!("align_on_import={}", self.align_on)); }
                ui.label("max shift ±(min)");
                ui.add(dv_rt(&mut self.align_max).speed(0.05).range(0.1..=10.0));
                ui.label("reference:");
                egui::ComboBox::new("aref", "")
                    .selected_text(self.samples.get(self.align_ref).map(|s| s.data.name.clone()).unwrap_or_else(|| "-".into()))
                    .show_ui(ui, |ui| { for i in 0..self.samples.len() { ui.selectable_value(&mut self.align_ref, i, &self.samples[i].data.name); } });
                if ui.button("Align all now").clicked() { self.align_all(); }
                if ui.button("Clear alignment").clicked() { self.clear_alignment(); }
                ui.separator();
                if ui.button("Targets…").clicked() { self.show_targets = true; }
                ui.weak(format!("zoom {}%", (self.zoom * 100.0).round()));
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| { ui.label(&self.status); });

        egui::SidePanel::left("samples").min_width(400.0).show(ctx, |ui| {
            ui.heading("Samples");
            ui.horizontal(|ui| {
                if ui.small_button("Select all").clicked() { for s in self.samples.iter_mut() { s.show = true; } }
                if ui.small_button("Select none").clicked() { for s in self.samples.iter_mut() { s.show = false; } }
            });
            ui.weak("Click = select · Ctrl+click = add/remove · Shift+click = range · right-click = menu · changing the type of a selected file changes all selected files");
            let mut kind_set: Option<(usize, Kind)> = None;
            let mut click: Option<(usize, bool, bool)> = None;
            let mut remove: Option<usize> = None;
            let mut set_ref: Option<usize> = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, s) in self.samples.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut s.show, "");
                        let label = if i == self.sel_s && s.show { RichText::new(&s.data.name).strong() } else { RichText::new(&s.data.name) };
                        let resp = ui.selectable_label(s.show, label);
                        if resp.clicked() { let m = ui.input(|inp| inp.modifiers); click = Some((i, m.command, m.shift)); }
                        resp.context_menu(|ui| {
                            if ui.button("Remove from project (file stays on disk)").clicked() { remove = Some(i); ui.close_menu(); }
                            if ui.button("Select only this file").clicked() { click = Some((i, false, false)); ui.close_menu(); }
                            if ui.button("Use as alignment reference").clicked() { set_ref = Some(i); ui.close_menu(); }
                        });
                        let mut kk = s.kind;
                        egui::ComboBox::new(("kind", i), "").selected_text(s.kind.label()).show_ui(ui, |ui| {
                            for k in Kind::ALL { ui.selectable_value(&mut kk, k, k.label()); }
                        });
                        if kk != s.kind { kind_set = Some((i, kk)); }
                        if s.rt_shift != 0.0 { ui.weak(format!("Δ{:+.2} min", s.rt_shift)); }
                    });
                }
            });
            if let Some((i, k)) = kind_set { self.set_type(i, k); }
            if let Some((i, ctrl, shift)) = click { self.apply_click(i, ctrl, shift); }
            if let Some(i) = set_ref { self.align_ref = i; self.audit.log("ALIGN_REFERENCE", &self.samples[i].data.name); }
            if let Some(i) = remove { self.remove_sample(i); }
        });

        if self.show_spectrum && self.tab == Tab::Chrom && !self.samples.is_empty() {
            egui::TopBottomPanel::bottom("spectrum").resizable(true).default_height(270.0).min_height(120.0).show(ctx, |ui| { self.ui_spectrum(ui); });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.samples.is_empty() {
                ui.heading("Drop Thermo .raw or mzML files (and optionally a .csv target table: name, RT|n/a, ion m/z values) onto this window.");
                ui.label("Or use File → Import / Open. Files are read via ThermoRawFileParser and kept in memory only; no converted files are written to disk.");
                return;
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Chrom, "Chromatograms");
                ui.selectable_value(&mut self.tab, Tab::Matrix, "Results matrix (all files)");
                ui.selectable_value(&mut self.tab, Tab::Setup, "Calibration setup");
            });
            ui.separator();
            let prev_hover = self.hover;
            self.hover = None;
            match self.tab { Tab::Chrom => self.ui_chrom(ui), Tab::Matrix => self.ui_matrix(ui), Tab::Setup => self.ui_setup(ui) }
            if self.hover != prev_hover { ui.ctx().request_repaint(); }
        });

        self.ui_targets_window(ctx);
        self.ui_prefs(ctx);
        if self.show_credits {
            let mut open = true;
            egui::Window::new("Credits & third-party software").open(&mut open).default_width(640.0).show(ctx, |ui| { ui.label(CREDITS); });
            self.show_credits = open;
        }
        if self.show_readme {
            let mut open = true;
            egui::Window::new("README (offline)").open(&mut open).default_size([760.0, 560.0]).show(ctx, |ui| {
                egui::ScrollArea::both().show(ui, |ui| { ui.monospace(README); });
            });
            self.show_readme = open;
        }
        if self.show_audit {
            let mut open = true;
            egui::Window::new("Audit trail (hash-chained)").open(&mut open).default_size([900.0, 400.0]).show(ctx, |ui| {
                egui::ScrollArea::both().stick_to_bottom(true).show(ui, |ui| {
                    for l in self.audit.recent.iter().rev().take(300).rev() { ui.monospace(l); }
                });
            });
            self.show_audit = open;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    fn app() -> App {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!("lcmsq_app_test_{}_{}.log", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_file(&p);
        let mut a = App::new_with_audit(&p.to_string_lossy());
        a.trfp_path.clear();
        a
    }
    fn info(n: usize) -> LoadInfo { LoadInfo { path: format!("synthetic{}.raw", n), raw_sha: "0".repeat(64), detail: String::new(), source: RawSource::Disk(PathBuf::from(format!("synthetic{}.raw", n))) } }
    fn add(a: &mut App, n: usize, offset: f64, scale: f64) {
        let d = mzml::parse_bytes(&format!("run{}.raw", n), &testkit::synthetic_mzml(offset, scale)).expect("synthetic mzML parses");
        a.add_sample(d, info(n));
    }
    fn targets(a: &mut App) {
        a.analytes = quant::parse_table("name,rt,ions\nSynth,5.0,150;250\n").unwrap();
        a.recompute_all();
    }
    fn shown(a: &App) -> Vec<usize> { a.shown() }

    #[test]
    fn explorer_style_selection() {
        let mut a = app();
        for i in 0..4 { add(&mut a, i, 0.0, 1.0); }
        a.apply_click(1, false, false);
        assert_eq!(shown(&a), vec![1]);
        a.apply_click(3, true, false);
        assert_eq!(shown(&a), vec![1, 3]);
        a.apply_click(0, false, true); // range from anchor(3) to 0
        assert_eq!(shown(&a), vec![0, 1, 2, 3]);
        a.apply_click(2, true, false); // toggle off
        assert_eq!(shown(&a), vec![0, 1, 3]);
        assert!(a.cur().is_some());
    }

    #[test]
    fn no_selection_means_no_current_file() {
        let mut a = app();
        for i in 0..2 { add(&mut a, i, 0.0, 1.0); }
        for s in a.samples.iter_mut() { s.show = false; }
        a.fix_selection();
        assert!(a.cur().is_none());
        assert!(a.shown().is_empty());
    }

    #[test]
    fn remove_file_keeps_disk_untouched_and_is_undoable() {
        let mut a = app();
        for i in 0..3 { add(&mut a, i, 0.0, 1.0); }
        a.remove_sample(1);
        assert_eq!(a.samples.len(), 2);
        assert_eq!(a.samples[1].data.name, "run2.raw");
        a.undo();
        assert_eq!(a.samples.len(), 3);
        assert_eq!(a.samples[1].data.name, "run1.raw");
        a.redo();
        assert_eq!(a.samples.len(), 2);
    }

    #[test]
    fn undo_history_is_limited_to_ten_steps() {
        let mut a = app();
        targets(&mut a);
        for _ in 0..12 { a.add_target(); }
        assert_eq!(a.analytes.len(), 13);
        for _ in 0..20 { a.undo(); }
        assert_eq!(a.analytes.len(), 3, "only 10 of the 12 additions can be undone");
        for _ in 0..20 { a.redo(); }
        assert_eq!(a.analytes.len(), 13);
    }

    #[test]
    fn live_target_editing_recomputes_areas() {
        let mut a = app();
        add(&mut a, 0, 0.0, 1.0);
        targets(&mut a);
        let first = a.samples[0].cells[0].areas[0];
        assert!(first > 0.0);
        a.sync_rows();
        a.rows[0][3] = "400".into(); // constant 10-count ion: different area
        a.apply_row(0);
        assert_eq!(a.analytes[0].ions, vec![400.0]);
        assert!((a.samples[0].cells[0].areas[0] - first).abs() > 1.0);
        a.rows[0][3] = "".into(); // invalid: must not be applied
        a.apply_row(0);
        assert_eq!(a.analytes[0].ions, vec![400.0]);
        a.add_target();
        assert_eq!(a.analytes.len(), 2);
        assert_eq!(a.samples[0].cells.len(), 2);
        a.delete_target(0);
        assert_eq!(a.analytes.len(), 1);
        assert_eq!(a.samples[0].cells.len(), 1);
        let csv_text = a.targets_csv();
        assert!(quant::parse_table(&csv_text).is_ok());
    }

    #[test]
    fn standard_curve_quantifies_a_known_sample() {
        let mut a = app();
        targets(&mut a);
        for (i, s) in [1.0, 2.0, 4.0, 8.0].iter().enumerate() {
            add(&mut a, i, 0.0, *s);
            a.samples[i].kind = Kind::Standard;
            a.samples[i].cells[0].conc = *s;
        }
        add(&mut a, 9, 0.0, 3.0);
        for k in 0..a.analytes.len() { a.refit(k); }
        let f = a.fits[0].expect("calibration fitted");
        assert!(f.r2 > 0.9999, "r2 = {}", f.r2);
        let c = a.conc_of(4, 0).expect("unknown quantified");
        assert!((c - 3.0).abs() / 3.0 < 0.02, "got {}", c);
        a.mode = Mode::Areas;
        assert!(a.conc_of(4, 0).is_none(), "areas-only mode reports no concentrations");
        assert!(a.samples[4].cells[0].areas[0] > 0.0);
    }

    #[test]
    fn ion_ratio_and_area_are_close_to_analytical_values() {
        let mut a = app();
        targets(&mut a);
        add(&mut a, 0, 0.0, 2.0);
        assert!(a.samples[0].cells[0].found);
        let expect = testkit::expected_area_seconds(2.0);
        let with_baseline = a.samples[0].cells[0].areas[0];
        // Without baseline subtraction only the tails beyond the automatic window (5 % of the apex) are missing.
        a.baseline = false;
        a.recompute_all();
        let c = &a.samples[0].cells[0];
        assert!((c.areas[0] - expect).abs() / expect < 0.03, "area {} vs {}", c.areas[0], expect);
        assert!((c.areas[1] / c.areas[0] - 0.5).abs() < 0.01);
        // With baseline subtraction the 5 % pedestal at the window edges is removed as well: lower, but not by much.
        assert!(with_baseline < c.areas[0] && with_baseline > 0.8 * expect, "{} vs {}", with_baseline, expect);
    }

    #[test]
    fn alignment_on_import_corrects_a_shifted_run() {
        let mut a = app();
        a.align_on = true;
        targets(&mut a);
        add(&mut a, 0, 0.0, 1.0);
        add(&mut a, 1, 0.2, 1.0);
        assert!((a.samples[1].rt_shift + 0.2).abs() < 0.02, "shift {}", a.samples[1].rt_shift);
        // after alignment both runs integrate to the same area
        let (x, y) = (a.samples[0].cells[0].areas[0], a.samples[1].cells[0].areas[0]);
        assert!((x - y).abs() / x < 0.03);
    }

    #[test]
    fn project_round_trip_keeps_manual_integration() {
        let mut a = app();
        targets(&mut a);
        add(&mut a, 0, 0.0, 1.0);
        a.samples[0].kind = Kind::Standard;
        a.samples[0].cells[0].conc = 5.0;
        a.apply_edit(0, 0, 4.9, 5.1);
        a.commit(0, 0);
        let pf = a.project_file();
        let back = ProjectFile::parse(&pf.to_text()).unwrap();
        assert_eq!(pf, back);
        assert!(back.samples[0].cells[0].manual);
        let mut b = app();
        b.apply_params(&back.params);
        assert_eq!(b.weighting, a.weighting);
        assert_eq!(b.tol, a.tol);
    }

    #[test]
    fn manual_integration_is_audited_with_old_and_new_values() {
        let mut a = app();
        targets(&mut a);
        add(&mut a, 0, 0.0, 1.0);
        a.reason = "test".into();
        a.apply_edit(0, 0, 4.8, 5.2);
        a.commit(0, 0);
        assert!(a.audit.recent.iter().any(|l| l.contains("MANUAL_INTEGRATION") && l.contains("reason=test")));
        assert!(audit::verify(&a.audit.path).is_ok());
    }

    #[test]
    fn ms1_basis_quantifies_the_precursor_signal() {
        let mut a = app();
        a.analytes = quant::parse_table("name,rt,ions\nSynth,5.0,150;250;p:500\n").unwrap();
        a.recompute_all();
        add(&mut a, 0, 0.0, 1.0);
        let ms2 = a.samples[0].cells[0].areas[0];
        assert_eq!(a.samples[0].xics[0].len(), 2);
        a.set_basis(Basis::Ms1);
        assert_eq!(a.samples[0].xics[0].len(), 1, "one precursor trace");
        let ms1 = a.samples[0].cells[0].areas[0];
        let expect = 50000.0 * 0.1 * (2.0 * std::f64::consts::PI).sqrt() * 60.0;
        assert!((ms1 - expect).abs() / expect < 0.15, "MS1 area {} vs {}", ms1, expect);
        assert!((ms1 - ms2).abs() > 1.0, "the two signals are different measurements");
        a.undo();
        assert!(a.basis == Basis::Ms2);
        assert_eq!(a.samples[0].xics[0].len(), 2);
    }

    #[test]
    fn target_without_precursor_gives_no_peak_on_ms1_basis() {
        let mut a = app();
        targets(&mut a);
        add(&mut a, 0, 0.0, 1.0);
        a.set_basis(Basis::Ms1);
        assert!(!a.samples[0].cells[0].found);
        assert!(a.status.contains("precursor"));
    }

    #[test]
    fn hover_picks_the_nearest_trace_and_point() {
        let mk = |name: &str, y: f64, ci: usize| Series {
            name: name.into(), data: (0..100).map(|i| [i as f64 * 0.1, y]).collect(), ci, cur: false, file: 0, level: 2,
            fname: "f".into(), desc: name.into(), scale: 1.0,
        };
        let s = vec![mk("low", 10.0, 0), mk("high", 1000.0, 1)];
        let h = nearest_hover(&s, 5.04, 900.0, 10.0).unwrap();
        assert_eq!((h.desc.as_str(), h.idx), ("high", 50));
        assert_eq!(nearest_hover(&s, 5.04, 20.0, 10.0).unwrap().desc, "low");
        assert!(nearest_hover(&s, 50.0, 10.0, 10.0).is_none(), "cursor far outside the data");
    }

    #[test]
    fn spectrum_panel_scan_selection() {
        let mut a = app();
        targets(&mut a);
        add(&mut a, 0, 0.0, 1.0);
        let i = a.apex_scan(0, 0).expect("apex scan");
        assert!((a.samples[0].data.spectra[i].rt - 5.0).abs() < 0.02);
        assert!(matches!(a.spectrum_pick(), Some((0, _, "apex of the selected target"))));
        let j = a.nearest_ms2(0, 5.0).unwrap();
        assert!((a.samples[0].data.spectra[j].rt - 5.0).abs() <= 0.005 + 1e-9);
        a.pin_scan(0, 7.0);
        assert!(matches!(a.spectrum_pick(), Some((0, _, "pinned"))));
        a.hover = Some((0, 2.0));
        assert!(matches!(a.spectrum_pick(), Some((0, _, "pinned"))), "hover is ignored by default");
        a.follow_hover = true;
        assert!(matches!(a.spectrum_pick(), Some((0, _, "hovered"))));
        a.follow_hover = false;
        assert!(matches!(a.spectrum_pick(), Some((0, _, "pinned"))));
    }

    #[test]
    fn spectrum_panel_does_not_follow_hover_by_default() {
        assert!(!app().follow_hover);
    }

    #[test]
    fn several_targets_each_have_their_own_standards_and_replicates() {
        let mut a = app();
        a.analytes = quant::parse_table("name,rt,ions\nMorph,5.0,150\nFent,5.0,250\n").unwrap();
        a.recompute_all();
        // Morph standards: levels 1, 2, 2 (replicate), 4;  Fent standards: levels 1, 2, 4;  one unknown with scale 3
        let morph = [1.0, 2.0, 2.0, 4.0];
        let fent = [1.0, 2.0, 4.0];
        for (i, s) in morph.iter().enumerate() { add(&mut a, i, 0.0, *s); a.samples[i].cells[0].conc = *s; a.samples[i].kind = Kind::Standard; }
        for (j, s) in fent.iter().enumerate() { let i = morph.len() + j; add(&mut a, i, 0.0, *s); a.samples[i].cells[1].conc = *s; a.samples[i].kind = Kind::Standard; }
        let u = morph.len() + fent.len();
        add(&mut a, u, 0.0, 3.0);
        for k in 0..2 { a.refit(k); }
        assert_eq!(a.fits[0].unwrap().n, 4, "all Morph replicates are used by default");
        assert_eq!(a.fits[1].unwrap().n, 3, "Fent standards do not leak into the Morph curve and vice versa");
        let (cm, cf) = (a.conc_of(u, 0).unwrap(), a.conc_of(u, 1).unwrap());
        assert!((cm - 3.0).abs() / 3.0 < 0.02, "Morph {}", cm);
        assert!((cf - 3.0).abs() / 3.0 < 0.02, "Fent {}", cf);
        // exclude one replicate of the level-2 pair
        a.samples[2].cells[0].include = false;
        a.refit(0);
        assert_eq!(a.fits[0].unwrap().n, 3);
        let c2 = a.conc_of(u, 0).unwrap();
        assert!((c2 - 3.0).abs() / 3.0 < 0.02);
        assert_eq!(a.fits[1].unwrap().n, 3, "other target unaffected");
    }

    #[test]
    fn assigning_a_replicate_group_with_one_action() {
        let mut a = app();
        a.analytes = quant::parse_table("name,rt,ions\nMorph,5.0,150\nFent,5.0,250\n").unwrap();
        a.recompute_all();
        for i in 0..4 { add(&mut a, i, 0.0, 1.0 + i as f64); }
        a.apply_click(0, false, false);
        a.apply_click(2, false, true); // files 0..=2
        a.assign_standards(1, 10.0);
        for i in 0..3 { assert!(a.samples[i].kind == Kind::Standard && a.samples[i].cells[1].conc == 10.0 && a.samples[i].cells[0].conc == 0.0); }
        assert!(a.samples[3].kind == Kind::Unknown && a.samples[3].cells[1].conc == 0.0);
        a.undo();
        assert!(a.samples[0].kind == Kind::Unknown && a.samples[0].cells[1].conc == 0.0);
    }

    #[test]
    fn concentrations_survive_a_project_round_trip_and_old_projects_load() {
        let mut a = app();
        targets(&mut a);
        add(&mut a, 0, 0.0, 1.0);
        a.samples[0].cells[0].conc = 7.5;
        let pf = a.project_file();
        let back = ProjectFile::parse(&pf.to_text()).unwrap();
        assert_eq!(back.samples[0].cells[0].conc, Some(7.5));
        let old = pf.to_text().replace("\ttrue\t7.5\n", "\ttrue\n");
        assert_eq!(ProjectFile::parse(&old).unwrap().samples[0].cells[0].conc, None, "older projects have no per-target concentration");
    }

    #[test]
    fn quick_xic_traces_a_custom_mz_without_a_target() {
        let mut a = app();
        add(&mut a, 0, 0.0, 2.0);
        a.quick_mz = testkit::FRAG1;
        a.quick_level = 2;
        let x = a.quick_trace(0);
        let apex = x.iter().cloned().fold([0.0, 0.0], |m, p| if p[1] > m[1] { p } else { m });
        assert!((apex[0] - 5.0).abs() < 0.02 && (apex[1] - 2000.0).abs() < 25.0, "{:?}", apex);
        a.quick_mz = 500.0;
        a.quick_level = 1;
        assert!(a.quick_trace(0).iter().any(|p| p[1] > 40000.0), "MS1 trace of the MS1 ion");
        let before = a.analytes.len();
        a.add_quick_target();
        assert_eq!(a.analytes.len(), before + 1);
        assert_eq!(a.analytes[before].precursor, Some(500.0));
        assert!(a.samples[0].cells.len() == a.analytes.len());
    }

    #[test]
    fn precursor_of_each_ms2_scan_is_available() {
        let mut a = app();
        add(&mut a, 0, 0.0, 1.0);
        let sp = &a.samples[0].data.spectra;
        assert_eq!(sp[0].precursor, Some(testkit::PRECURSOR), "selected ion m/z wins over the isolation window target");
        assert_eq!(prec_str(sp[0].precursor), "286.1438");
        assert_eq!(prec_str(None), "n/a");
        assert!(a.samples[0].data.ms1.iter().all(|s| s.precursor.is_none()));
        assert!(scan_info(&a.samples[0].data, 2, 0).contains("precursor m/z 286.1438"));
        assert!(scan_info(&a.samples[0].data, 1, 0).starts_with("MS1 scan"));
    }

    #[test]
    fn cursor_snaps_to_the_nearest_peak_and_top_peaks_are_ranked() {
        let (mz, inten) = (vec![100.0, 150.0, 200.0, 250.0], vec![10.0f32, 100.0, 50.0, 70.0]);
        let (xw, yw) = (200.0, 100.0);
        assert_eq!(nearest_peak(&mz, &inten, 1.0, 151.0, 50.0, xw, yw), Some((150.0, 100.0)));
        assert_eq!(nearest_peak(&mz, &inten, 1.0, 197.0, 60.0, xw, yw), Some((200.0, 50.0)));
        assert_eq!(nearest_peak(&mz, &inten, 0.5, 250.0, 10.0, xw, yw), Some((250.0, 35.0)), "scaled intensity is returned");
        assert_eq!(nearest_peak(&mz, &inten, 1.0, 900.0, 10.0, xw, yw), None, "nothing near the cursor");
        let t = top_peaks(&mz, &inten, 3);
        assert_eq!(t.iter().map(|p| p.0).collect::<Vec<_>>(), vec![150.0, 250.0, 200.0]);
        assert_eq!(top_peaks(&mz, &inten, 10).len(), 4);
    }

    #[test]
    fn changing_the_type_of_a_selected_file_changes_all_selected_files() {
        let mut a = app();
        for i in 0..5 { add(&mut a, i, 0.0, 1.0); }
        a.apply_click(1, false, false);
        a.apply_click(3, false, true); // files 1..=3 selected
        assert_eq!(a.shown(), vec![1, 2, 3]);
        a.set_type(2, Kind::Standard);
        let kinds: Vec<bool> = a.samples.iter().map(|s| s.kind == Kind::Standard).collect();
        assert_eq!(kinds, vec![false, true, true, true, false], "only the selected files change");
        // a file outside the selection changes alone
        a.set_type(4, Kind::Blank);
        assert!(a.samples[4].kind == Kind::Blank && a.samples[1].kind == Kind::Standard);
        // one single selected file changes alone
        a.apply_click(0, false, false);
        a.set_type(0, Kind::QC);
        assert!(a.samples[0].kind == Kind::QC && a.samples[2].kind == Kind::Standard);
        // one undo step reverts the whole group
        a.set_type(0, Kind::Unknown);
        a.apply_click(1, false, false);
        a.apply_click(3, false, true);
        a.set_type(1, Kind::Unknown);
        assert!(a.samples[1..=3].iter().all(|s| s.kind == Kind::Unknown));
        a.undo();
        assert!(a.samples[1..=3].iter().all(|s| s.kind == Kind::Standard), "undo restores the whole group at once");
    }

    #[test]
    fn zoom_window_needs_a_real_drag_and_is_order_independent() {
        let (xw, yw) = (10.0, 1000.0);
        assert_eq!(zoom_box([2.0, 100.0], [4.0, 600.0], xw, yw), Some(([2.0, 100.0], [4.0, 600.0])));
        assert_eq!(zoom_box([4.0, 600.0], [2.0, 100.0], xw, yw), Some(([2.0, 100.0], [4.0, 600.0])), "dragging up-left works too");
        assert_eq!(zoom_box([2.0, 100.0], [2.05, 600.0], xw, yw), None, "a click or a tiny jitter does not zoom");
        assert_eq!(zoom_box([2.0, 100.0], [4.0, 105.0], xw, yw), None);
    }

    #[test]
    fn boundary_lines_are_only_grabbed_when_the_pointer_is_on_them() {
        // 100 px per minute: 8 px = 0.08 min
        assert_eq!(nearest_boundary(4.95, 5.0, 5.4, 100.0, 8.0), Some(Boundary::Start));
        assert_eq!(nearest_boundary(5.37, 5.0, 5.4, 100.0, 8.0), Some(Boundary::End));
        assert_eq!(nearest_boundary(5.2, 5.0, 5.4, 100.0, 8.0), None, "in the middle of the peak a drag draws a zoom window");
        assert_eq!(nearest_boundary(5.2, 5.0, 5.4, 1000.0, 8.0), None);
        assert_eq!(nearest_boundary(5.004, 5.0, 5.4, 1000.0, 8.0), Some(Boundary::Start));
    }

    #[test]
    fn wheel_scaling_keeps_the_pivot_in_place() {
        assert_eq!(scale_range(0.0, 10.0, 5.0, 0.5), (2.5, 7.5));
        assert_eq!(scale_range(0.0, 10.0, 0.0, 0.5), (0.0, 5.0), "pivot at the edge: that edge stays");
        assert_eq!(scale_range(0.0, 10.0, 5.0, 2.0), (-5.0, 15.0), "factor > 1 zooms out");
        let (lo, hi) = scale_range(100.0, 200.0, 120.0, 0.25);
        assert!((120.0 - lo) / (hi - lo) - 0.2 < 1e-12, "relative pivot position is unchanged");
    }

    #[test]
    fn cursor_following_is_limited_to_the_data_area() {
        let b = PlotBounds::from_min_max([2.0, 0.0], [8.0, 100.0]);
        assert!(in_view(&PlotPoint::new(5.0, 50.0), &b));
        assert!(in_view(&PlotPoint::new(2.0, 0.0), &b), "the border counts as inside");
        assert!(!in_view(&PlotPoint::new(1.9, 50.0), &b), "over the y axis labels");
        assert!(!in_view(&PlotPoint::new(5.0, -3.0), &b), "below the x axis");
        assert!(!in_view(&PlotPoint::new(8.5, 50.0), &b));
    }

    #[test]
    fn only_standards_are_listed_in_the_setup_matrix() {
        let mut a = app();
        for i in 0..3 { add(&mut a, i, 0.0, 1.0); }
        let listed = |a: &App| (0..a.samples.len()).filter(|&s| a.samples[s].kind == Kind::Standard).count();
        assert_eq!(listed(&a), 0);
        a.apply_click(1, false, false);
        a.set_type(1, Kind::Standard);
        assert_eq!(listed(&a), 1);
    }
}
