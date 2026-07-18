//! Grok Build 的 Imagine 图片/视频工具请求规范。
//!
//! Grok Build 不把媒体生成伪装成 Responses 的 content block：图片调用
//! `/images/*`，视频先调用 `/videos/generations` 再轮询 `/videos/{id}`。
//! 这里把适合 HTTP 客户端的 Build tool 输入规范化为上游 xAI payload。

use serde_json::{Value, json};

pub const IMAGE_QUALITY_MODEL: &str = "grok-imagine-image-quality";
pub const VIDEO_BASE_MODEL: &str = "grok-imagine-video";
pub const VIDEO_QUALITY_MODEL: &str = "grok-imagine-video-1.5-preview";

const DEFAULT_IMAGE_ASPECT_RATIO: &str = "auto";
const DEFAULT_VIDEO_RESOLUTION: &str = "480p";
const DEFAULT_VIDEO_DURATION: u64 = 6;
const VALID_VIDEO_RESOLUTIONS: &[&str] = &["480p", "720p"];
const VALID_VIDEO_ASPECT_RATIOS: &[&str] = &["1:1", "16:9", "9:16", "3:2", "2:3"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRequestError(String);

impl MediaRequestError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for MediaRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MediaRequestError {}

/// Build `image_gen(prompt, aspect_ratio)` → xAI `/images/generations`。
pub fn build_image_generation_body(request: &Value) -> Result<Value, MediaRequestError> {
    let prompt = required_string(request, "prompt")?;
    let aspect_ratio = optional_string(request, "aspect_ratio")
        .unwrap_or_else(|| DEFAULT_IMAGE_ASPECT_RATIO.to_string());
    Ok(json!({
        "model": IMAGE_QUALITY_MODEL,
        "prompt": prompt,
        "n": 1,
        "aspect_ratio": aspect_ratio,
        "resolution": "1k",
        "response_format": "b64_json",
    }))
}

/// Build `image_edit(prompt, image[], aspect_ratio)` → xAI `/images/edits`。
/// 单图使用 API 的 `image` object；多图使用 `images` array 且才发送比例。
pub fn build_image_edit_body(request: &Value) -> Result<Value, MediaRequestError> {
    let prompt = required_string(request, "prompt")?;
    let images = image_edit_references(request.get("image"))?;
    if images.is_empty() {
        return Err(MediaRequestError::new(
            "image_edit 需要至少一张 image 参考图",
        ));
    }
    let mut body = json!({
        "model": IMAGE_QUALITY_MODEL,
        "prompt": prompt,
        "n": 1,
        "resolution": "1k",
        "response_format": "b64_json",
    });
    if images.len() == 1 {
        body["image"] = json!({ "url": images[0] });
    } else {
        body["images"] = Value::Array(
            images
                .into_iter()
                .map(|url| json!({ "url": url }))
                .collect(),
        );
        body["aspect_ratio"] = Value::String(
            optional_string(request, "aspect_ratio")
                .unwrap_or_else(|| DEFAULT_IMAGE_ASPECT_RATIO.to_string()),
        );
    }
    Ok(body)
}

/// Build 的两个视频工具共用同一个上游 endpoint：
///
/// - `image_to_video`: `{ image, prompt?, duration?, resolution_name? }`
/// - `reference_to_video`: `{ images: [2..7], prompt, aspect_ratio,
///   duration?, resolution_name? }`
///
/// 代理不会读取客户端本地路径，因此图片只能是 HTTPS URL 或 base64 data URL；
/// 这正是 Grok Build 在把本地附件压缩/编码之后发往 xAI 的 wire 格式。
pub fn build_video_generation_body(request: &Value) -> Result<Value, MediaRequestError> {
    let single_image = request.get("image").map(image_reference).transpose()?;
    let references = image_references(request.get("images"), "images")?;
    if single_image.is_some() && !references.is_empty() {
        return Err(MediaRequestError::new(
            "image_to_video 与 reference_to_video 不能同时提供 image 和 images",
        ));
    }

    let duration = optional_duration(request)?.unwrap_or(DEFAULT_VIDEO_DURATION);
    if !matches!(duration, 6 | 10) {
        return Err(MediaRequestError::new(format!(
            "duration 只能是 6 或 10 秒，当前为 {duration}"
        )));
    }
    let resolution = optional_string(request, "resolution_name")
        .or_else(|| optional_string(request, "resolution"))
        .unwrap_or_else(|| DEFAULT_VIDEO_RESOLUTION.to_string());
    if !VALID_VIDEO_RESOLUTIONS.contains(&resolution.as_str()) {
        return Err(MediaRequestError::new(format!(
            "resolution_name 只能是 {}，当前为 {resolution}",
            VALID_VIDEO_RESOLUTIONS.join("、")
        )));
    }

    if let Some(image) = single_image {
        let prompt = optional_string(request, "prompt").unwrap_or_default();
        return Ok(json!({
            "model": VIDEO_QUALITY_MODEL,
            "prompt": prompt,
            "image": { "url": image },
            "duration": duration,
            "resolution": resolution,
        }));
    }

    if references.len() < 2 || references.len() > 7 {
        return Err(MediaRequestError::new(
            "reference_to_video 的 images 必须包含 2 到 7 张参考图",
        ));
    }
    let prompt = required_string(request, "prompt")?;
    let aspect_ratio = required_string(request, "aspect_ratio")?;
    if !VALID_VIDEO_ASPECT_RATIOS.contains(&aspect_ratio.as_str()) {
        return Err(MediaRequestError::new(format!(
            "aspect_ratio 只能是 {}，当前为 {aspect_ratio}",
            VALID_VIDEO_ASPECT_RATIOS.join("、")
        )));
    }
    Ok(json!({
        "model": VIDEO_BASE_MODEL,
        "prompt": prompt,
        "duration": duration,
        "aspect_ratio": aspect_ratio,
        "resolution": resolution,
        "reference_images": references
            .into_iter()
            .map(|url| json!({ "url": url }))
            .collect::<Vec<_>>(),
    }))
}

fn required_string(request: &Value, field: &str) -> Result<String, MediaRequestError> {
    optional_string(request, field)
        .ok_or_else(|| MediaRequestError::new(format!("{field} 不能为空")))
}

fn optional_string(request: &Value, field: &str) -> Option<String> {
    request
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn optional_duration(request: &Value) -> Result<Option<u64>, MediaRequestError> {
    let Some(value) = request.get("duration") else {
        return Ok(None);
    };
    match value {
        Value::Number(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| MediaRequestError::new("duration 必须是正整数、6 或 10")),
        Value::String(value) => value
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| MediaRequestError::new("duration 必须是 6 或 10")),
        Value::Null => Ok(None),
        _ => Err(MediaRequestError::new("duration 必须是数字或字符串")),
    }
}

fn image_references(value: Option<&Value>, field: &str) -> Result<Vec<String>, MediaRequestError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        Value::Array(values) => values.iter().map(image_reference).collect(),
        Value::Null => Ok(Vec::new()),
        _ => image_reference(value)
            .map(|image| vec![image])
            .map_err(|_| MediaRequestError::new(format!("{field} 必须是图片 URL 或图片 URL 数组"))),
    }
}

/// Grok Build 的 `image_edit` 会把本地文件或 data URL 解码、压缩后统一
/// 发送为 data URL。HTTP 代理不能安全地读取调用方机器上的文件，因此它的
/// 可移植交集只有 data URL；HTTPS 远程图片仍由视频工具支持。
fn image_edit_references(value: Option<&Value>) -> Result<Vec<String>, MediaRequestError> {
    // Grok Build 的 `ImageEditInput.image` 是 `Vec<String>`，即使只有
    // 一张参考图也通过数组传递。保持这个调用侧契约，随后才根据数量改写成
    // 上游 `/images/edits` 所要求的 `image` / `images` wire shape。
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| MediaRequestError::new("image_edit 的 image 必须是图片 data URL 数组"))?;
    let images = values
        .iter()
        .map(image_reference)
        .collect::<Result<Vec<_>, _>>()?;
    if images.iter().any(|image| !image.starts_with("data:image/")) {
        return Err(MediaRequestError::new(
            "image_edit 的 image 必须是 data:image/...;base64,...；Grok Build 会把本地参考图转成该格式，代理不会读取客户端本地路径或下载 HTTPS 图片",
        ));
    }
    Ok(images)
}

fn image_reference(value: &Value) -> Result<String, MediaRequestError> {
    let value = match value {
        Value::String(value) => value.as_str(),
        Value::Object(object) => object
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| MediaRequestError::new("图片对象必须包含 url"))?,
        _ => return Err(MediaRequestError::new("图片必须是 URL 字符串或 {url} 对象")),
    }
    .trim();
    if value.starts_with("https://") {
        return Ok(value.to_string());
    }
    if value.starts_with("data:image/") {
        let Some(separator) = value.find(',') else {
            return Err(MediaRequestError::new("图片 data URL 缺少 base64 内容"));
        };
        if value[..separator].contains(";base64") && separator + 1 < value.len() {
            return Ok(value.to_string());
        }
        return Err(MediaRequestError::new("图片 data URL 必须使用 base64 编码"));
    }
    Err(MediaRequestError::new(
        "图片必须是 HTTPS URL 或 data:image/...;base64,...；代理无法读取客户端本地路径",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_generation_matches_grok_build_wire_defaults() {
        let body = build_image_generation_body(&json!({ "prompt": "a capybara" })).unwrap();
        assert_eq!(body["model"], IMAGE_QUALITY_MODEL);
        assert_eq!(body["n"], 1);
        assert_eq!(body["aspect_ratio"], "auto");
        assert_eq!(body["resolution"], "1k");
        assert_eq!(body["response_format"], "b64_json");
    }

    #[test]
    fn image_edit_uses_single_and_multiple_api_shapes() {
        let single = build_image_edit_body(&json!({
            "prompt": "make it sunset",
            "image": ["data:image/png;base64,AA=="]
        }))
        .unwrap();
        assert!(single.get("image").is_some());
        assert!(single.get("images").is_none());

        let multiple = build_image_edit_body(&json!({
            "prompt": "combine them",
            "image": ["data:image/png;base64,AA==", "data:image/jpeg;base64,AA=="],
            "aspect_ratio": "16:9"
        }))
        .unwrap();
        assert_eq!(multiple["images"].as_array().unwrap().len(), 2);
        assert_eq!(multiple["aspect_ratio"], "16:9");
        assert!(
            build_image_edit_body(&json!({
                "prompt": "do not download remote files",
                "image": ["https://example.com/a.png"]
            }))
            .is_err()
        );
        assert!(
            build_image_edit_body(&json!({
                "prompt": "keep Build's array contract",
                "image": "data:image/png;base64,AA=="
            }))
            .is_err()
        );
    }

    #[test]
    fn image_to_video_uses_quality_model_and_defaults() {
        let body = build_video_generation_body(&json!({
            "image": "https://example.com/frame.png"
        }))
        .unwrap();
        assert_eq!(body["model"], VIDEO_QUALITY_MODEL);
        assert_eq!(body["duration"], 6);
        assert_eq!(body["resolution"], "480p");
        assert_eq!(body["image"]["url"], "https://example.com/frame.png");
    }

    #[test]
    fn reference_to_video_uses_base_model_and_validates_shape() {
        let body = build_video_generation_body(&json!({
            "prompt": "cinematic transition",
            "images": [
                "https://example.com/a.png",
                "data:image/jpeg;base64,AA=="
            ],
            "aspect_ratio": "16:9",
            "duration": "10",
            "resolution_name": "720p"
        }))
        .unwrap();
        assert_eq!(body["model"], VIDEO_BASE_MODEL);
        assert_eq!(body["duration"], 10);
        assert_eq!(body["reference_images"].as_array().unwrap().len(), 2);
        assert!(build_video_generation_body(&json!({ "images": [] })).is_err());
    }
}
