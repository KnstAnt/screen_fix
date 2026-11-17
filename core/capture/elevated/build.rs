#[path = "../../../deploy/windows/release_resource.rs"]
mod release_resource;

fn main() {
    println!("cargo:rerun-if-changed=screen_fix_elevated.manifest");
    println!("cargo:rerun-if-changed=screen_fix_elevated.rc");
    if std::env::var_os("CARGO_FEATURE_ELEVATED_RELEASE").is_some() {
        assert_eq!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("windows"));
        release_resource::embed("screen_fix_elevated.rc", "screen-fix-elevated-capture");
    }
}
