fn main() {
    println!(
        "cargo:rustc-link-search={}",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );
    println!("cargo:rerun-if-changed=memory.x");

    // Short git revision baked into version strings so a flashed image can be
    // matched to a commit. Dirty detection is best-effort: cargo reruns this
    // script on HEAD/index changes, not on every working-tree edit.
    let revision = git_revision().unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=BENCHVOLT_GIT_REV={revision}");
    println!("cargo:rerun-if-changed=../../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../../.git/index");
}

fn git_revision() -> Option<String> {
    let output = |args: &[&str]| {
        let result = std::process::Command::new("git").args(args).output().ok()?;
        result
            .status
            .success()
            .then(|| String::from_utf8_lossy(&result.stdout).trim().to_string())
    };
    let hash = output(&["rev-parse", "--short=8", "HEAD"])?;
    let dirty = match output(&["status", "--porcelain"]) {
        Some(status) if status.is_empty() => "",
        _ => "-dirty",
    };
    Some(format!("{hash}{dirty}"))
}
