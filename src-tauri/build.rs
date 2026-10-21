fn main() {
    // tauri-build embeds the Common-Controls v6 manifest into the app exe only.
    // The unit-test exe links the same WebView2/dialog code and dies at load
    // with STATUS_ENTRYPOINT_NOT_FOUND without it. Embed that same manifest via
    // the linker for every target instead (it is the whole of tauri's default).
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if windows_msvc {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
        let windows = tauri_build::WindowsAttributes::new_without_app_manifest();
        tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
            .expect("tauri build");
    } else {
        tauri_build::build()
    }
}
