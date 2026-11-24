// SPDX-License-Identifier: MIT
//! xnuhost's build script.
//!
//! - `cfg(xh_machine)`: the target can run the kernel (AArch64 Linux or Android): the vCPUs, the host-call thunks
//!   (arch.rs) and the task layer are built; elsewhere (the x86_64 PC's host tests) their stand-ins are.
//! - On such a target the task stub (runtime/xnuhost/stub, its own no_std crate for aarch64-unknown-none) is built by
//!   stub/build.sh into OUT_DIR/stub.bin, which arch.rs embeds (`STUB`).
use std::path::PathBuf;
use std::process::Command;

/// Host builds: src/gpu3d_amdgpu/layout.c against the headers the guest's Mesa is built with (Mesa cecc0e9's
/// include/drm-uapi, drm_hw.h and amdgpu_virtio_proto.h from external/mesa-droidvm, libdrm's amdgpu.h from
/// `pkg-config libdrm_amdgpu` or /usr/include/libdrm), for the test that checks gpu3d_amdgpu/uapi.rs against them
/// (cfg `amdgpu_layout`). Without either, the test says it is skipped.
fn amdgpu_layout(here: &std::path::Path, out: &std::path::Path) {
    let src = here.join("src/gpu3d_amdgpu/layout.c");
    println!("cargo::rerun-if-changed={}", src.display());
    println!("cargo::rerun-if-env-changed=MESA");
    let mesa = std::env::var("MESA").map(PathBuf::from).unwrap_or_else(|_| here.join("../../external/mesa-droidvm"));
    let proto = mesa.join("src/amd/common/virtio/amdgpu_virtio_proto.h");
    let libdrm = Command::new("pkg-config")
        .args(["--variable=includedir", "libdrm_amdgpu"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("libdrm"))
        .filter(|d| d.join("amdgpu.h").is_file())
        .unwrap_or_else(|| PathBuf::from("/usr/include/libdrm"));
    if !proto.is_file() || !libdrm.join("amdgpu.h").is_file() {
        return;
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".into());
    let obj = out.join("oneios_amdgpu_layout.o");
    let lib = out.join("liboneios_amdgpu_layout.a");
    let ok = Command::new(&cc)
        .args(["-std=gnu11", "-O0", "-fPIC", "-Wall", "-Werror", "-c"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .arg(format!("-I{}", mesa.join("include/drm-uapi").display()))
        .arg(format!("-I{}", libdrm.display()))
        .arg(format!("-I{}", mesa.join("src/virtio/virtio-gpu").display()))
        .arg(format!("-I{}", mesa.join("src/amd/common/virtio").display()))
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&lib);
    if !ok || !Command::new(&ar).arg("rcs").arg(&lib).arg(&obj).status().is_ok_and(|s| s.success()) {
        println!("cargo::warning=gpu3d_amdgpu: layout.c did not build: the layout test is skipped");
        return;
    }
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=oneios_amdgpu_layout");
    println!("cargo::rustc-cfg=amdgpu_layout");
}

fn main() {
    println!("cargo::rustc-check-cfg=cfg(xh_machine)");
    println!("cargo::rustc-check-cfg=cfg(amdgpu_layout)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "android" {
        let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
        amdgpu_layout(&here, &out);
    }
    if arch != "aarch64" || !(os == "linux" || os == "android") {
        return;
    }
    println!("cargo::rustc-cfg=xh_machine");
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let stub = here.join("stub");
    for watched in ["build.sh", "Cargo.toml", "Cargo.lock", "build.rs", "stub.ld", "src"] {
        println!("cargo::rerun-if-changed={}", stub.join(watched).display());
    }
    println!("cargo::rerun-if-changed=../abi/src");
    println!("cargo::rerun-if-env-changed=NDK");
    println!("cargo::rerun-if-env-changed=ANDROID_NDK_HOME");
    let status = Command::new("sh")
        .arg(stub.join("build.sh"))
        .arg(out.join("stub.bin"))
        .env("STUB_TARGET_DIR", out.join("stub-target"))
        .status()
        .expect("sh: stub/build.sh");
    assert!(status.success(), "stub/build.sh failed: the task stub is not built");
}
