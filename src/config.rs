// JSON configuration file loading and scenario config resolution.

use std::error::Error;
use std::path::Path;

use crate::cmd::scenario::{
    camera::CameraConfig, codec::CodecConfig, display::DisplayConfig, gpu::GpuConfig,
    npu::NpuConfig, pipeline::PipelineConfig,
};

/// Top-level JSON configuration containing optional per-scenario settings.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ScenarioConfigs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npu: Option<NpuConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<DisplayConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<CodecConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<PipelineConfig>,
}

/// Load scenario configurations from a JSON file.
pub fn load_config(path: &Path) -> Result<ScenarioConfigs, Box<dyn Error>> {
    let content = std::fs::read_to_string(path)?;
    let configs: ScenarioConfigs = serde_json::from_str(&content)?;
    Ok(configs)
}

/// Dump default configurations for all scenarios as pretty JSON.
#[must_use]
pub fn dump_default_config() -> String {
    let configs = ScenarioConfigs {
        npu: Some(NpuConfig::default()),
        camera: Some(CameraConfig::default()),
        display: Some(DisplayConfig::default()),
        codec: Some(CodecConfig::default()),
        gpu: Some(GpuConfig::default()),
        pipeline: Some(PipelineConfig::default()),
    };
    serde_json::to_string_pretty(&configs).expect("default config serialization should not fail")
}

/// Resolve NPU config: JSON base (if present) with CLI overrides.
#[must_use]
pub fn resolve_npu(
    json: Option<&NpuConfig>,
    iterations: Option<u32>,
    clients: Option<u32>,
) -> NpuConfig {
    let mut cfg = json.cloned().unwrap_or_default();
    if let Some(v) = iterations {
        cfg.iterations = v;
    }
    if let Some(v) = clients {
        cfg.clients = v;
    }
    cfg
}

/// Resolve Camera config: JSON base with CLI overrides.
#[must_use]
pub fn resolve_camera(
    json: Option<&CameraConfig>,
    width: Option<u32>,
    height: Option<u32>,
    frames: Option<u32>,
) -> CameraConfig {
    let mut cfg = json.cloned().unwrap_or_default();
    if let Some(v) = width {
        cfg.width = v;
    }
    if let Some(v) = height {
        cfg.height = v;
    }
    if let Some(v) = frames {
        cfg.frames = v;
    }
    cfg
}

/// Resolve Display config: JSON base with CLI overrides.
#[must_use]
pub fn resolve_display(
    json: Option<&DisplayConfig>,
    width: Option<u32>,
    height: Option<u32>,
    frames: Option<u32>,
) -> DisplayConfig {
    let mut cfg = json.cloned().unwrap_or_default();
    if let Some(v) = width {
        cfg.width = v;
    }
    if let Some(v) = height {
        cfg.height = v;
    }
    if let Some(v) = frames {
        cfg.frames = v;
    }
    cfg
}

/// Resolve Codec config: JSON base with CLI overrides.
#[must_use]
pub fn resolve_codec(
    json: Option<&CodecConfig>,
    width: Option<u32>,
    height: Option<u32>,
    frames: Option<u32>,
) -> CodecConfig {
    let mut cfg = json.cloned().unwrap_or_default();
    if let Some(v) = width {
        cfg.width = v;
    }
    if let Some(v) = height {
        cfg.height = v;
    }
    if let Some(v) = frames {
        cfg.frames = v;
    }
    cfg
}

/// Resolve GPU config: JSON base with CLI overrides.
#[must_use]
pub fn resolve_gpu(
    json: Option<&GpuConfig>,
    buffer_count: Option<usize>,
    texture_size: Option<u64>,
) -> GpuConfig {
    let mut cfg = json.cloned().unwrap_or_default();
    if let Some(v) = buffer_count {
        cfg.buffer_count = v;
    }
    if let Some(v) = texture_size {
        cfg.texture_size = v;
    }
    cfg
}

/// Resolve Pipeline config: JSON base with CLI overrides.
#[must_use]
pub fn resolve_pipeline(json: Option<&PipelineConfig>, frames: Option<u32>) -> PipelineConfig {
    let mut cfg = json.cloned().unwrap_or_default();
    if let Some(v) = frames {
        cfg.frames = v;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_full_config() {
        let json = dump_default_config();
        let configs: ScenarioConfigs = serde_json::from_str(&json).unwrap();
        assert!(configs.npu.is_some());
        assert!(configs.camera.is_some());
        assert!(configs.display.is_some());
        assert!(configs.codec.is_some());
        assert!(configs.gpu.is_some());
        assert!(configs.pipeline.is_some());
    }

    #[test]
    fn load_partial_config() {
        let json = r#"{ "npu": { "iterations": 50 } }"#;
        let configs: ScenarioConfigs = serde_json::from_str(json).unwrap();
        let npu = configs.npu.unwrap();
        assert_eq!(npu.iterations, 50);
        // Other fields should be default.
        assert_eq!(npu.clients, 4);
        assert!(configs.camera.is_none());
    }

    #[test]
    fn load_empty_config() {
        let json = "{}";
        let configs: ScenarioConfigs = serde_json::from_str(json).unwrap();
        assert!(configs.npu.is_none());
        assert!(configs.camera.is_none());
    }

    #[test]
    fn dump_roundtrip() {
        let json = dump_default_config();
        let configs: ScenarioConfigs = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string_pretty(&configs).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn resolve_npu_json_only() {
        let npu = NpuConfig {
            iterations: 50,
            clients: 2,
            ..Default::default()
        };
        let resolved = resolve_npu(Some(&npu), None, None);
        assert_eq!(resolved.iterations, 50);
        assert_eq!(resolved.clients, 2);
    }

    #[test]
    fn resolve_npu_cli_override() {
        let npu = NpuConfig {
            iterations: 50,
            ..Default::default()
        };
        let resolved = resolve_npu(Some(&npu), Some(10), None);
        assert_eq!(resolved.iterations, 10);
        assert_eq!(resolved.clients, 4);
    }

    #[test]
    fn resolve_npu_no_json() {
        let resolved = resolve_npu(None, Some(25), Some(8));
        assert_eq!(resolved.iterations, 25);
        assert_eq!(resolved.clients, 8);
    }

    #[test]
    fn resolve_camera_cli_override() {
        let cam = CameraConfig::default();
        let resolved = resolve_camera(Some(&cam), Some(3840), None, Some(200));
        assert_eq!(resolved.width, 3840);
        assert_eq!(resolved.height, 1080);
        assert_eq!(resolved.frames, 200);
    }

    #[test]
    fn resolve_pipeline_frames() {
        let resolved = resolve_pipeline(None, Some(60));
        assert_eq!(resolved.frames, 60);
        assert!(resolved.workloads.is_empty());
    }

    #[test]
    fn camera_format_serde() {
        use crate::cmd::scenario::camera::CameraFormat;
        let json = serde_json::to_string(&CameraFormat::Raw10).unwrap();
        assert_eq!(json, "\"raw10\"");
        let parsed: CameraFormat = serde_json::from_str("\"nv12\"").unwrap();
        assert!(matches!(parsed, CameraFormat::Nv12));
    }

    #[test]
    fn pipeline_workloads_serde() {
        let json = r#"{ "frames": 10, "workloads": { "camera": "cam_heap" } }"#;
        let cfg: PipelineConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.frames, 10);
        assert_eq!(cfg.workloads.get("camera").unwrap(), "cam_heap");
    }

    #[test]
    fn load_config_file_not_found() {
        let result = load_config(Path::new("/nonexistent/config.json"));
        assert!(result.is_err());
    }
}
