fn main() {
    println!("cargo:rerun-if-changed=screen_fix.manifest");
    println!("cargo:rerun-if-changed=screen_fix.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for("screen_fix.rc", &["screen-fix"], embed_resource::NONE)
            .manifest_required()
            .expect("Embed the screen_fix Windows DPI manifest");
    }
}
