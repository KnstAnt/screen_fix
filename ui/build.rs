#[path = "../deploy/windows/release_resource.rs"]
mod release_resource;

fn main() {
    println!("cargo:rerun-if-changed=screen_fix.manifest");
    println!("cargo:rerun-if-changed=screen_fix.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        release_resource::embed("screen_fix.rc", "screen-fix");
    }
}
