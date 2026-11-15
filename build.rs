// Embeds the icon, the application manifest (Windows 10/11, per-monitor DPI v2, no
// elevation) and version info into both EXEs.
fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/refract.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    if std::path::Path::new("assets/refract.ico").exists() {
        res.set_icon("assets/refract.ico");
    }
    res.set_manifest_file("assets/app.manifest");
    res.set("ProductName", "Refract-screener");
    res.set("FileDescription", "Refract-screener");
    res.set("CompanyName", "Refract");
    res.set("LegalCopyright", "Refract");
    if let Err(e) = res.compile() {
        println!("cargo:warning=resources not embedded: {e}");
    }
}
