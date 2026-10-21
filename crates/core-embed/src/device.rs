//! Where embedding models run: the `embedding-device` setting, the device a model ended up on,
//! and NVIDIA driver detection.

use std::fmt;
use std::io;
use std::path::Path;

/// Where the operator wants embedding models to run (`embedding-device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevicePreference {
    /// CUDA when an NVIDIA driver is present and CUDA starts, otherwise the CPU.
    #[default]
    Auto,
    Cpu,
    /// CUDA or a load error. For benchmarks, where a silent CPU fallback would mislead.
    Cuda {
        device_id: i32,
    },
}

impl DevicePreference {
    /// Parses `auto`, `cpu`, `cuda` or `cuda:N`; an empty value means `auto`.
    pub fn parse(value: &str) -> io::Result<Self> {
        match value.trim() {
            "" | "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "cuda" => Ok(Self::Cuda { device_id: 0 }),
            other => other
                .strip_prefix("cuda:")
                .and_then(|id| id.parse::<i32>().ok())
                .filter(|id| *id >= 0)
                .map(|device_id| Self::Cuda { device_id })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "invalid embedding device '{other}' (use auto, cpu, cuda or cuda:N)"
                        ),
                    )
                }),
        }
    }
}

/// Where a loaded model actually runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Cuda { device_id: i32 },
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Device::Cpu => f.write_str("cpu"),
            Device::Cuda { device_id } => write!(f, "cuda:{device_id}"),
        }
    }
}

/// True when an NVIDIA kernel driver is loaded. Only then is CUDA worth trying, and only then
/// is the GPU build of ONNX Runtime worth downloading.
pub(crate) fn nvidia_driver_present() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && Path::new("/proc/driver/nvidia/version").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_preferences() {
        assert_eq!(DevicePreference::parse("").unwrap(), DevicePreference::Auto);
        assert_eq!(
            DevicePreference::parse(" auto ").unwrap(),
            DevicePreference::Auto
        );
        assert_eq!(
            DevicePreference::parse("cpu").unwrap(),
            DevicePreference::Cpu
        );
        assert_eq!(
            DevicePreference::parse("cuda").unwrap(),
            DevicePreference::Cuda { device_id: 0 }
        );
        assert_eq!(
            DevicePreference::parse("cuda:1").unwrap(),
            DevicePreference::Cuda { device_id: 1 }
        );
        for bad in ["gpu", "cuda:", "cuda:-1", "cuda:x"] {
            assert!(DevicePreference::parse(bad).is_err(), "{bad}");
        }
    }
}
