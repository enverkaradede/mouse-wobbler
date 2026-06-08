fn main() {
    // Link ApplicationServices on macOS for AXIsProcessTrusted
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-lib=framework=ApplicationServices");

    tauri_build::build()
}
