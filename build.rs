fn main() {
    println!("cargo:rerun-if-changed=assets/memtest.ico");
    println!("cargo:rerun-if-env-changed=WINDRES");
    #[cfg(all(windows, feature = "gui"))]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/memtest.ico")
            .set("OriginalFilename", "memtest.exe")
            .compile_for("memtest")
            .expect("compile the Windows icon resource");
    }
}
