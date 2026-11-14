use std::{env, fs, path::Path};

fn main() {
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/snapit.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let Ok(out_dir) = env::var("OUT_DIR") else {
        return;
    };
    // rc.exe resolves `ICON` paths against neither the .rc nor the crate root
    // reliably, so use an absolute path (forward slashes avoid RC escapes).
    let icon = icon.display().to_string().replace('\\', "/");
    let rc = Path::new(&out_dir).join("snapit.rc");
    if fs::write(&rc, format!("1 ICON \"{icon}\"\n")).is_err() {
        return;
    }
    match embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        Ok(()) => {}
        Err(err) => println!("cargo:warning=failed to embed the Windows icon: {err}"),
    }
}
