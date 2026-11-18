// Shared by the protected release executable build scripts.
use std::{env, fs, path::{Path, PathBuf}};

fn canonical<const N: usize>(name: &str) -> [u8; N] {
    let text = env::var(name).unwrap_or_else(|_| panic!("Missing required {name}"));
    assert!(text.len() == 2 * N, "{name} has an invalid length");
    let digit = |byte| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("{name} requires lowercase hexadecimal"),
    };
    let mut value = [0; N];
    for (byte, pair) in value.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *byte = digit(pair[0]) * 16 + digit(pair[1]);
    }
    assert!(value.iter().any(|&byte| byte != 0), "{name} must be nonzero");
    value
}

fn resource_path(path: &Path) -> String {
    let path = path.to_str().expect("UTF-8 Windows resource path");
    assert!(!path.contains(['"', '\r', '\n']), "Invalid Windows resource path");
    path.replace('\\', "/")
}

pub fn embed(manifest_rc: &str, binary: &str) {
    println!("cargo:rerun-if-env-changed=SCREEN_FIX_PUBLISHER_SPKI_SHA256");
    println!("cargo:rerun-if-env-changed=SCREEN_FIX_RELEASE_ID");
    if env::var_os("CARGO_FEATURE_ELEVATED_RELEASE").is_none() {
        embed_resource::compile_for(manifest_rc, &[binary], embed_resource::NONE)
            .manifest_required()
            .expect("Embed the Windows manifest");
        return;
    }
    assert_eq!(env::var("PROFILE").as_deref(), Ok("release"), "Elevation requires a release build");
    assert_eq!(
        env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("x86_64"),
        "Elevation requires x64 Windows"
    );
    assert_eq!(env::var("CARGO_CFG_TARGET_ENV").as_deref(), Ok("msvc"), "Elevation requires the MSVC Windows target");
    let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    assert!(features.split(',').any(|feature| feature == "crt-static"), "Elevation requires the static runtime");
    let publisher = canonical::<32>("SCREEN_FIX_PUBLISHER_SPKI_SHA256");
    let release = canonical::<16>("SCREEN_FIX_RELEASE_ID");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Build output directory"));
    let data = output.join("screen_fix_release.bin");
    let role = match binary {
        "screen-fix" => 1,
        "screen-fix-input-helper" => 2,
        "screen-fix-elevated-capture" => 3,
        _ => panic!("Unknown protected release image"),
    };
    let mut metadata = [0; 52];
    metadata[..4].copy_from_slice(&[1, role, 0, 0]);
    metadata[4..36].copy_from_slice(&publisher);
    metadata[36..].copy_from_slice(&release);
    fs::write(&data, metadata).expect("Write the release trust resource");
    let rc = output.join("screen_fix_release.rc");
    // Preserve the original resource inputs, including the executable's manifest.
    let include = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Package directory")).join(manifest_rc);
    fs::write(&rc, format!(
        "#include \"{}\"\n271 RCDATA \"{}\"\n",
        resource_path(&include),
        resource_path(&data),
    )).expect("Write release resource authoring");
    embed_resource::compile_for(&rc, &[binary], embed_resource::NONE)
        .manifest_required()
        .expect("Embed the manifest and release trust resource");
}
