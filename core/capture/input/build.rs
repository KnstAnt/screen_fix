#[path = "../../../deploy/windows/release_resource.rs"]
mod release_resource;

fn main() {
    println!("cargo:rerun-if-changed=screen_fix_input_helper.manifest");
    println!("cargo:rerun-if-changed=screen_fix_input_helper.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        release_resource::embed("screen_fix_input_helper.rc", "screen-fix-input-helper");
    }
}
