//! Embeds the app icon in the Windows `clipscribe.exe`. Nothing happens for other targets or
//! without the `cli` feature (a library build, as frename's, stays as it was).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=docs/icon/clipscribe.ico");
    #[cfg(windows)]
    embed_icon();
}

#[cfg(windows)]
fn embed_icon() {
    let windows_target = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let cli = std::env::var_os("CARGO_FEATURE_CLI").is_some();
    if !windows_target || !cli {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("docs/icon/clipscribe.ico");
    if let Err(e) = resource.compile() {
        // A missing resource compiler must not break the build; the exe just has no icon.
        println!("cargo:warning=app icon not embedded: {e}");
    }
}
