fn main() {
    // Windows exe icon, version info and manifest (DPI awareness, modern controls).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/app.manifest");
        println!("cargo:rerun-if-changed=assets/downloaderx.ico");
        embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
