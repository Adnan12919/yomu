fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        println!("cargo:rerun-if-changed=resources/windows/yomu.rc");
        println!("cargo:rerun-if-changed=assets/yomu.ico");

        let include_dirs = [
            std::path::Path::new("."),
            std::path::Path::new("assets"),
            std::path::Path::new("resources/windows"),
        ];
        let _ = embed_resource::compile(
            "resources/windows/yomu.rc",
            embed_resource::ParamsIncludeDirs(include_dirs),
        );
    }
}
