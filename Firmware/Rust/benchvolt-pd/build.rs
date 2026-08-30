fn main() {
    // The flash map depends on the boot architecture: the repo memory.x is
    // the v1 (stock bootloader) layout; the v2-boot feature swaps in the
    // 0x08005000 partition (which must end below the in-partition app
    // descriptor at 0x0801EFC0). The chosen map is emitted to OUT_DIR so
    // only one memory.x is ever on the search path.
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let memory_x = if std::env::var("CARGO_FEATURE_V2_BOOT").is_ok() {
        "MEMORY\n{\n  /* v2 boot chain: trampoline + cores below, descriptor above. */\n  FLASH : ORIGIN = 0x08005000, LENGTH = 0x19FC0\n\n  /* Boot core copies the 192-byte vector table here before jumping. */\n  RAM : ORIGIN = 0x200000C0, LENGTH = 16K - 0xC0\n}\n"
            .to_string()
    } else {
        std::fs::read_to_string(format!("{manifest}/memory.v1.x")).unwrap()
    };
    std::fs::write(format!("{out_dir}/memory.x"), memory_x).unwrap();
    println!("cargo:rustc-link-search={out_dir}");
    println!("cargo:rerun-if-changed=memory.v1.x");

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
