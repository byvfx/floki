// Embeds the Windows icon. Linux and macOS builds skip this: the resource
// compiler only exists on a Windows host, and the egui window icon is compiled
// in separately via `include_bytes!` for every target.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/floki.ico");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let host = std::env::var("HOST").unwrap_or_default();
    if !host.contains("windows") {
        println!(
            "cargo:warning=skipping the Windows icon: host `{host}` is cross-compiling. \
             Build on Windows to embed assets/icons/floki.ico."
        );
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icons/floki.ico");
    if let Err(err) = res.compile() {
        panic!("failed to embed assets/icons/floki.ico into the Windows executable: {err}");
    }
}
