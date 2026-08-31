use std::io::Cursor;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{ColorType, DynamicImage, GenericImageView, ImageEncoder, ImageFormat};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::exec_sessions::SessionState;
use crate::safe_path::resolve_safe_path;
use crate::tool::{Tool, ToolBehavior, parse_tool_args};
use crate::types::{AppConfig, ToolResult};

/// Images are base64-inlined into the response, which roughly grows them by a
/// third. 5 MB of source keeps a single tool result within what an MCP client
/// will accept.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const PROMPT_IMAGE_PATCH_SIZE: u32 = 32;
const HIGH_DETAIL_MAX_DIMENSION: u32 = 2048;
const HIGH_DETAIL_MAX_PATCHES: usize = 2_500;
const ORIGINAL_DETAIL_MAX_DIMENSION: u32 = 6000;
const ORIGINAL_DETAIL_MAX_PATCHES: usize = 10_000;

pub struct ViewImage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewImageDetail {
    High,
    Original,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewImageArgs {
    path: String,
    detail: Option<String>,
}

fn image_dimensions_fit(width: u32, height: u32, max_dimension: u32, max_patches: usize) -> bool {
    let patches_wide = width.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patches_high = height.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patch_count = u64::from(patches_wide) * u64::from(patches_high);
    width <= max_dimension && height <= max_dimension && patch_count <= max_patches as u64
}

fn output_dimensions(
    width: u32,
    height: u32,
    max_dimension: u32,
    max_patches: usize,
) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    if image_dimensions_fit(width, height, max_dimension, max_patches) {
        return (width, height);
    }

    let max_dimension_scale = (f64::from(max_dimension) / f64::from(width.max(height))).min(1.0);
    let width = ((f64::from(width) * max_dimension_scale).round() as u32).max(1);
    let height = ((f64::from(height) * max_dimension_scale).round() as u32).max(1);
    if image_dimensions_fit(width, height, max_dimension, max_patches) {
        return (width, height);
    }

    let width_f64 = f64::from(width);
    let height_f64 = f64::from(height);
    let patch_size = f64::from(PROMPT_IMAGE_PATCH_SIZE);
    let mut scale = (patch_size * patch_size * max_patches as f64 / width_f64 / height_f64).sqrt();
    let scaled_patches_wide = width_f64 * scale / patch_size;
    let scaled_patches_high = height_f64 * scale / patch_size;
    scale *= (scaled_patches_wide.floor() / scaled_patches_wide)
        .min(scaled_patches_high.floor() / scaled_patches_high);

    (
        ((width_f64 * scale).floor() as u32).max(1),
        ((height_f64 * scale).floor() as u32).max(1),
    )
}

fn supported_format(bytes: &[u8]) -> Option<ImageFormat> {
    match image::guess_format(bytes).ok()? {
        format @ (ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP) => {
            Some(format)
        }
        _ => None,
    }
}

fn output_mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::WebP => "image/webp",
        _ => "image/png",
    }
}

fn can_preserve_source_bytes(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    )
}

fn encode_image(
    image: &DynamicImage,
    preferred_format: ImageFormat,
) -> Result<(Vec<u8>, ImageFormat), String> {
    let target_format = match preferred_format {
        ImageFormat::Jpeg => ImageFormat::Jpeg,
        ImageFormat::WebP => ImageFormat::WebP,
        _ => ImageFormat::Png,
    };
    let mut buffer = Vec::new();
    match target_format {
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            PngEncoder::new(&mut buffer)
                .write_image(
                    rgba.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .map_err(|error| error.to_string())?;
        }
        ImageFormat::Jpeg => {
            JpegEncoder::new_with_quality(&mut buffer, 85)
                .encode_image(image)
                .map_err(|error| error.to_string())?;
        }
        ImageFormat::WebP => {
            let rgba = image.to_rgba8();
            WebPEncoder::new_lossless(&mut buffer)
                .write_image(
                    rgba.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .map_err(|error| error.to_string())?;
        }
        _ => unreachable!("unsupported output image format"),
    }
    Ok((buffer, target_format))
}

fn prepare_image(
    input_path: &str,
    bytes: Vec<u8>,
    detail: ViewImageDetail,
) -> Result<(Vec<u8>, &'static str), String> {
    let format = supported_format(&bytes).ok_or_else(|| {
        format!("Not a recognised image file: {input_path}. Supported: PNG, JPEG, GIF, WebP.")
    })?;
    let image = image::load(Cursor::new(&bytes), format).map_err(|_| {
        format!("Not a recognised image file: {input_path}. Supported: PNG, JPEG, GIF, WebP.")
    })?;
    let (source_width, source_height) = image.dimensions();
    let (max_dimension, max_patches) = match detail {
        ViewImageDetail::High => (HIGH_DETAIL_MAX_DIMENSION, HIGH_DETAIL_MAX_PATCHES),
        ViewImageDetail::Original => (ORIGINAL_DETAIL_MAX_DIMENSION, ORIGINAL_DETAIL_MAX_PATCHES),
    };
    let (target_width, target_height) =
        output_dimensions(source_width, source_height, max_dimension, max_patches);

    if (target_width, target_height) == (source_width, source_height)
        && can_preserve_source_bytes(format)
    {
        return Ok((bytes, output_mime(format)));
    }

    let prepared = if (target_width, target_height) == (source_width, source_height) {
        image
    } else {
        image.resize_exact(target_width, target_height, FilterType::Triangle)
    };
    let preferred_format = if can_preserve_source_bytes(format) {
        format
    } else {
        ImageFormat::Png
    };
    let (bytes, output_format) = encode_image(&prepared, preferred_format)?;
    Ok((bytes, output_mime(output_format)))
}

#[async_trait]
impl Tool for ViewImage {
    fn name(&self) -> &'static str {
        "view_image"
    }

    fn title(&self) -> String {
        "View image".to_string()
    }

    fn behavior(&self) -> ToolBehavior {
        ToolBehavior::new(
            true,
            false,
            true,
            false,
            "Reads and returns one local image without modifying state.",
        )
    }

    fn description(&self) -> String {
        "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk.".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Local filesystem path to an image file."
                },
                "detail": {
                    "type": "string",
                    "enum": ["high", "original"],
                    "description": "Image detail level. Defaults to `high`; use `original` to preserve exact resolution."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, config: &AppConfig, _session: &SessionState) -> ToolResult {
        let ViewImageArgs {
            path: input_path,
            detail,
        } = match parse_tool_args(args) {
            Ok(args) => args,
            Err(error) => return *error,
        };
        let detail = match detail.as_deref() {
            None | Some("high") => ViewImageDetail::High,
            Some("original") => ViewImageDetail::Original,
            Some(detail) => {
                return ToolResult::error(format!(
                    "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `{detail}`"
                ));
            }
        };

        let file_path = match resolve_safe_path(&input_path, &config.work_dir, false) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let metadata = match tokio::fs::metadata(&file_path).await {
            Ok(m) if m.is_file() => m,
            _ => return ToolResult::error(format!("File not found: {input_path}")),
        };

        if metadata.len() > MAX_IMAGE_BYTES {
            return ToolResult::error(format!(
                "Image too large: {} bytes (limit {}). Resize it before viewing.",
                metadata.len(),
                MAX_IMAGE_BYTES
            ));
        }

        let bytes = match tokio::fs::read(&file_path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(e.to_string()),
        };

        let (bytes, mime_type) = match prepare_image(&input_path, bytes, detail) {
            Ok(prepared) => prepared,
            Err(error) => return ToolResult::error(error),
        };

        ToolResult::image(STANDARD.encode(&bytes), mime_type)
    }
}
