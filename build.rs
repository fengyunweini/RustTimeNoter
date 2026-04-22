fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // 嵌入清单：让 Windows 正确识别版本与 longPath
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:app.manifest");
    }
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
}
