fn main() {
    println!("cargo:rerun-if-changed=ui/win-keeper.ico");
    println!("cargo:rerun-if-changed=ui/tray-icon.png");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let cross_compiling_gnu =
            cfg!(unix) && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
        let target_arch =
            std::env::var("CARGO_CFG_TARGET_ARCH").expect("missing Windows target architecture");
        if cross_compiling_gnu && target_arch == "aarch64" {
            compile_arm64_resources();
        } else {
            let mut resources = winres::WindowsResource::new();
            if cross_compiling_gnu {
                let prefix = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
                    Ok("x86_64") => "x86_64-w64-mingw32",
                    Ok("x86") => "i686-w64-mingw32",
                    Ok(architecture) => {
                        panic!("unsupported Windows GNU architecture: {architecture}")
                    }
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
    }

    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");
}

fn compile_arm64_resources() {
    use std::{fs, path::PathBuf, process::Command};

    let output_directory =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("missing Cargo OUT_DIR"));
    let manifest_directory = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("missing Cargo manifest directory"),
    );
    let icon = manifest_directory.join("ui/win-keeper.ico");
    let version: Vec<u16> = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .chain(std::iter::repeat(0))
        .take(4)
        .collect();
    let resource_script = output_directory.join("win-keeper-arm64.rc");
    let resource_file = output_directory.join("win-keeper-arm64.res");
    let object_file = output_directory.join("win-keeper-arm64.obj");
    let script = format!(
        r#"1 ICON "{}"
1 VERSIONINFO
FILEVERSION {},{},{},{}
PRODUCTVERSION {},{},{},{}
FILEFLAGSMASK 0x3fL
FILEOS 0x40004L
FILETYPE 0x1L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "ProductName", "WinKeeper\0"
      VALUE "FileDescription", "WinKeeper Process Supervisor\0"
      VALUE "InternalName", "win-keeper\0"
      VALUE "OriginalFilename", "win-keeper.exe\0"
      VALUE "FileVersion", "{}\0"
      VALUE "ProductVersion", "{}\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon.display(),
        version[0],
        version[1],
        version[2],
        version[3],
        version[0],
        version[1],
        version[2],
        version[3],
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_VERSION"),
    );
    fs::write(&resource_script, script).expect("failed to write ARM64 resource script");
    let status = Command::new("llvm-rc")
        .current_dir(&output_directory)
        .arg("/no-preprocess")
        .arg("/FO")
        .arg(resource_file.file_name().unwrap())
        .arg(resource_script.file_name().unwrap())
        .status()
        .expect("failed to execute llvm-rc");
    assert!(
        status.success(),
        "llvm-rc failed to compile ARM64 resources"
    );
    let status = Command::new("llvm-cvtres")
        .current_dir(&output_directory)
        .arg("/MACHINE:ARM64")
        .arg(format!(
            "/OUT:{}",
            object_file.file_name().unwrap().to_string_lossy()
        ))
        .arg(resource_file.file_name().unwrap())
        .status()
        .expect("failed to execute llvm-cvtres");
    assert!(
        status.success(),
        "llvm-cvtres failed to convert ARM64 resources"
    );
    println!("cargo:rustc-link-arg={}", object_file.display());
}
