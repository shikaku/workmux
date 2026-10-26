//! Tag the version with the fork commit, so `workmux --version` tells this
//! build apart from an upstream release with the same package version.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

fn main() {
    let mut version = format!("{}+fork", env!("CARGO_PKG_VERSION"));
    if let Some(commit) = git(&["rev-parse", "--short=8", "HEAD"]) {
        version.push('.');
        version.push_str(&commit);
        let clean = Command::new("git")
            .args(["diff", "--quiet", "HEAD"])
            .status()
            .is_ok_and(|status| status.success());
        if !clean {
            version.push_str(".dirty");
        }

        // A commit moves HEAD or the branch ref; watch only paths that
        // exist, since a missing one makes cargo rerun this on every build.
        let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watched.push(branch);
        }
        for name in watched {
            if let Some(path) = git(&["rev-parse", "--git-path", &name])
                && std::path::Path::new(&path).exists()
            {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    // Edits to tracked sources change the dirty flag.
    for path in ["build.rs", "Cargo.toml", "Cargo.lock", "src"] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rustc-env=WORKMUX_VERSION={version}");
}
