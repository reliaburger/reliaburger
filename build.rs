/// Build script for Reliaburger.
///
/// When the `ebpf` feature is enabled on Linux, compiles the eBPF
/// C programs in `ebpf/` to `.bpf.o` object files using clang.
/// On other platforms or without the feature, this is a no-op.
use std::path::Path;
use std::process::Command;

fn main() {
    // Cargo exposes TARGET only to build scripts. Re-export it so Phase 15
    // evidence can distinguish otherwise identical cross-compiled binaries.
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=RELIABURGER_TARGET={target}");
    }
    println!("cargo:rerun-if-env-changed=RELIABURGER_GIT_SHA");
    // Release builds name their commit in RELIABURGER_GIT_SHA, which rustc
    // sees directly. Anything else built from a checkout asks git, so two
    // local builds of the same version still say which code they hold.
    let release_commit = std::env::var("RELIABURGER_GIT_SHA").is_ok_and(|sha| !sha.is_empty());
    if !release_commit && let Some(commit) = checkout_commit() {
        println!("cargo:rustc-env=RELIABURGER_GIT_SHA={commit}");
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        compile_executor();
    }

    // Select the target OS: the build script itself runs on the host.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") && cfg!(feature = "ebpf") {
        compile_ebpf();
    }
}

/// The checkout's HEAD commit, or `None` when git can't say (no git, not a
/// checkout, or a repository git refuses to read).
///
/// Also tells Cargo to rerun this script when HEAD moves: when HEAD itself
/// changes (a checkout) and when the branch it names gets a new commit.
fn checkout_commit() -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let output = Command::new("git").args(args).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        Some(text.trim().to_string())
    };
    let commit = git(&["rev-parse", "HEAD"])?;
    // A worktree keeps its own HEAD but shares refs with the main checkout.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        rerun_if_exists(Path::new(&head));
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(reference) = git(&["rev-parse", "--git-path", &branch])
    {
        rerun_if_exists(Path::new(&reference));
    }
    if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"]) {
        rerun_if_exists(Path::new(&packed));
    }
    let is_hex = !commit.is_empty() && commit.chars().all(|c| c.is_ascii_hexdigit());
    is_hex.then_some(commit)
}

/// Watch a file only if it exists: Cargo reruns the script on every build
/// for a watched path that's missing.
fn rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn compile_ebpf() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let ebpf_dir = Path::new("ebpf");

    let programs = [
        ("onion_connect.bpf.c", "onion_connect.bpf.o", false),
        ("onion_dns.bpf.c", "onion_dns.bpf.o", false),
        ("onion_connect.bpf.c", "onion_connect_owned.bpf.o", true),
    ];

    for (program, object, persistent) in programs {
        let src = ebpf_dir.join(program);
        let obj = Path::new(&out_dir).join(object);

        println!("cargo:rerun-if-changed={}", src.display());

        // Detect the host architecture for kernel header include path
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "x86_64".to_string());
        let linux_arch = match arch.as_str() {
            "x86_64" => "x86",
            "aarch64" => "arm64",
            other => other,
        };
        let asm_include = format!(
            "/usr/include/{}-linux-gnu",
            match arch.as_str() {
                "x86_64" => "x86_64",
                "aarch64" => "aarch64",
                other => other,
            }
        );

        let mut compiler = Command::new("clang");
        if persistent {
            compiler.arg("-DRELIABURGER_PERSISTENT_MAPS=1");
        }
        let status = compiler
            .args([
                "-O2",
                "-target",
                "bpf",
                "-g",
                &format!("-D__TARGET_ARCH_{linux_arch}"),
                "-I/usr/include",
                "-I/usr/include/bpf",
                &format!("-I{asm_include}"),
                "-c",
            ])
            .arg(&src)
            .arg("-o")
            .arg(&obj)
            .status()
            .expect("failed to run clang — is clang installed?");

        if !status.success() {
            panic!("clang failed to compile {}", src.display());
        }
    }

    // Also watch shared headers
    println!("cargo:rerun-if-changed=ebpf/onion_common.h");
    println!("cargo:rerun-if-changed=ebpf/smoker_common.h");

    // Expose the build directory for development tooling. Runtime loading
    // uses embedded bytes unless the operator explicitly overrides it.
    println!("cargo:rustc-env=RELIABURGER_BPF_DIR={out_dir}");
}

/// Embed an image-independent static PID-1 helper; cross builds supply CC.
fn compile_executor() {
    println!("cargo:rerun-if-changed=src/bun/reusable_executor/helper.c");
    println!("cargo:rerun-if-env-changed=CC");
    let target = std::env::var("TARGET").expect("Cargo target");
    let target_key = format!("CC_{}", target.replace('-', "_"));
    println!("cargo:rerun-if-env-changed={target_key}");
    let compiler = std::env::var(&target_key)
        .or_else(|_| std::env::var("CC"))
        .unwrap_or_else(|_| "cc".into());
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"))
            .join("rb-executor-helper");
    let result = Command::new(compiler)
        .args([
            "-O2",
            "-static",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "src/bun/reusable_executor/helper.c",
            "-o",
        ])
        .arg(output)
        .status()
        .expect("failed to execute static Linux C compiler");
    assert!(
        result.success(),
        "static reusable executor helper compilation failed"
    );
}
