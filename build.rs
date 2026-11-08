//! The commit a `lore` binary is built from, for `lore --version`:
//! `lore 0.4.1 (5fd9ae0)`, and `(5fd9ae0+)` when the tree had uncommitted
//! changes. Builds that cannot ask git (a source archive, no git on PATH) say
//! the version alone. A package that builds `lore` from a pinned git revision
//! of this crate gets that revision.
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    let dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    // Rebuilt when the sources or the checked-out commit change.
    for path in ["build.rs", "Cargo.toml", "src"] {
        println!("cargo:rerun-if-changed={path}");
    }
    let mut build = String::new();
    // A git dependency (cargo's `git/checkouts`): the depending package's pin
    // already names the commit, and its packaging checks `lore --version` word
    // for word (Zerus), so such a build says the version alone.
    let normalized = dir.to_string_lossy().replace('\\', "/");
    let dependency = normalized.contains("/git/checkouts/");
    // Only this crate's own repository: a crate vendored inside another one must
    // not report that one's commit.
    let toplevel = git(&dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
    let own = toplevel.as_ref().and_then(|top| std::fs::canonicalize(top).ok()) == std::fs::canonicalize(&dir).ok();
    if own && !dependency {
        if let Some(sha) = git(&dir, &["rev-parse", "--short=7", "HEAD"]) {
            let dirty = git(&dir, &["status", "--porcelain", "--", "src", "Cargo.toml", "Cargo.lock", "build.rs"]).is_some_and(|out| !out.is_empty());
            build = format!("{sha}{}", if dirty { "+" } else { "" });
        }
        for name in ["HEAD", "index"] {
            if let Some(path) = git(&dir, &["rev-parse", "--git-path", name]) {
                let path = if Path::new(&path).is_absolute() { PathBuf::from(path) } else { dir.join(path) };
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
        if let Some(head) = git(&dir, &["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&dir, &["rev-parse", "--git-path", &head]) {
                let path = if Path::new(&path).is_absolute() { PathBuf::from(path) } else { dir.join(path) };
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
    }
    println!("cargo:rustc-env=LORE_BUILD_COMMIT={build}");
    gpu_libraries();
}

/// The DirectML build must still start where DirectML.dll is missing: the DLL
/// is delay-loaded (`/DELAYLOAD`) and the binary loads its own pinned copy by
/// path before the first GPU session (embed::runtime); without it the build
/// stays on the CPU.
fn gpu_libraries() {
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if std::env::var_os("CARGO_FEATURE_DIRECTML").is_some() && windows_msvc {
        for kind in ["bins", "tests", "examples"] {
            println!("cargo:rustc-link-arg-{kind}=/DELAYLOAD:DirectML.dll");
            println!("cargo:rustc-link-arg-{kind}=delayimp.lib");
        }
    }
}
