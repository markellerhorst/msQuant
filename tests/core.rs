//! Unit/integration tests for the non-GUI core. Run: cargo test
use msquant::audit;
use msquant::fmt;
use msquant::history::History;
use msquant::icon;
use msquant::inflate;
use msquant::mzml;
use msquant::paths;
use msquant::png;
use msquant::project::{self, BlobSource, CellRec, ProjectFile, SampleRec};
use msquant::quant::{self, Analyte};
use msquant::sha256::{self, Sha256};
use msquant::svg;
use msquant::testkit;

// ---------- hashing ----------
#[test]
fn sha256_known_vectors() {
    assert_eq!(sha256::sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    assert_eq!(sha256::sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(sha256::sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
               "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
}

#[test]
fn sha256_streaming_matches_one_shot() {
    let mut h = Sha256::new();
    let chunk = vec![b'a'; 1000];
    for _ in 0..1000 { h.update(&chunk); }
    assert_eq!(h.finish_hex(), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut h = Sha256::new();
    for c in data.chunks(37) { h.update(c); }
    assert_eq!(h.finish_hex(), sha256::sha256_hex(&data));
}

// ---------- zlib ----------
#[test]
fn inflate_fixed_huffman_streams() {
    assert_eq!(inflate::zlib_decompress(&[0x78, 0x9c, 0x4b, 0x04, 0x00, 0x00, 0x62, 0x00, 0x62]).unwrap(), b"a");
    let hw = [0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01, 0x00, 0x1a, 0x0b, 0x04, 0x5d];
    assert_eq!(inflate::zlib_decompress(&hw).unwrap(), b"hello world");
}

#[test]
fn inflate_stored_blocks_including_multi_block() {
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    assert_eq!(inflate::zlib_decompress(&testkit::zlib_stored(&data)).unwrap(), data);
    assert!(inflate::zlib_decompress(&[0x78, 0x9c, 0xff]).is_err());
}

// ---------- scientific notation ----------
#[test]
fn scientific_notation_display() {
    assert_eq!(fmt::sci(100.0), "10²");
    assert_eq!(fmt::sci(1000.0), "10³");
    assert_eq!(fmt::sci(1.0), "10⁰");
    assert_eq!(fmt::sci(1234.5), "1.2345×10³");
    assert_eq!(fmt::sci(0.05), "5×10¯²");
    assert_eq!(fmt::sci(-250.0), "-2.5×10²");
    assert_eq!(fmt::sci(0.0), "0");
}

#[test]
fn scientific_notation_round_trips_through_the_parser() {
    for v in [100.0, 1000.0, 1234.5, 0.05, -250.0, 1e-7, 123456.0, 3.0] {
        let back = fmt::parse_sci(&fmt::sci(v)).unwrap_or_else(|| panic!("cannot parse {}", fmt::sci(v)));
        assert!((back - v).abs() <= v.abs() * 1e-5, "{} -> {} -> {}", v, fmt::sci(v), back);
    }
    assert_eq!(fmt::parse_sci("1.5e3"), Some(1500.0));
    assert_eq!(fmt::parse_sci("2,5"), Some(2.5));
    assert!((fmt::parse_sci("5x10^-2").unwrap() - 0.05).abs() < 1e-12);
    assert_eq!(fmt::parse_sci("garbage"), None);
}

#[test]
fn retention_times_use_two_decimals_and_mz_keeps_its_digits() {
    assert_eq!(fmt::rt2(7.2), "7.20");
    assert_eq!(fmt::rt2(0.0), "0.00");
    assert_eq!(fmt::rt2(12.3456), "12.35");
    assert_eq!(fmt::sci_prec(286.1438, 9), "2.861438×10²");
    assert_eq!(fmt::sci_prec(150.0, 9), "1.5×10²");
    assert_eq!(fmt::mz4(286.14384), "286.1438");
    assert_eq!(fmt::mz4(150.0), "150.0000");
    assert_eq!(fmt::mz4(1234.56789), "1234.5679", "plain rounding, never scientific notation");
    assert_eq!(fmt::sci3(1234567.0), "1.23×10⁶", "compact axis ticks");
    assert_eq!(fmt::pct0(40.0), "40%");
    assert_eq!(fmt::pct0(100.0), "100%");
}

// ---------- undo/redo ----------
#[test]
fn history_is_bounded_and_ordered() {
    let mut h: History<i32> = History::new(10);
    for i in 0..12 { h.record(i); }
    assert_eq!(h.lens(), (10, 0));
    assert_eq!(h.undo(100), Some(11));
    assert_eq!(h.undo(11), Some(10));
    assert_eq!(h.redo(10), Some(11));
    assert_eq!(h.redo(11), Some(100));
    assert_eq!(h.redo(100), None);
}

#[test]
fn history_new_action_clears_redo() {
    let mut h: History<i32> = History::new(10);
    h.record(1);
    h.record(2);
    assert_eq!(h.undo(3), Some(2));
    assert!(h.can_redo());
    h.record(4);
    assert!(!h.can_redo());
    assert_eq!(h.undo(5), Some(4));
    assert_eq!(h.undo(4), Some(1));
    assert_eq!(h.undo(1), None);
}

// ---------- target table ----------
#[test]
fn target_table_parsing() {
    let t = quant::parse_table("name,rt_min,ions\nMorphine,n/a,286.14;201.07;165.05\nCodeine,3.2,300.16 215.10\nFentanyl,5.8,337.23,188.13,105.07\n").unwrap();
    assert_eq!(t.len(), 3);
    assert_eq!(t[0].rt, None);
    assert_eq!(t[0].ions, vec![286.14, 201.07, 165.05]);
    assert_eq!(t[1].rt, Some(3.2));
    assert_eq!(t[1].ions.len(), 2);
    assert_eq!(t[2].ions.len(), 3);
    assert!(quant::parse_table("").is_err());
    assert!(quant::parse_table("name,rt,ions\nGood,1.0,100\nBad,x,100\n").is_err());
    assert_eq!(quant::parse_table("Drug\t2.5\t100.1\t200.2\n").unwrap()[0].ions, vec![100.1, 200.2]);
    let p = quant::parse_table("name,rt,ions\nMorphine,n/a,286.14;201.07;p:286.1438\n").unwrap();
    assert_eq!(p[0].precursor, Some(286.1438));
    assert_eq!(p[0].ions, vec![286.14, 201.07], "the precursor tag is not a product ion");
    assert_eq!(quant::parse_table("A,1.0,100\n").unwrap()[0].precursor, None);
}

// ---------- mzML ----------
#[test]
fn mzml_parsing_of_synthetic_run() {
    let d = mzml::parse_bytes("synthetic", &testkit::synthetic_mzml(0.0, 1.0)).unwrap();
    assert_eq!(d.spectra.len(), 1000, "MS2 scans");
    assert_eq!(d.tic1.len(), 1000, "MS1 points");
    assert_eq!(d.n_total, 2000);
    assert_eq!(d.sha256.len(), 64);
    assert_eq!(d.ms1.len(), 1000, "MS1 spectra are kept for MS1-based quantification");
    assert_eq!(d.ms1[3].mz, vec![500.0]);
    assert_eq!(d.spectra[0].precursor, Some(testkit::PRECURSOR), "selected ion m/z is preferred over the isolation window target (286)");
    assert!(d.ms1.iter().all(|s| s.precursor.is_none()), "MS1 scans have no precursor");
    assert_eq!((d.ms1[0].scan, d.spectra[0].scan, d.spectra[1].scan), (0, 1, 3), "scan numbers come from the spectrum id");
    assert!((d.spectra[10].rt - 0.105).abs() < 1e-9, "minute unit");
    assert!((d.tic1[10][0] - 0.1).abs() < 1e-9, "second unit converted to minutes");
    assert_eq!(d.spectra[0].mz, vec![150.0, 250.0, 400.0]);
    assert!(d.spectra.windows(2).all(|w| w[0].rt <= w[1].rt));
    let apex = d.bpc2.iter().cloned().fold([0.0, 0.0], |m, p| if p[1] > m[1] { p } else { m });
    assert!((apex[0] - 5.005).abs() < 0.011);
    assert!((apex[1] - 1000.0).abs() < 10.0);
    assert!(mzml::parse_bytes("empty", b"<mzML></mzML>").is_err());
}

// ---------- chromatography maths ----------
#[test]
fn xic_integration_matches_the_analytical_area() {
    let d = mzml::parse_bytes("s", &testkit::synthetic_mzml(0.0, 1.0)).unwrap();
    let x = quant::xic(&d.spectra, testkit::FRAG1, 0.01);
    assert_eq!(x.len(), d.spectra.len());
    let area = quant::integrate(&x, 4.5, 5.5, false);
    let expect = testkit::expected_area_seconds(1.0);
    assert!((area - expect).abs() / expect < 0.01, "area {} expected {}", area, expect);
    let x2 = quant::xic(&d.spectra, testkit::FRAG2, 0.01);
    let a2 = quant::integrate(&x2, 4.5, 5.5, false);
    assert!((a2 / area - 0.5).abs() < 0.005);
    assert!(quant::xic(&d.spectra, 999.0, 0.01).iter().all(|p| p[1] == 0.0));
}

#[test]
fn baseline_subtraction_removes_a_constant_offset() {
    let pts: Vec<[f64; 2]> = (0..1000).map(|i| { let t = 4.0 + i as f64 * 0.002; [t, 100.0 + 1000.0 * (-0.5f64 * ((t - 5.0) / 0.05).powi(2)).exp()] }).collect();
    let with = quant::integrate(&pts, 4.7, 5.3, true);
    let without = quant::integrate(&pts, 4.7, 5.3, false);
    let expect = 1000.0 * 0.05 * (2.0 * std::f64::consts::PI).sqrt() * 60.0;
    assert!((with - expect).abs() / expect < 0.01, "{} vs {}", with, expect);
    assert!(without > with * 1.2);
}

#[test]
fn auto_peak_brackets_the_apex_and_honours_the_rt_hint() {
    let g = |t: f64, c: f64, h: f64| h * (-0.5f64 * ((t - c) / 0.05).powi(2)).exp();
    let pts: Vec<[f64; 2]> = (0..1500).map(|i| { let t = i as f64 * 0.01; [t, g(t, 5.0, 1000.0) + g(t, 8.0, 400.0)] }).collect();
    let (s, e) = quant::auto_peak(&pts, None, 0.0).unwrap();
    assert!(s < 5.0 && e > 5.0 && s > 4.6 && e < 5.4, "{} {}", s, e);
    let (s2, e2) = quant::auto_peak(&pts, Some(8.0), 0.3).unwrap();
    assert!(s2 < 8.0 && e2 > 8.0 && s2 > 7.6 && e2 < 8.4, "{} {}", s2, e2);
    assert!(quant::auto_peak(&pts, Some(11.0), 0.3).is_none());
}

#[test]
fn calibration_fit_recovers_the_line_for_every_weighting() {
    let pts: Vec<(f64, f64)> = [1.0, 2.0, 5.0, 10.0, 50.0].iter().map(|&x| (x, 250.0 * x + 40.0)).collect();
    for w in 0..3u8 {
        let f = quant::fit(&pts, w).unwrap();
        assert!((f.m - 250.0).abs() < 1e-6 && (f.b - 40.0).abs() < 1e-5, "weighting {}", w);
        assert!(f.r2 > 0.999999);
        assert_eq!((f.lo, f.hi, f.n), (1.0, 50.0, 5));
    }
    assert!(quant::fit(&[(1.0, 2.0), (1.0, 3.0)], 0).is_none(), "one level is not a curve");
}

#[test]
fn rt_alignment_estimates_the_shift_with_the_right_sign() {
    let g = |t: f64, c: f64| (-0.5f64 * ((t - c) / 0.08).powi(2)).exp();
    let make = |off: f64| -> Vec<[f64; 2]> {
        (0..1500).map(|i| { let t = i as f64 * 0.01; [t, 100.0 + 5000.0 * g(t, 5.0 + off) + 3000.0 * g(t, 8.0 + off) + 2000.0 * g(t, 12.0 + off)] }).collect()
    };
    let r = make(0.0);
    assert!(quant::estimate_shift(&r, &r, 1.0).abs() < 0.011);
    let s = quant::estimate_shift(&r, &make(0.25), 1.0);
    assert!((s + 0.25).abs() < 0.02, "late run must be moved earlier, got {}", s);
    let s = quant::estimate_shift(&r, &make(-0.4), 1.0);
    assert!((s - 0.4).abs() < 0.02, "early run must be moved later, got {}", s);
}

// ---------- audit trail ----------
#[test]
fn audit_trail_detects_tampering_and_continues_across_sessions() {
    let p = std::env::temp_dir().join(format!("lcmsq_audit_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let path = p.to_string_lossy().to_string();
    let mut a = audit::Audit::new(&path);
    a.user = "tester".into();
    a.log("TEST1", "first");
    a.log("TEST2", "second");
    a.log("TEST3", "third | with pipe");
    assert_eq!(audit::verify(&path).unwrap(), 3);
    let mut b = audit::Audit::new(&path);
    b.log("TEST4", "after restart");
    assert_eq!(audit::verify(&path).unwrap(), 4);
    let txt = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, txt.replace("second", "SECOND")).unwrap();
    assert!(audit::verify(&path).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn default_user_is_na() {
    let p = std::env::temp_dir().join(format!("lcmsq_audit_user_{}.log", std::process::id()));
    assert_eq!(audit::Audit::new(&p.to_string_lossy()).user, "N/A");
}

// ---------- project file ----------
#[test]
fn project_file_round_trip() {
    let pf = ProjectFile {
        app_version: "0.3.0".into(), user: "eller".into(),
        params: vec![("tol".into(), "10".into()), ("mode".into(), "curve".into())],
        analytes: vec![Analyte { name: "A".into(), rt: None, ions: vec![100.5, 200.25], precursor: Some(286.1438) }, Analyte { name: "B".into(), rt: Some(3.25), ions: vec![300.0], precursor: None }],
        samples: vec![SampleRec {
            path: r"C:\data\a b.raw".into(), raw_sha256: "ab".repeat(32), kind: "Standard".into(), conc: 0.1, rt_shift: -0.0123, show: true, embedded: true,
            cells: vec![CellRec { analyte: 0, found: true, manual: true, start: 1.0 / 3.0, end: 2.5, include: false, conc: Some(2.5) },
                        CellRec { analyte: 1, found: false, manual: false, start: 0.0, end: 0.0, include: true, conc: None }],
        }],
    };
    assert_eq!(ProjectFile::parse(&pf.to_text()).unwrap(), pf);
    assert_eq!(pf.param("mode"), Some("curve"));
    assert!(ProjectFile::parse("garbage").is_err());
    assert!(ProjectFile::parse("LCMSQUANT-PROJECT\t1\nC\t0\ttrue\ttrue\t1\t2\ttrue\n").is_err(), "cell before sample");
}

// ---------- icon decoding ----------
fn ico(data: &[u8], w: u8, h: u8) -> Vec<u8> {
    let mut o = vec![0, 0, 1, 0, 1, 0, w, h, 0, 0, 1, 0, 32, 0];
    o.extend_from_slice(&(data.len() as u32).to_le_bytes());
    o.extend_from_slice(&22u32.to_le_bytes());
    o.extend_from_slice(data);
    o
}

#[test]
fn ico_with_bmp_entry_decodes() {
    let mut d = vec![];
    for v in [40u32, 2, 4] { d.extend_from_slice(&v.to_le_bytes()); }
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&32u16.to_le_bytes());
    d.extend_from_slice(&[0u8; 24]);
    d.extend_from_slice(&[255, 0, 0, 255, 255, 255, 255, 128]); // bottom row BGRA: blue, white(alpha 128)
    d.extend_from_slice(&[0, 0, 255, 255, 0, 255, 0, 255]);     // top row: red, green
    let (rgba, w, h) = icon::decode_ico(&ico(&d, 2, 2)).unwrap();
    assert_eq!((w, h), (2, 2));
    assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128]);
}

#[test]
fn ico_with_png_entry_decodes_including_sub_filter() {
    fn chunk(ty: &[u8], body: &[u8]) -> Vec<u8> {
        let mut c = (body.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(ty); c.extend_from_slice(body); c.extend_from_slice(&[0, 0, 0, 0]);
        c
    }
    let raw = [0u8, 1, 2, 3, 255, 4, 5, 6, 255, 1, 10, 20, 30, 255, 10, 10, 10, 0];
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = vec![]; ihdr.extend_from_slice(&2u32.to_be_bytes()); ihdr.extend_from_slice(&2u32.to_be_bytes()); ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend(chunk(b"IHDR", &ihdr));
    png.extend(chunk(b"IDAT", &testkit::zlib_stored(&raw)));
    png.extend(chunk(b"IEND", &[]));
    let (rgba, w, h) = icon::decode_ico(&ico(&png, 2, 2)).unwrap();
    assert_eq!((w, h), (2, 2));
    assert_eq!(rgba, vec![1, 2, 3, 255, 4, 5, 6, 255, 10, 20, 30, 255, 20, 30, 40, 255]);
    assert!(icon::decode_ico(b"not an icon at all, definitely not").is_none());
}

#[test]
fn the_real_app_icon_decodes_when_present() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("img").join("lcms-quant-ico.ico");
    if let Ok(bytes) = std::fs::read(&p) {
        match icon::decode_ico(&bytes) {
            Some((rgba, w, h)) => assert_eq!(rgba.len(), (w * h * 4) as usize),
            None => eprintln!("NOTE: {} uses an ICO sub-format the built-in decoder does not support (window icon is skipped, exe icon still embedded)", p.display()),
        }
    } else { eprintln!("SKIPPED: {} not found", p.display()); }
}

// ---------- project container with embedded raw files ----------

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lcmsq_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sample(path: &str) -> SampleRec {
    SampleRec { path: path.into(), raw_sha256: String::new(), kind: "Unknown".into(), conc: 0.0, rt_shift: 0.0, show: true, embedded: false, cells: vec![] }
}

#[test]
fn project_container_embeds_raw_files_and_extracts_them_bit_exact() {
    let dir = tmp_dir("container");
    let big: Vec<u8> = (0..2_500_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let small = b"tiny raw file".to_vec();
    let (a, b) = (dir.join("a.raw"), dir.join("b.raw"));
    std::fs::write(&a, &big).unwrap();
    std::fs::write(&b, &small).unwrap();
    let pf = ProjectFile { app_version: "t".into(), user: "u".into(), params: vec![], analytes: vec![],
                           samples: vec![sample(&a.to_string_lossy()), sample("C:\\gone\\missing.raw"), sample(&b.to_string_lossy())] };
    let out = dir.join("p.quant");
    let tmp = dir.join("tmp");
    let offs = project::write_container(&out, &pf, &[Some(BlobSource::File(a.clone())), None, Some(BlobSource::File(b.clone()))], &tmp).unwrap();
    assert!(offs[1].is_none());

    let (back, offs2) = project::read_container(&out).unwrap();
    assert_eq!(offs, offs2);
    assert_eq!(back.samples.iter().map(|s| s.embedded).collect::<Vec<_>>(), vec![true, false, true]);

    let dest = dir.join("x.raw");
    let (o, l) = offs2[0].unwrap();
    let h = project::extract_blob(&out, o, l, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), big);
    assert_eq!(h, sha256::sha256_hex(&big));
    let (o2, l2) = offs2[2].unwrap();
    project::extract_blob(&out, o2, l2, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), small);

    // the original files vanish; saving again over the SAME project must keep the data (read from the old copy, written to a temp file)
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    let blobs = vec![
        Some(BlobSource::Range { path: out.clone(), offset: o, len: l }), None,
        Some(BlobSource::Range { path: out.clone(), offset: o2, len: l2 }),
    ];
    let offs3 = project::write_container(&out, &back, &blobs, &tmp).unwrap();
    let (_, offs4) = project::read_container(&out).unwrap();
    assert_eq!(offs3, offs4);
    let (o, l) = offs4[0].unwrap();
    project::extract_blob(&out, o, l, &dest).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), big);
    assert_eq!(std::fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0), 0, "the temporary file is moved away, nothing is left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_text_only_projects_still_open() {
    let dir = tmp_dir("legacy");
    let pf = ProjectFile { samples: vec![sample("x.raw")], ..Default::default() };
    let old = pf.to_text().replace("\ttrue\tfalse\n", "\ttrue\n"); // strip the 'embedded' column an old version did not write
    let p = dir.join("old.quant");
    std::fs::write(&p, old).unwrap();
    let (back, offs) = project::read_container(&p).unwrap();
    assert_eq!(back.samples.len(), 1);
    assert_eq!(offs, vec![None]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn truncated_container_is_rejected() {
    let dir = tmp_dir("trunc");
    let raw = dir.join("a.raw");
    std::fs::write(&raw, vec![7u8; 100_000]).unwrap();
    let pf = ProjectFile { samples: vec![sample("a.raw")], ..Default::default() };
    let out = dir.join("p.quant");
    project::write_container(&out, &pf, &[Some(BlobSource::File(raw))], &dir.join("tmp")).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    std::fs::write(&out, &bytes[..bytes.len() - 5000]).unwrap();
    assert!(project::read_container(&out).is_err());
    std::fs::write(&out, b"LCMSQNT2\xff\xff\xff\xff\xff\xff\xff\x7f").unwrap();
    assert!(project::read_container(&out).is_err(), "absurd text length");
    assert!(project::write_container(&dir.join("q.quant"), &pf, &[], &dir.join("tmp")).is_err(), "blob list must match the samples");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- PNG / SVG figure export ----------
#[test]
fn crc32_matches_the_standard_check_values() {
    assert_eq!(png::crc32(b"123456789"), 0xCBF43926);
    assert_eq!(png::crc32(b"IEND"), 0xAE426082, "CRC of the PNG IEND chunk");
}

#[test]
fn png_encoder_round_trips_through_the_decoder() {
    let (w, h) = (300u32, 200u32); // > 65535 bytes of pixel data: several stored blocks
    let rgb: Vec<u8> = (0..(w * h * 3)).map(|i| (i % 251) as u8).collect();
    let bytes = png::encode_rgb(w, h, &rgb).unwrap();
    assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    assert_eq!(&bytes[bytes.len() - 8..bytes.len() - 4], b"IEND");
    let (rgba, dw, dh) = icon::decode_png(&bytes).unwrap();
    assert_eq!((dw, dh), (w, h));
    for (px, orig) in rgba.chunks(4).zip(rgb.chunks(3)) { assert_eq!(&px[..3], orig); assert_eq!(px[3], 255); }
    assert!(png::encode_rgb(2, 2, &[0; 5]).is_err());
}

#[test]
fn svg_ticks_are_nice_numbers() {
    assert_eq!(svg::nice_ticks(0.0, 100.0, 5), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
    let t = svg::nice_ticks(4.93, 5.07, 5);
    assert!(t.iter().any(|v| (v - 5.0).abs() < 1e-9));
    assert!(t.iter().all(|v| *v >= 4.93 - 1e-9 && *v <= 5.07 + 1e-9));
    assert_eq!(svg::nice_ticks(1.0, 1.0, 5), vec![1.0]);
}

#[test]
fn svg_export_contains_axes_series_and_escapes_text() {
    let plot = svg::SvgPlot {
        title: "run <1> & \"x\"".into(), x_label: "Retention time (min)".into(), y_label: "Intensity (% of maximum)".into(),
        series: vec![
            svg::SvgSeries { name: "m/z 150.0000".into(), rgb: (31, 119, 180), width: 2.0, points: vec![[7.0, 0.0], [7.2, 100.0], [7.4, 0.0]], marker: false },
            svg::SvgSeries { name: "empty".into(), rgb: (0, 0, 0), width: 1.0, points: vec![], marker: false },
        ],
        vlines: vec![svg::SvgVLine { x: 7.2, rgb: (0, 100, 0), label: "start".into() }],
        labels: vec![svg::SvgLabel { x: 7.2, y: 100.0, text: "286.1438".into(), rgb: (0, 0, 0) }],
        notes: vec![], bounds: [7.0, 7.4, 0.0, 100.0], x_fmt: fmt::rt2, y_fmt: fmt::pct0,
    };
    let s = svg::render(&plot);
    assert!(s.starts_with("<svg ") && s.trim_end().ends_with("</svg>"));
    assert_eq!(s.matches("<path ").count(), 1, "empty series are skipped");
    assert!(s.contains("run &lt;1&gt; &amp; &quot;x&quot;"));
    assert!(s.contains(">7.20<"), "retention-time tick with two decimals");
    assert!(s.contains(">100%<") && s.contains(">0%<"), "percent axis from 0 to 100");
    assert!(s.contains("m/z 150.0000") && s.contains("clip-path"));
    assert!(s.contains(">286.1438<"), "peak labels (m/z of the top peaks) are part of the figure");
    assert!(!s.contains("NaN"));
}

#[test]
fn project_blob_reader_returns_the_embedded_bytes() {
    let dir = tmp_dir("blob");
    let raw = dir.join("a.mzML");
    std::fs::write(&raw, b"<mzML>demo</mzML>").unwrap();
    let pf = ProjectFile { samples: vec![sample("a.mzML")], ..Default::default() };
    let out = dir.join("p.quant");
    let offs = project::write_container(&out, &pf, &[Some(BlobSource::File(raw))], &dir.join("tmp")).unwrap();
    let (o, l) = offs[0].unwrap();
    assert_eq!(project::read_blob(&out, o, l).unwrap(), b"<mzML>demo</mzML>");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn svg_calibration_plot_has_markers_line_and_formula_box() {
    let plot = svg::SvgPlot {
        title: "Calibration curve · Morphine".into(), x_label: "Concentration (ng/mL)".into(), y_label: "Area (intensity·s)".into(),
        series: vec![
            svg::SvgSeries { name: "used".into(), rgb: (31, 119, 180), width: 1.0, points: vec![[1.0, 250.0], [2.0, 510.0], [4.0, 990.0]], marker: true },
            svg::SvgSeries { name: "excluded".into(), rgb: (150, 150, 150), width: 1.0, points: vec![[2.0, 700.0]], marker: true },
            svg::SvgSeries { name: "fit".into(), rgb: (214, 39, 40), width: 2.0, points: vec![[0.0, 10.0], [4.2, 1030.0]], marker: false },
        ],
        vlines: vec![], labels: vec![],
        notes: vec!["area = 2.45×10²·conc + 1×10¹".into(), "R² = 9.9987×10⁻¹   n = 4".into(), "weighting 1/x²".into()],
        bounds: [0.0, 5.0, 0.0, 1200.0], x_fmt: fmt::sci3, y_fmt: fmt::sci3,
    };
    let s = svg::render(&plot);
    assert_eq!(s.matches("<path ").count(), 1, "only the fit is a line");
    assert!(s.matches("<circle").count() >= 4 + 2, "4 data points + legend markers");
    assert!(s.contains("area = 2.45×10²·conc + 1×10¹") && s.contains("R² = 9.9987×10⁻¹"), "regression formula and R² are inside the figure");
    assert!(s.contains("weighting 1/x²"));
}

// ---------- file locations ----------
#[test]
fn installation_root_is_found_from_the_executable_path() {
    use std::path::Path;
    assert_eq!(paths::root_from_exe(Path::new("/apps/msQuant/msQuant.exe")), Path::new("/apps/msQuant"));
    assert_eq!(paths::root_from_exe(Path::new("/work/msquant/target/debug/msQuant.exe")), Path::new("/work/msquant"), "cargo run uses the project root");
    assert_eq!(paths::root_from_exe(Path::new("/work/msquant/target/release/msQuant")), Path::new("/work/msquant"));
    assert_eq!(paths::root_from_exe(Path::new("/x/debug/msQuant")), Path::new("/x/debug"), "a folder called debug outside of target is a normal install folder");
}
