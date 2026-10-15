//! Compiles `smoke.c` with `cc` against the built cdylib and runs it.
use std::path::PathBuf;
use std::process::Command;

#[test]
fn c_smoke() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.join("../../target");
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let libdir = target.join(profile);
    assert!(
        libdir.join("libpgarbor_ffi.so").exists() || libdir.join("libpgarbor_ffi.dylib").exists(),
        "cdylib not built in {libdir:?}; run `cargo build -p pgarbor-ffi` first"
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("smoke");
    let st = Command::new("cc")
        .arg(manifest.join("tests/smoke.c"))
        .arg("-I")
        .arg(manifest.join("../../include"))
        .arg("-L")
        .arg(&libdir)
        .arg("-lpgarbor_ffi")
        .arg(format!("-Wl,-rpath,{}", libdir.display()))
        .arg("-o")
        .arg(&exe)
        .status()
        .expect("cc");
    assert!(st.success(), "cc failed");
    let out = Command::new(&exe).arg(dir.path().join("c.pga")).output().expect("run");
    println!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success());
}
