fn main() {
    println!("cargo:rerun-if-changed=screen_fix_input_helper.manifest");
    println!("cargo:rerun-if-changed=screen_fix_input_helper.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for(
            "screen_fix_input_helper.rc",
            &["screen-fix-input-helper"],
            embed_resource::NONE,
        )
        .manifest_required()
        .expect("Embed the input helper Windows manifest");
    }
}
