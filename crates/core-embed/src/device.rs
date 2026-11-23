//! Where embedding models run: the `embedding-device` setting, and the device a model ended
//! up on.

use std::fmt;
use std::io;

use crate::model_error;

/// Where the operator wants embedding models to run (`embedding-device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevicePreference {
    /// Metal on macOS, CUDA in a build with the `cuda` feature, otherwise the CPU. A GPU that
    /// does not start falls back to the CPU, and the reason is kept.
    #[default]
    Auto,
    Cpu,
    /// CUDA or a load error. For benchmarks, where a silent CPU fallback would mislead.
    Cuda {
        device_id: i32,
    },
    /// Metal or a load error.
    Metal,
}

impl DevicePreference {
    /// Parses `auto`, `cpu`, `metal`, `cuda` or `cuda:N`; an empty value means `auto`.
    pub fn parse(value: &str) -> io::Result<Self> {
        match value.trim() {
            "" | "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "metal" => Ok(Self::Metal),
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
                            "invalid embedding device '{other}' (use auto, cpu, metal, cuda or cuda:N)"
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
    Metal,
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu => f.write_str("cpu"),
            Self::Cuda { device_id } => write!(f, "cuda:{device_id}"),
            Self::Metal => f.write_str("metal"),
        }
    }
}

/// Opens the device `preference` asks for. With `Auto` it also returns why a GPU this build
/// supports could not be used.
pub(crate) fn open_device(
    preference: DevicePreference,
) -> io::Result<(candle_core::Device, Device, Option<String>)> {
    match preference {
        DevicePreference::Cpu => Ok((candle_core::Device::Cpu, Device::Cpu, None)),
        DevicePreference::Cuda { device_id } => {
            let device = new_cuda(device_id).map_err(|e| {
                model_error(format!(
                    "embedding-device requires CUDA, but it could not start: {e}"
                ))
            })?;
            Ok((device, Device::Cuda { device_id }, None))
        }
        DevicePreference::Metal => {
            let device = candle_core::Device::new_metal(0).map_err(|e| {
                model_error(format!(
                    "embedding-device requires Metal, but it could not start: {e}"
                ))
            })?;
            Ok((device, Device::Metal, None))
        }
        DevicePreference::Auto => {
            let mut failures = Vec::new();
            if candle_core::utils::metal_is_available() {
                match candle_core::Device::new_metal(0) {
                    Ok(device) => return Ok((device, Device::Metal, None)),
                    Err(e) => failures.push(format!("Metal: {e}")),
                }
            }
            if candle_core::utils::cuda_is_available() {
                match new_cuda(0) {
                    Ok(device) => return Ok((device, Device::Cuda { device_id: 0 }, None)),
                    Err(e) => failures.push(format!("CUDA: {e}")),
                }
            }
            let fallback = (!failures.is_empty()).then(|| failures.join("; "));
            Ok((candle_core::Device::Cpu, Device::Cpu, fallback))
        }
    }
}

fn new_cuda(device_id: i32) -> candle_core::Result<candle_core::Device> {
    let ordinal = usize::try_from(device_id)
        .map_err(|_| candle_core::Error::Msg(format!("invalid CUDA device {device_id}")))?;
    candle_core::Device::new_cuda(ordinal)
}
