//! Integration tests against the real Thermo .raw files in ./test-data.
//! They need ThermoRawFileParser (tools/ThermoRawFileParser*/ or $THERMO_RAW_FILE_PARSER) and, for the
//! framework-dependent build, the .NET runtime. If the data or the converter is missing the tests print
//! "SKIPPED" (run `cargo test -- --nocapture` to see it) and pass, so CI without the large files stays green.
use msquant::mzml;
use msquant::project::{ProjectFile, SampleRec};
use msquant::quant::{self, Analyte};
use msquant::rawconv;
use std::path::PathBuf;
use std::sync::OnceLock;

fn dir() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data") }

fn files(ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir()).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().map(|x| x.eq_ignore_ascii_case(ext)).unwrap_or(false)).collect()).unwrap_or_default();
    v.sort();
    v
}

struct Loaded { path: PathBuf, raw_sha: String, data: mzml::MzData }

/// Converts the first .raw file once (conversion can take a while) and shares it between tests.
fn first_run() -> Option<&'static Loaded> {
    static CELL: OnceLock<Option<Loaded>> = OnceLock::new();
    CELL.get_or_init(|| {
        let raw = match files("raw").into_iter().next() { Some(r) => r, None => { eprintln!("SKIPPED: no .raw file in {}", dir().display()); return None; } };
        let trfp = match rawconv::find_trfp("") { Some(t) => t, None => { eprintln!("SKIPPED: ThermoRawFileParser not found (tools/ThermoRawFileParser*/)"); return None; } };
        let raw_sha = rawconv::hash_file(&raw).expect("hash raw file");
        let conv = match rawconv::convert_in_memory(&trfp, &raw) {
            Ok(c) => c,
            Err(e) => { eprintln!("SKIPPED: conversion not possible here: {}", e); return None; }
        };
        let name = raw.file_name().unwrap().to_string_lossy().to_string();
        let data = mzml::parse_bytes(&name, &conv.mzml).unwrap_or_else(|e| panic!("{} could not be parsed: {}", name, e));
        Some(Loaded { path: raw, raw_sha, data })
    }).as_ref()
}

#[test]
fn raw_file_is_hashed_converted_in_memory_and_parsed() {
    let Some(l) = first_run() else { return };
    assert_eq!(l.raw_sha.len(), 64);
    assert_eq!(l.data.sha256.len(), 64);
    assert!(!l.data.spectra.is_empty() || !l.data.tic1.is_empty(), "no scans parsed from {}", l.path.display());
    assert!(l.data.n_total >= l.data.spectra.len() + l.data.tic1.len());
    for tr in [&l.data.tic1, &l.data.bpc1, &l.data.tic2, &l.data.bpc2] {
        assert!(tr.windows(2).all(|w| w[0][0] <= w[1][0]), "retention times must be ascending");
        assert!(tr.iter().all(|p| p[0].is_finite() && p[1].is_finite() && p[1] >= 0.0));
    }
    assert_eq!(l.data.tic1.len(), l.data.bpc1.len());
    assert_eq!(l.data.tic2.len(), l.data.spectra.len());
    for (t, b) in l.data.tic1.iter().zip(l.data.bpc1.iter()) { assert!(b[1] <= t[1] + 1e-6, "base peak cannot exceed the total ion current"); }
}

#[test]
fn nothing_is_written_next_to_the_raw_file() {
    let Some(l) = first_run() else { return };
    let parent = l.path.parent().unwrap();
    let stray = std::fs::read_dir(parent).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.extension().map(|x| x.eq_ignore_ascii_case("mzml")).unwrap_or(false)).count();
    assert_eq!(stray, 0, "in-memory conversion must not leave .mzML files behind");
    assert!(!PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("converted_mzml").exists());
}

#[test]
fn xic_extraction_and_integration_on_the_strongest_fragment() {
    let Some(l) = first_run() else { return };
    if l.data.spectra.is_empty() { eprintln!("SKIPPED: file has no MS2 scans"); return; }
    // data-driven target: most intense fragment of the most intense MS2 scan
    let best = l.data.spectra.iter().max_by(|a, b| a.inten.iter().sum::<f32>().partial_cmp(&b.inten.iter().sum::<f32>()).unwrap()).unwrap();
    let (mut mz, mut top) = (0.0, 0.0f32);
    for (m, i) in best.mz.iter().zip(best.inten.iter()) { if *i > top { top = *i; mz = *m; } }
    let x = quant::xic(&l.data.spectra, mz, mz * 10e-6);
    assert_eq!(x.len(), l.data.spectra.len());
    assert!(x.iter().any(|p| p[1] > 0.0), "the chosen fragment must appear in its own scan");
    let (t0, t1) = (x.first().unwrap()[0], x.last().unwrap()[0]);
    let total = quant::integrate(&x, t0, t1, false);
    assert!(total > 0.0);
    let (s, e) = quant::auto_peak(&x, None, 0.0).expect("a peak is found");
    assert!(s >= t0 && e <= t1 && s < e);
    let peak = quant::integrate(&x, s, e, false);
    assert!(peak > 0.0 && peak <= total * 1.000001, "peak area {} must not exceed the whole-run area {}", peak, total);
}

#[test]
fn a_run_aligns_to_itself_with_zero_shift() {
    let Some(l) = first_run() else { return };
    let tr = if l.data.tic1.len() >= 5 { &l.data.tic1 } else { &l.data.tic2 };
    assert!(quant::estimate_shift(tr, tr, 1.0).abs() < 0.011);
}

#[test]
fn two_real_runs_get_a_bounded_alignment_shift() {
    let raws = files("raw");
    if raws.len() < 2 { eprintln!("SKIPPED: needs two .raw files in test-data"); return; }
    let Some(trfp) = rawconv::find_trfp("") else { eprintln!("SKIPPED: ThermoRawFileParser not found"); return };
    let load = |p: &PathBuf| -> Option<mzml::MzData> {
        let c = rawconv::convert_in_memory(&trfp, p).ok()?;
        mzml::parse_bytes(&p.file_name()?.to_string_lossy(), &c.mzml).ok()
    };
    let (Some(a), Some(b)) = (load(&raws[0]), load(&raws[1])) else { eprintln!("SKIPPED: conversion failed"); return };
    let tr = |d: &mzml::MzData| if d.tic1.len() >= 5 { d.tic1.clone() } else { d.tic2.clone() };
    let s = quant::estimate_shift(&tr(&a), &tr(&b), 1.5);
    assert!(s.abs() <= 1.5 + 1e-9, "shift {} outside the search window", s);
}

#[test]
fn project_file_can_reference_the_real_files() {
    let raws = files("raw");
    if raws.is_empty() { eprintln!("SKIPPED: no .raw file in test-data"); return; }
    let pf = ProjectFile {
        app_version: "test".into(), user: "N/A".into(), params: vec![],
        analytes: vec![Analyte { name: "T".into(), rt: None, ions: vec![100.0], precursor: Some(250.5) }],
        samples: raws.iter().map(|p| SampleRec {
            path: p.to_string_lossy().to_string(), raw_sha256: rawconv::hash_file(p).unwrap(), kind: "Unknown".into(), conc: 0.0, rt_shift: 0.0, show: true, embedded: false, cells: vec![],
        }).collect(),
    };
    let back = ProjectFile::parse(&pf.to_text()).unwrap();
    assert_eq!(back, pf);
    assert!(back.samples.iter().all(|s| s.raw_sha256.len() == 64));
}

#[test]
fn optional_target_tables_in_test_data_parse() {
    let tables = files("csv");
    if tables.is_empty() { eprintln!("SKIPPED: no .csv target table in test-data"); return; }
    for t in tables {
        let text = std::fs::read_to_string(&t).unwrap();
        let a = quant::parse_table(&text).unwrap_or_else(|e| panic!("{}: {}", t.display(), e));
        assert!(a.iter().all(|x| !x.ions.is_empty() && !x.name.is_empty()));
    }
}

#[test]
fn optional_project_files_in_test_data_parse() {
    for p in files("quant") {
        let text = std::fs::read_to_string(&p).unwrap();
        ProjectFile::parse(&text).unwrap_or_else(|e| panic!("{}: {}", p.display(), e));
    }
}

#[test]
fn target_table_applied_to_the_real_run_never_panics() {
    let Some(l) = first_run() else { return };
    let tables = files("csv");
    let Some(t) = tables.first() else { eprintln!("SKIPPED: no .csv target table in test-data"); return };
    let targets = quant::parse_table(&std::fs::read_to_string(t).unwrap()).unwrap();
    for a in &targets {
        for &mz in &a.ions {
            let x = quant::xic(&l.data.spectra, mz, mz * 10e-6);
            if let Some((s, e)) = quant::auto_peak(&x, a.rt, 0.3) { assert!(quant::integrate(&x, s, e, true).is_finite()); }
        }
    }
}
