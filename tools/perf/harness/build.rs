//! Bind the executable to the actual clean codec checkout and measurement files.
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "git identity discovery failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn rust_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

fn main() {
    let harness = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = harness.join("../../..").canonicalize().unwrap();
    let codec_paths = ["ruzstd/src", "ruzstd/Cargo.toml", "Cargo.toml"];
    let mut status_args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    status_args.extend(codec_paths);
    assert!(
        git(&root, &status_args).is_empty(),
        "dirty production codec cannot claim a commit identity"
    );
    let source = git(&root, &["rev-parse", "HEAD"]);
    println!("cargo:rustc-env=PERF_BUILD_SOURCE_SHA={source}");
    // Track both detached HEAD and a symbolic branch ref, including worktrees.
    for token in [
        "HEAD".to_owned(),
        git(&root, &["rev-parse", "--symbolic-full-name", "HEAD"]),
    ] {
        if !token.is_empty() {
            let path = git(&root, &["rev-parse", "--git-path", &token]);
            println!("cargo:rerun-if-changed={}", root.join(path).display());
        }
    }
    for path in codec_paths {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    let mut files = vec![harness.join("Cargo.toml"), harness.join("build.rs")];
    let mut sources = Vec::new();
    rust_sources(&harness.join("src"), &mut sources);
    sources.sort();
    files.extend(sources);
    let mut digest = Sha256::new();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        digest.update(
            path.strip_prefix(&harness)
                .unwrap()
                .to_str()
                .unwrap()
                .as_bytes(),
        );
        digest.update(b"\0");
        digest.update(std::fs::read(path).unwrap());
        digest.update(b"\0");
    }
    println!(
        "cargo:rustc-env=PERF_BUILD_HARNESS_SHA256={:x}",
        digest.finalize()
    );
    let lock = harness.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    println!(
        "cargo:rustc-env=PERF_BUILD_LOCK_SHA256={:x}",
        Sha256::digest(std::fs::read(lock).unwrap())
    );
}
