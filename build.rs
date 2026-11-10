// Embeds the icon into the Windows .exe (Explorer / taskbar). Failure is non-fatal.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("img/msQuant-ico.ico");
        if let Err(e) = res.compile() { println!("cargo:warning=could not embed icon: {}", e); }
    }
}
