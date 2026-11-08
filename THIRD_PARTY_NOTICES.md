# Third-party software and credits

This project is MIT-licensed. Everything below is compatible with that license; the Rust dependencies are
permissive (MIT and/or Apache-2.0 and similar). Their license texts ship with their crates and must be kept
if you redistribute binaries.

## Used at run time as an external program (not part of this repository)
**ThermoRawFileParser** (CompOmics) – Apache-2.0 – https://github.com/CompOmics/ThermoRawFileParser
- Hulstaert N, Shofstahl J, Sachsenberg T, Walzer M, Barsnes H, Martens L, Perez-Riverol Y. "ThermoRawFileParser: Modular, Scalable, and Cross-Platform RAW File Conversion." J Proteome Res 2020;19(1):537–542. doi:10.1021/acs.jproteome.9b00328 (please cite when publishing results).
- It wraps Thermo Fisher Scientific's **RawFileReader** library (© Thermo Fisher Scientific, Inc.), which is under Thermo's own license (`THERMO_LICENSE` in the ThermoRawFileParser download). It is **not** distributed with this project; do not commit or redistribute the ThermoRawFileParser folder unless you comply with both licenses.

## Rust dependencies (compiled into the program)
| Crate | License | Purpose |
|---|---|---|
| eframe, egui (emilk/egui) | MIT OR Apache-2.0 | GUI framework |
| egui_plot | MIT OR Apache-2.0 | plots |
| rfd | MIT | native file dialogs |
| winresource (Windows build only) | MIT | embeds the .exe icon |
| transitive dependencies of the above | permissive (MIT, Apache-2.0, BSD, Zlib, ISC, Unicode, …) | see below |

The complete list of transitive licenses can be generated with
`cargo install cargo-about && cargo about init && cargo about generate about.hbs > licenses.html`
(or `cargo install cargo-license && cargo license`). Re-run this before every binary release and ship the output with the binary.

## Reference and assets
- **mzmine** (MIT) – https://github.com/mzmine/mzmine – reference for handling vendor raw files through an external converter. No mzmine source code is included.
- **App icon** (`img/lcms-quant-ico.ico`): generated with Gemini (Google). You are responsible for checking that Google's terms for generated content suit your use.
- **Development**: written with Claude Sonnet 5.5 (Medium), Anthropic.
