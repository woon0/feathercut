fn main() {
    println!("cargo:rerun-if-changed=ui");
    slint_build::compile("ui/app.slint").expect("Failed to compile Slint UI definition");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
    let candidates = [
        std::path::Path::new("runtime/mpv/libmpv-2.dll"),
        std::path::Path::new("_archive/runtime/mpv/libmpv-2.dll"),
    ];
    let runtime = candidates.into_iter().find(|p| p.is_file());
    if let Some(runtime) = runtime {
        let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        let profile = out.ancestors().nth(3).unwrap();
        let destination = profile.join("libmpv-2.dll");
        let source = std::fs::metadata(runtime).unwrap();
        let unchanged = std::fs::metadata(&destination).is_ok_and(|existing| {
            existing.len() == source.len() && existing.modified().ok() == source.modified().ok()
        });
        if !unchanged {
            std::fs::copy(runtime, destination).expect("Copy playback runtime");
        }
    }
}

fn embed_windows_icon() {
    use std::{path::PathBuf, process::Command};
    println!("cargo:rerun-if-changed=assets/feather.ico");
    println!("cargo:rerun-if-env-changed=RC");
    let compiler = std::env::var_os("RC")
        .map(PathBuf::from)
        .or_else(|| {
            let sdk =
                PathBuf::from(std::env::var_os("ProgramFiles(x86)")?).join("Windows Kits/10/bin");
            let mut versions: Vec<_> = std::fs::read_dir(sdk)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect();
            versions.sort();
            let host = std::env::var("HOST").unwrap();
            let architecture = if host.starts_with("aarch64") {
                "arm64"
            } else if host.starts_with("x86_64") {
                "x64"
            } else {
                "x86"
            };
            versions
                .into_iter()
                .rev()
                .map(|path| path.join(architecture).join("rc.exe"))
                .find(|path| path.is_file())
        })
        .expect("Windows SDK resource compiler required; install the SDK or set RC to rc.exe");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let resource = out.join("feathercut.res");
    let script = out.join("feathercut.rc");
    let icon = std::env::current_dir().unwrap().join("assets/feather.ico");
    std::fs::write(
        &script,
        format!(
            "1 ICON \"{}\"\n",
            icon.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let status = Command::new(compiler)
        .args(["/nologo", "/fo"])
        .arg(&resource)
        .arg(&script)
        .status()
        .expect("Run Windows resource compiler");
    assert!(status.success(), "Compile Feathercut executable icon");
    println!("cargo:rustc-link-arg-bins={}", resource.display());
}
