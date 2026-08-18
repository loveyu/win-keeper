fn main() {
    println!("cargo:rerun-if-changed=ui/win-keeper.ico");
    println!("cargo:rerun-if-changed=ui/tray-icon.png");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resources = winres::WindowsResource::new();
        let cross_compiling_gnu =
            cfg!(unix) && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
        if cross_compiling_gnu {
            let prefix = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
                Ok("x86_64") => "x86_64-w64-mingw32",
                Ok("x86") => "i686-w64-mingw32",
                Ok(architecture) => panic!("unsupported Windows GNU architecture: {architecture}"),
                Err(error) => panic!("missing Windows target architecture: {error}"),
            };
            resources
                .set_windres_path(&format!("{prefix}-windres"))
                .set_ar_path(&format!("{prefix}-ar"));
        }
        resources
            .set_icon("ui/win-keeper.ico")
            .set("ProductName", "WinKeeper")
            .set("FileDescription", "WinKeeper Process Supervisor")
            .set("InternalName", "win-keeper")
            .set("OriginalFilename", "win-keeper.exe")
            .compile()
            .expect("failed to embed Windows application resources");
        if cross_compiling_gnu {
            let resource = std::path::PathBuf::from(
                std::env::var_os("OUT_DIR").expect("missing Cargo OUT_DIR"),
            )
            .join("resource.o");
            println!("cargo:rustc-link-arg={}", resource.display());
        }
    }

    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");
}
