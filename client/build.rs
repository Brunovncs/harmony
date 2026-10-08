// The icon is resource 1, which GPUI uses for the window and Windows for the file. No manifest
// here: GPUI embeds its own.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/harmony.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        embed_resource::compile("assets/harmony.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
