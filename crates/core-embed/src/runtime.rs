//! The ONNX Runtime shared library: where it lives, downloading it on first use (the GPU build
//! when an NVIDIA driver is present), and loading it once per process.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::device::nvidia_driver_present;

/// Provider libraries the GPU build of ONNX Runtime loads from its own folder.
const GPU_PROVIDER_LIBRARIES: [&str; 2] = [
    "libonnxruntime_providers_shared.so",
    "libonnxruntime_providers_cuda.so",
];
/// Runtime folders ending in `-cuda<major>` hold the GPU build for that CUDA major version.
const CUDA_FOLDER_MARKER: &str = "-cuda";
/// CUDA major versions Microsoft publishes GPU builds for, newest first.
const SUPPORTED_CUDA_MAJORS: [u32; 2] = [13, 12];
/// Used when the NVIDIA driver is present but no CUDA runtime library was found yet.
const DEFAULT_CUDA_MAJOR: u32 = 12;

/// The ONNX Runtime release that is downloaded and expected.
pub const RUNTIME_RELEASE: &str = "1.28.0";

/// File name of the ONNX Runtime shared library on this platform.
#[cfg(target_os = "macos")]
pub const RUNTIME_LIBRARY_NAME: &str = "libonnxruntime.dylib";
#[cfg(target_os = "windows")]
pub const RUNTIME_LIBRARY_NAME: &str = "onnxruntime.dll";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const RUNTIME_LIBRARY_NAME: &str = "libonnxruntime.so";

/// Loads the ONNX Runtime library once per process, installing it first when allowed.
pub(crate) fn ensure_runtime(library: &Path, install: bool) -> io::Result<()> {
    static READY: Mutex<bool> = Mutex::new(false);
    let mut ready = READY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *ready {
        return Ok(());
    }
    if !library.is_file() {
        if !install {
            return Err(io::Error::other(format!(
                "ONNX Runtime {RUNTIME_RELEASE} library not found at {}",
                library.display()
            )));
        }
        install_runtime(library).map_err(|e| {
            io::Error::other(format!(
                "installing ONNX Runtime {RUNTIME_RELEASE} into {}: {e}",
                library.display()
            ))
        })?;
    }
    let builder = ort::init_from(library.to_string_lossy().to_string()).map_err(|e| {
        io::Error::other(format!(
            "loading ONNX Runtime from {}: {e}",
            library.display()
        ))
    })?;
    let _ = builder.commit();
    *ready = true;
    Ok(())
}

/// Where the automatically installed runtime lives under `runtime_root`.
///
/// Machines with an NVIDIA driver get the GPU build, in a folder of its own per CUDA major
/// version (`1.28.0-cuda12`): a CPU build installed earlier must not block it, and the CUDA 12
/// and 13 builds need different CUDA libraries. The GPU build also runs everything on the CPU
/// when CUDA cannot start, so picking it is never worse than the CPU build.
pub fn default_runtime_library(runtime_root: &Path) -> PathBuf {
    let folder = if nvidia_driver_present() {
        let major = installed_cuda_major().unwrap_or(DEFAULT_CUDA_MAJOR);
        format!("{RUNTIME_RELEASE}{CUDA_FOLDER_MARKER}{major}")
    } else {
        RUNTIME_RELEASE.to_string()
    };
    runtime_root.join(folder).join(RUNTIME_LIBRARY_NAME)
}

/// Newest supported CUDA major version whose runtime library (`libcudart.so.N`) this machine
/// can load, from the loader cache, `LD_LIBRARY_PATH` and the usual install folders.
fn installed_cuda_major() -> Option<u32> {
    let loader_cache = ["/sbin/ldconfig", "ldconfig"]
        .iter()
        .find_map(|tool| Command::new(tool).arg("-p").output().ok())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::var_os("LD_LIBRARY_PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default();
    dirs.extend(
        [
            "/usr/local/cuda/lib64",
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib64",
        ]
        .map(PathBuf::from),
    );
    SUPPORTED_CUDA_MAJORS.into_iter().find(|major| {
        let name = format!("libcudart.so.{major}");
        loader_cache.contains(&name) || dirs.iter().any(|dir| dir.join(&name).exists())
    })
}

/// The CUDA major version a runtime folder was installed for, or `None` for a CPU build.
fn runtime_cuda_major(library: &Path) -> Option<u32> {
    let folder = library.parent()?.file_name()?.to_str()?;
    let (_, major) = folder.rsplit_once(CUDA_FOLDER_MARKER)?;
    major
        .parse()
        .ok()
        .filter(|major| SUPPORTED_CUDA_MAJORS.contains(major))
}

fn install_runtime(library: &Path) -> io::Result<()> {
    let cuda_major = runtime_cuda_major(library);
    let package = runtime_package(cuda_major).ok_or_else(|| {
        io::Error::other(format!(
            "no {}ONNX Runtime download for {}-{}; install it and set embedding-runtime",
            if cuda_major.is_some() { "GPU " } else { "" },
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;
    let dir = library
        .parent()
        .ok_or_else(|| io::Error::other("runtime library path has no parent folder"))?;
    fs::create_dir_all(dir)?;
    let name = format!("{package}-{RUNTIME_RELEASE}");
    let url = format!(
        "https://github.com/microsoft/onnxruntime/releases/download/v{RUNTIME_RELEASE}/{name}.tgz"
    );
    let work = dir.join(format!(".download-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    let providers: &[&str] = if cuda_major.is_some() {
        &GPU_PROVIDER_LIBRARIES
    } else {
        &[]
    };
    let result = download_and_extract(&url, &name, &work, library, providers);
    let _ = fs::remove_dir_all(&work);
    result
}

fn download_and_extract(
    url: &str,
    name: &str,
    work: &Path,
    library: &Path,
    providers: &[&str],
) -> io::Result<()> {
    let archive = work.join("onnxruntime.tgz");
    run_tool(
        Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&archive)
            .arg(url),
        "curl",
    )?;
    run_tool(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(work),
        "tar",
    )?;
    let package_lib = work.join(name).join("lib");
    let target_dir = library
        .parent()
        .ok_or_else(|| io::Error::other("runtime library path has no parent folder"))?;
    // Providers first: the main library being present is what marks an install as complete.
    for provider in providers {
        let staged = target_dir.join(format!("{provider}.partial"));
        fs::copy(package_lib.join(provider), &staged)?;
        fs::rename(&staged, target_dir.join(provider))?;
    }
    // the package's libonnxruntime.{so,dylib} is a link; fs::copy follows it to the real file
    let source = package_lib.join(RUNTIME_LIBRARY_NAME);
    let staged = library.with_extension("partial");
    fs::copy(&source, &staged)?;
    fs::rename(&staged, library)
}

fn run_tool(command: &mut Command, tool: &str) -> io::Result<()> {
    let output = command
        .output()
        .map_err(|e| io::Error::other(format!("{tool} could not start: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{tool} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
/// Microsoft's release package for this machine; these builds pick CPU code at runtime.
/// The GPU builds add the CUDA provider and still run on the CPU when CUDA cannot start.
fn runtime_package(cuda_major: Option<u32>) -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH, cuda_major) {
        ("linux", "x86_64", Some(12)) => Some("onnxruntime-linux-x64-gpu_cuda12"),
        ("linux", "x86_64", Some(13)) => Some("onnxruntime-linux-x64-gpu_cuda13"), 
        ("macos", "aarch64", None) => Some("onnxruntime-osx-arm64"),
        ("macos", "x86_64", None) => Some("onnxruntime-osx-x86_64"),
        ("linux", "x86_64", None) => Some("onnxruntime-linux-x64"),
        ("linux", "aarch64", None) => Some("onnxruntime-linux-aarch64"),
        _ => None,
    }
}
