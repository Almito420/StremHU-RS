//! Builds the C++ shim over libtorrent and puts its runtime next to the binary.
//!
//! The shim is a DLL, so MSVC resolves libtorrent, Boost, OpenSSL and iconv there
//! and this script only has to link one import library. The DLLs themselves are
//! copied beside the executable, because Windows resolves them from the exe's
//! directory at load time.

use std::path::{Path, PathBuf};

/// Where vcpkg put libtorrent. Overridable so the build is not tied to one machine.
fn vcpkg_root() -> PathBuf {
    std::env::var("VCPKG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("D:/vcpkg"))
}

fn main() {
    println!("cargo:rerun-if-changed=shim/shim.cpp");
    println!("cargo:rerun-if-changed=shim/CMakeLists.txt");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");

    let vcpkg = vcpkg_root();
    let toolchain = vcpkg.join("scripts/buildsystems/vcpkg.cmake");
    assert!(
        toolchain.exists(),
        "vcpkg toolchain not found at {}; set VCPKG_ROOT",
        toolchain.display()
    );

    let dst = cmake::Config::new("shim")
        .generator("Visual Studio 17 2022")
        .define("CMAKE_TOOLCHAIN_FILE", &toolchain)
        .define("VCPKG_TARGET_TRIPLET", "x64-windows")
        .profile("Release")
        .build();

    println!("cargo:rustc-link-search=native={}", dst.join("lib").display());
    println!("cargo:rustc-link-lib=dylib=stremhu_shim");

    // Everything the shim needs at load time, from both the install tree and vcpkg.
    let target_dir = target_dir();
    // The shim itself is fatal if it cannot be refreshed. A stale copy beside the
    // executable produces a binary that links against symbols the loaded DLL does not
    // export, and Windows reports that as STATUS_ENTRYPOINT_NOT_FOUND at startup with
    // no indication of the cause. Failing the build here is far cheaper than debugging
    // that, and the usual reason is simply a running instance holding the file open.
    copy_dlls(&dst.join("bin"), &target_dir, Failure::Fatal);
    // The vcpkg dependencies do not change when our code does.
    copy_dlls(
        &vcpkg.join("installed/x64-windows/bin"),
        &target_dir,
        Failure::Warn,
    );
}

enum Failure {
    Fatal,
    Warn,
}

/// `OUT_DIR` is `target/<profile>/build/<crate>-<hash>/out`, so the directory the
/// executable lands in is four levels up.
fn target_dir() -> PathBuf {
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    out.ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .expect("OUT_DIR is deeper than four levels")
}

fn copy_dlls(from: &Path, to: &Path, on_failure: Failure) {
    let Ok(entries) = std::fs::read_dir(from) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("dll") {
            continue;
        }
        let Some(name) = path.file_name() else { continue };
        let dest = to.join(name);

        if is_current(&path, &dest) {
            continue;
        }
        if let Err(e) = std::fs::copy(&path, &dest) {
            let message = format!(
                "could not copy {} to {}: {e}. A process holding the file open, such as a \
                 running instance, is the usual cause.",
                path.display(),
                dest.display()
            );
            match on_failure {
                Failure::Fatal => panic!("{message}"),
                Failure::Warn => println!("cargo:warning={message}"),
            }
        }
    }
}

/// Whether the destination is already this exact file.
///
/// Both size and modification time have to match. Size alone is not enough: two
/// builds of the same source differ in content far more often than in length, so a
/// size-only check silently keeps an old DLL beside a new executable.
fn is_current(src: &Path, dest: &Path) -> bool {
    let (Ok(src_meta), Ok(dst_meta)) = (src.metadata(), dest.metadata()) else {
        return false;
    };
    if src_meta.len() != dst_meta.len() {
        return false;
    }
    match (src_meta.modified(), dst_meta.modified()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
