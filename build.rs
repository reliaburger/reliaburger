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

        let status = Command::new("clang")
            .args([
                "-O2",
                "-target",
                "bpf",
                "-g",
                "-D__TARGET_ARCH_x86",
                "-I/usr/include",
                "-I/usr/include/bpf",
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
