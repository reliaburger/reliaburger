/// Build script for Reliaburger.
///
/// When the `ebpf` feature is enabled on Linux, compiles the eBPF
/// C programs in `ebpf/` to `.bpf.o` object files using clang.
/// On other platforms or without the feature, this is a no-op.
use std::path::Path;
use std::process::Command;

fn main() {
    // Only compile eBPF programs on Linux with the ebpf feature
    if cfg!(target_os = "linux") && cfg!(feature = "ebpf") {
        compile_ebpf();
    }
}

fn compile_ebpf() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let ebpf_dir = Path::new("ebpf");

    let programs = ["onion_connect.bpf.c", "onion_dns.bpf.c"];

    for program in &programs {
        let src = ebpf_dir.join(program);
        let obj = Path::new(&out_dir).join(program.replace(".c", ".o"));

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

        let status = Command::new("clang")
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

    // Also watch the shared header
    println!("cargo:rerun-if-changed=ebpf/onion_common.h");
}
