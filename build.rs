use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    build_ver();
    platform_cfg();
}

fn build_ver() {
    // Cargo reruns build scripts when a tracked path is missing. Deliberately
    // never create this file: refresh the Git version on every build, even if
    // only the current commit changed and all source files stayed the same.
    let always_rebuild = PathBuf::from(env::var_os("OUT_DIR").unwrap())
        .join("neolink-version-always-rebuild");
    println!("cargo:rerun-if-changed={}", always_rebuild.display());

    let cargo_ver = env::var("CARGO_PKG_VERSION").unwrap();
    let commit = git_cmd_commit().unwrap_or("unknown commit".to_string());
    let version = git_ver().unwrap_or(cargo_ver);

    println!("cargo:rustc-env=NEOLINK_VERSION={} ({})", version, commit);
    println!(
        "cargo:rustc-env=NEOLINK_PROFILE={}",
        env::var("PROFILE").unwrap()
    );
}

fn git_ver() -> Option<String> {
    github_ver().or_else(git_cmd_ver)
}

fn git_cmd_ver() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

/// Attempts to retrieve the current Git commit hash (HEAD) of the repository.
///
/// # Returns
/// * `Some(String)` - The full commit hash prefixed with "commit: " (e.g., "commit: a1b2c3d...").
/// * `None` - If Git is not installed, the directory is not a Git repo, or an error occurs.
fn git_cmd_commit() -> Option<String> {
    let mut git_cmd = Command::new("git");
    git_cmd.args(["rev-parse", "HEAD"]);

    if let Ok(output) = git_cmd.output() {
        if output.status.success() {
            let commit_hash = String::from_utf8(output.stdout).ok()?;
            let format_commit_hash = format!("commit: {}", commit_hash.trim());
            return Some(format_commit_hash);
        }
    }

    None
}

fn github_ver() -> Option<String> {
    if let Ok(sha1) = env::var("GITHUB_SHA") {
        println!("cargo:rerun-if-env-changed=GITHUB_SHA");
        Some(sha1)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn platform_cfg() {
    let gstreamer_dir = env::var_os("GSTREAMER_1_0_ROOT_X86_64")
        .and_then(|x| x.into_string().ok())
        .unwrap_or_else(|| {
            env::var_os("GSTREAMER_1_0_ROOT_MSVC_X86_64")
                .and_then(|x| x.into_string().ok())
                .unwrap_or_else(|| r#"C:\gstreamer\1.0\x86_64\"#.to_string())
        });

    println!(r"cargo:rustc-link-search=native={}\lib", gstreamer_dir);
}

#[cfg(target_os = "macos")]
fn platform_cfg() {
    let gstreamer_dir = env::var_os("GSTREAMER_1_0_ROOT_MACOSX")
        .and_then(|x| x.into_string().ok())
        .unwrap_or_else(|| r"/Library/Frameworks/GStreamer.framework/Versions/1.0".to_string());
    let openssl_dir = env::var_os("OPENSSL_1_1_ROOT_MACOSX")
        .and_then(|x| x.into_string().ok())
        .unwrap_or_else(|| r"/usr/local/opt/openssl@1.1".to_string());

    println!(
        r"cargo:rustc-link-search=native={}/lib;{}/lib",
        gstreamer_dir, openssl_dir
    );
    println!(
        r"cargo:rustc-link-arg=-Wl,-rpath,{}/lib;{}/lib/",
        gstreamer_dir, openssl_dir
    );
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn platform_cfg() {}
