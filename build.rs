// Optional CUDA kernel compilation.
//
// If a CUDA toolkit is found, cuda/qbtc_sha256_miner.cu is compiled and
// linked in, and GPU mining becomes available at runtime. If it is not found,
// or nvcc fails for any reason, a CPU stub providing the same symbols is
// compiled instead: the node still builds and runs, and the miner reports
// that no GPU is available and uses the CPU backend. GPU support is therefore
// never a build requirement.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    println!("cargo:rerun-if-changed=cuda/qbtc_sha256_miner.cu");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CUDA_PATH");

    match find_cuda_path() {
        Some(cuda_path) => {
            if let Err(e) = build_cuda_kernel(&out_dir, &cuda_path) {
                // cargo only parses `cargo:` directives from a build script's
                // stdout, and treats each line as its own directive -- so a
                // multi-line nvcc error has to be emitted one line at a time
                // or everything after the first line is silently dropped.
                println!("cargo:warning=CUDA build failed; falling back to the CPU stub.");
                for line in e.lines() {
                    println!("cargo:warning=  {}", line);
                }
                build_cpu_stub(&out_dir);
            }
        }
        None => {
            println!(
                "cargo:warning=No CUDA toolkit found (set CUDA_PATH to override). \
                 Building without GPU support."
            );
            build_cpu_stub(&out_dir);
        }
    }
}

fn is_windows() -> bool {
    env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
}

fn nvcc_name() -> &'static str {
    if is_windows() {
        "nvcc.exe"
    } else {
        "nvcc"
    }
}

// Newest toolkit first, so a machine with several installed side by side does
// not get pinned to the oldest one.
fn newest_matching_dir(parent: &str, prefix: &str, nvcc: &str) -> Option<String> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(parent)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(prefix))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .rev()
        .find(|p| p.join("bin").join(nvcc).exists())
        .map(|p| p.to_string_lossy().into_owned())
}

fn find_cuda_path() -> Option<String> {
    let nvcc = nvcc_name();

    if let Ok(p) = env::var("CUDA_PATH") {
        if Path::new(&p).join("bin").join(nvcc).exists() {
            return Some(p);
        }
    }

    if is_windows() {
        return newest_matching_dir(
            r"C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA",
            "v",
            nvcc,
        );
    }

    for base in ["/usr/local/cuda", "/opt/cuda", "/usr"] {
        if Path::new(base).join("bin").join(nvcc).exists() {
            return Some(base.to_string());
        }
    }
    // Versioned installs, e.g. /usr/local/cuda-12.4.
    newest_matching_dir("/usr/local", "cuda-", nvcc)
}

fn build_cuda_kernel(out_dir: &str, cuda_path: &str) -> Result<(), String> {
    let windows = is_windows();
    let nvcc = Path::new(cuda_path).join("bin").join(nvcc_name());
    let obj = PathBuf::from(out_dir).join(if windows { "qbtc_cuda.obj" } else { "qbtc_cuda.o" });
    let lib = PathBuf::from(out_dir).join(if windows {
        "qbtc_cuda_kernel.lib"
    } else {
        "libqbtc_cuda_kernel.a"
    });

    let target = env::var("TARGET").unwrap_or_default();
    let mut nvcc_cmd = Command::new(&nvcc);

    if windows {
        // nvcc shells out to cl.exe as its host compiler, and the archiving
        // step below needs lib.exe. Neither is on PATH in a plain cmd.exe or
        // PowerShell window, so borrow the environment the `cc` crate itself
        // would use instead of requiring a Developer Command Prompt.
        if let Some(t) = cc::windows_registry::find_tool(&target, "cl.exe") {
            for (k, v) in t.env() {
                nvcc_cmd.env(k, v);
            }
        }
    }

    nvcc_cmd.args([
        "cuda/qbtc_sha256_miner.cu",
        "-c",
        "-o",
        &obj.to_string_lossy(),
        // Pascal, Turing and Ampere as real SASS, plus PTX so newer
        // architectures still run via JIT.
        "--generate-code",
        "arch=compute_61,code=sm_61",
        "--generate-code",
        "arch=compute_75,code=sm_75",
        "--generate-code",
        "arch=compute_86,code=sm_86",
        "--generate-code",
        "arch=compute_86,code=compute_86",
        "-O3",
    ]);

    if windows {
        nvcc_cmd.args(["-Xcompiler", "/MD"]);
        // nvcc version-gates its host compiler (host_config.h, C1189) and
        // rejects MSVC toolsets newer than the one it shipped with, even for
        // code as plain as this. nvcc's own error names this flag as the
        // sanctioned override.
        nvcc_cmd.arg("-allow-unsupported-compiler");
    } else {
        nvcc_cmd.args(["-Xcompiler", "-fPIC"]);
    }

    // .output() rather than .status(): nvcc's diagnostics go to stderr, and
    // without them a failure here is undiagnosable from the build log.
    let output = nvcc_cmd
        .output()
        .map_err(|e| format!("could not run {}: {}", nvcc.display(), e))?;
    if !output.status.success() {
        return Err(format!(
            "nvcc failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    if windows {
        let mut lib_cmd = cc::windows_registry::find_tool(&target, "lib.exe")
            .map(|t| t.to_command())
            .unwrap_or_else(|| Command::new("lib.exe"));
        let status = lib_cmd
            .arg(format!("/OUT:{}", lib.display()))
            .arg(&obj)
            .status()
            .map_err(|e| {
                format!(
                    "could not run lib.exe: {} (no MSVC C++ build tools found, \
                     and lib.exe is not on PATH)",
                    e
                )
            })?;
        if !status.success() {
            return Err(format!("lib.exe failed: {}", status));
        }
        println!("cargo:rustc-link-search=native={}\\lib\\x64", cuda_path);
    } else {
        let ar = env::var("AR").unwrap_or_else(|_| "ar".to_string());
        let status = Command::new(&ar)
            .args(["crs"])
            .arg(&lib)
            .arg(&obj)
            .status()
            .map_err(|e| format!("could not run {}: {}", ar, e))?;
        if !status.success() {
            return Err(format!("{} failed: {}", ar, status));
        }
        // lib64 on most distributions, lib on a few.
        for sub in ["lib64", "lib"] {
            let dir = Path::new(cuda_path).join(sub);
            if dir.exists() {
                println!("cargo:rustc-link-search=native={}", dir.display());
            }
        }
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=qbtc_cuda_kernel");
    println!("cargo:rustc-link-lib=dylib=cudart");
    println!("cargo:warning=CUDA kernel compiled; GPU mining available.");
    Ok(())
}

// Provides every symbol src/gpu_miner.rs declares, so a build without CUDA
// links and runs normally. qbtc_cuda_device_count() reporting zero devices is
// what makes the miner select the CPU backend at startup.
fn build_cpu_stub(out_dir: &str) {
    const STUB: &str = r#"#include <stdint.h>

int qbtc_cuda_mine_on(int dev, const uint8_t* h120, uint64_t tu64,
                      uint64_t start, uint64_t bsz, uint64_t* out,
                      volatile int* sf) {
    (void)dev; (void)h120; (void)tu64; (void)start;
    (void)bsz; (void)out; (void)sf;
    return -1;
}

int qbtc_cuda_device_count(int* n) {
    if (n) *n = 0;
    return -1;
}

int qbtc_cuda_init_device(int dev) {
    (void)dev;
    return -1;
}

const char* qbtc_cuda_device_name(int dev) {
    (void)dev;
    return "CUDA_NOT_AVAILABLE";
}

void qbtc_cuda_shutdown(void) {}
"#;
    let stub = PathBuf::from(out_dir).join("qbtc_cuda_stub.c");
    fs::write(&stub, STUB).expect("could not write the CUDA stub");
    cc::Build::new().file(&stub).compile("qbtc_cuda_kernel");
}
