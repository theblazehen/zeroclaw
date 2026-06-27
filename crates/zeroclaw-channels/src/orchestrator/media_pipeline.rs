//! Automatic media understanding pipeline for inbound channel messages.
//!
//! Pre-processes media attachments (audio, images, video) before the agent sees
//! the message, enriching the text with human-readable annotations:
//!
//! - **Audio**: transcribed via the existing [`super::transcription`] infrastructure,
//!   prepended as `[Audio transcription: ...]`.
//! - **Images**: when a vision-capable model_provider is active, described as `[Image: <description>]`.
//!   Falls back to `[Image: attached]` when vision is unavailable.
//! - **Video**: summarised as `[Video summary: ...]` when an API is available,
//!   otherwise `[Video: attached]`.
//!
//! The pipeline is **opt-in** via `[media_pipeline] enabled = true` in config.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use zeroclaw_config::schema::MediaPipelineConfig;

use super::super::transcription::TranscriptionManager;

// Re-export media types from zeroclaw-types for backwards compatibility.
pub use zeroclaw_api::media::{MediaAttachment, MediaKind};

/// The media understanding pipeline.
///
/// Consumes a message's text and attachments, returning enriched text with
/// media annotations prepended.
pub struct MediaPipeline<'a> {
    config: &'a MediaPipelineConfig,
    transcription_manager: Option<&'a TranscriptionManager>,
    vision_available: bool,
    attachment_dir: Option<PathBuf>,
}

impl<'a> MediaPipeline<'a> {
    /// Create a new pipeline. `vision_available` indicates whether the current
    /// model provider supports vision (image description). `transcription_manager`
    /// is `None` when transcription is disabled at the channel level — audio
    /// attachments fall back to `[Audio: attached]` annotations.
    pub fn new(
        config: &'a MediaPipelineConfig,
        transcription_manager: Option<&'a TranscriptionManager>,
        vision_available: bool,
    ) -> Self {
        Self {
            config,
            transcription_manager,
            vision_available,
            attachment_dir: None,
        }
    }

    pub fn with_attachment_dir(mut self, attachment_dir: impl Into<PathBuf>) -> Self {
        self.attachment_dir = Some(attachment_dir.into());
        self
    }

    /// Process a message's attachments and return enriched text.
    ///
    /// If the pipeline is disabled via config, media-understanding features are
    /// skipped, but document attachments are still materialized so the agent can
    /// see a usable `[DOCUMENT:<path>]` marker.
    pub async fn process(&self, original_text: &str, attachments: &[MediaAttachment]) -> String {
        if attachments.is_empty() {
            return original_text.to_string();
        }

        let mut annotations = Vec::new();

        for attachment in attachments {
            match attachment.kind() {
                MediaKind::Audio if self.config.enabled && self.config.transcribe_audio => {
                    let annotation = self.process_audio(attachment).await;
                    annotations.push(annotation);
                }
                MediaKind::Image if self.config.enabled && self.config.describe_images => {
                    let annotation = self.process_image(attachment);
                    annotations.push(annotation);
                }
                MediaKind::Video if self.config.enabled && self.config.summarize_video => {
                    let annotation = self.process_video(attachment);
                    annotations.push(annotation);
                }
                MediaKind::Unknown => {
                    let annotation = self.process_document(attachment);
                    annotations.push(annotation);
                }
                _ => {}
            }
        }

        if annotations.is_empty() {
            return original_text.to_string();
        }

        let mut enriched = String::with_capacity(
            annotations.iter().map(|a| a.len() + 1).sum::<usize>() + original_text.len() + 2,
        );

        for annotation in &annotations {
            enriched.push_str(annotation);
            enriched.push('\n');
        }

        if !original_text.is_empty() {
            enriched.push('\n');
            enriched.push_str(original_text);
        }

        enriched.trim().to_string()
    }

    /// Transcribe an audio attachment using the existing transcription infra.
    async fn process_audio(&self, attachment: &MediaAttachment) -> String {
        let Some(manager) = self.transcription_manager else {
            return "[Audio: attached]".to_string();
        };

        match manager
            .transcribe(&attachment.data, &attachment.file_name)
            .await
        {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    "[Audio transcription: (empty)]".to_string()
                } else {
                    format!("[Audio transcription: {trimmed}]")
                }
            }
            Err(err) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"file": attachment.file_name, "error": format!("{}", err)})), "Media pipeline: audio transcription failed");
                "[Audio: transcription failed]".to_string()
            }
        }
    }

    /// Describe an image attachment.
    ///
    /// When vision is available, the image will be passed through to the
    /// model_provider as an `[IMAGE:]` marker and described by the model in the
    /// normal flow.
    fn process_image(&self, attachment: &MediaAttachment) -> String {
        if self.vision_available {
            let (mime, data) = image_payload_for_vision(attachment);
            let b64 = STANDARD.encode(data.as_ref());
            format!(
                "[Image: {} attached, will be processed by vision model]\n[IMAGE:data:{};base64,{}]",
                attachment.file_name, mime, b64
            )
        } else {
            format!("[Image: {} attached]", attachment.file_name)
        }
    }

    /// Summarize a video attachment.
    ///
    /// Video analysis requires external APIs not currently integrated.
    /// For now we add a placeholder annotation.
    fn process_video(&self, attachment: &MediaAttachment) -> String {
        format!("[Video: {} attached]", attachment.file_name)
    }

    fn process_document(&self, attachment: &MediaAttachment) -> String {
        let Some(attachment_dir) = &self.attachment_dir else {
            return format!("[Document: {} attached]", attachment.file_name);
        };

        match materialize_attachment(attachment_dir, attachment) {
            Ok(path) => format!(
                "[Document: {} attached]\n[DOCUMENT:{}]",
                attachment.file_name,
                path.display()
            ),
            Err(err) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"file": attachment.file_name, "error": format!("{}", err)})), "Media pipeline: document materialization failed");
                format!("[Document: {} attached]", attachment.file_name)
            }
        }
    }
}

fn materialize_attachment(
    base_dir: &Path,
    attachment: &MediaAttachment,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(base_dir)?;
    let file_name = safe_attachment_file_name(&attachment.file_name);
    let mut path = base_dir.join(&file_name);
    let mut suffix = 1usize;
    while path.exists() {
        let candidate = if let Some((stem, ext)) = file_name.rsplit_once('.') {
            format!("{stem}-{suffix}.{ext}")
        } else {
            format!("{file_name}-{suffix}")
        };
        path = base_dir.join(candidate);
        suffix += 1;
    }
    std::fs::write(&path, &attachment.data)?;
    Ok(path)
}

fn safe_attachment_file_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | '\0' => '_',
            other if other.is_control() => '_',
            other => other,
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_string();

    if sanitized.is_empty() {
        "attachment.bin".to_string()
    } else {
        sanitized
    }
}

fn image_payload_for_vision(attachment: &MediaAttachment) -> (String, Cow<'_, [u8]>) {
    let mime = attachment.mime_type.as_deref().unwrap_or("image/jpeg");

    #[cfg(feature = "image-normalization")]
    if is_webp_attachment(attachment, mime) {
        match webp_to_png(&attachment.data) {
            Ok(png) => return ("image/png".to_string(), Cow::Owned(png)),
            Err(err) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "file": attachment.file_name,
                            "error": format!("{}", err),
                            "error_key": "media_pipeline_webp_to_png_failed",
                        })),
                    "Media pipeline: failed to normalize WebP image for vision"
                );
            }
        }
    }

    (mime.to_string(), Cow::Borrowed(&attachment.data))
}

#[cfg(feature = "image-normalization")]
fn is_webp_attachment(attachment: &MediaAttachment, mime: &str) -> bool {
    mime.eq_ignore_ascii_case("image/webp")
        || attachment
            .file_name
            .rsplit_once('.')
            .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("webp"))
}

#[cfg(feature = "image-normalization")]
fn webp_to_png(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory_with_format(data, image::ImageFormat::WebP)?;
    let mut cursor = std::io::Cursor::new(Vec::new());
    image.write_to(&mut cursor, image::ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_pipeline_config(enabled: bool) -> MediaPipelineConfig {
        MediaPipelineConfig {
            enabled,
            transcribe_audio: true,
            describe_images: true,
            summarize_video: true,
        }
    }

    fn sample_audio() -> MediaAttachment {
        MediaAttachment {
            file_name: "voice.ogg".to_string(),
            data: vec![0u8; 100],
            mime_type: Some("audio/ogg".to_string()),
        }
    }

    fn sample_image() -> MediaAttachment {
        MediaAttachment {
            file_name: "photo.jpg".to_string(),
            data: vec![0u8; 50],
            mime_type: Some("image/jpeg".to_string()),
        }
    }

    fn sample_video() -> MediaAttachment {
        MediaAttachment {
            file_name: "clip.mp4".to_string(),
            data: vec![0u8; 200],
            mime_type: Some("video/mp4".to_string()),
        }
    }

    fn sample_document() -> MediaAttachment {
        MediaAttachment {
            file_name: "forms.pdf".to_string(),
            data: b"pdf bytes".to_vec(),
            mime_type: Some("application/pdf".to_string()),
        }
    }

    #[test]
    fn media_kind_from_mime() {
        let audio = MediaAttachment {
            file_name: "file".to_string(),
            data: vec![],
            mime_type: Some("audio/ogg".to_string()),
        };
        assert_eq!(audio.kind(), MediaKind::Audio);

        let image = MediaAttachment {
            file_name: "file".to_string(),
            data: vec![],
            mime_type: Some("image/png".to_string()),
        };
        assert_eq!(image.kind(), MediaKind::Image);

        let video = MediaAttachment {
            file_name: "file".to_string(),
            data: vec![],
            mime_type: Some("video/mp4".to_string()),
        };
        assert_eq!(video.kind(), MediaKind::Video);
    }

    #[test]
    fn media_kind_from_extension() {
        let audio = MediaAttachment {
            file_name: "voice.ogg".to_string(),
            data: vec![],
            mime_type: None,
        };
        assert_eq!(audio.kind(), MediaKind::Audio);

        let image = MediaAttachment {
            file_name: "photo.png".to_string(),
            data: vec![],
            mime_type: None,
        };
        assert_eq!(image.kind(), MediaKind::Image);

        let video = MediaAttachment {
            file_name: "clip.mp4".to_string(),
            data: vec![],
            mime_type: None,
        };
        assert_eq!(video.kind(), MediaKind::Video);

        let unknown = MediaAttachment {
            file_name: "data.bin".to_string(),
            data: vec![],
            mime_type: None,
        };
        assert_eq!(unknown.kind(), MediaKind::Unknown);
    }

    #[tokio::test]
    async fn disabled_pipeline_returns_original_text() {
        let config = default_pipeline_config(false);
        let pipeline = MediaPipeline::new(&config, None, false);

        let result = pipeline.process("hello", &[sample_audio()]).await;
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn empty_attachments_returns_original_text() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, false);

        let result = pipeline.process("hello", &[]).await;
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn image_annotation_with_vision() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, true);

        let result = pipeline.process("check this", &[sample_image()]).await;
        assert!(
            result.contains("[Image: photo.jpg attached, will be processed by vision model]"),
            "expected vision annotation, got: {result}"
        );
        assert!(
            result.contains("[IMAGE:data:image/jpeg;base64,"),
            "expected image data marker, got: {result}"
        );
        assert!(result.contains("check this"));
    }

    #[cfg(feature = "image-normalization")]
    #[tokio::test]
    async fn webp_image_is_normalized_to_png_for_vision() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, true);
        let mut cursor = std::io::Cursor::new(Vec::new());
        let webp = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([255, 0, 0, 255]),
        ));
        webp.write_to(&mut cursor, image::ImageFormat::WebP)
            .expect("test WebP should encode");

        let sticker = MediaAttachment {
            file_name: "sticker.webp".to_string(),
            data: cursor.into_inner(),
            mime_type: Some("image/webp".to_string()),
        };

        let result = pipeline.process("what is this?", &[sticker]).await;

        assert!(result.contains("[IMAGE:data:image/png;base64,"));
        assert!(!result.contains("[IMAGE:data:image/webp;base64,"));
        assert!(result.contains("what is this?"));
    }

    #[tokio::test]
    async fn image_annotation_without_vision() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, false);

        let result = pipeline.process("check this", &[sample_image()]).await;
        assert!(
            result.contains("[Image: photo.jpg attached]"),
            "expected basic image annotation, got: {result}"
        );
        assert!(
            !result.contains("[IMAGE:data:"),
            "non-vision path must not inline image data, got: {result}"
        );
    }

    #[tokio::test]
    async fn video_annotation() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, false);

        let result = pipeline.process("watch", &[sample_video()]).await;
        assert!(
            result.contains("[Video: clip.mp4 attached]"),
            "expected video annotation, got: {result}"
        );
    }

    #[tokio::test]
    async fn audio_without_transcription_enabled() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, false);

        let result = pipeline.process("", &[sample_audio()]).await;
        assert_eq!(result, "[Audio: attached]");
    }

    #[tokio::test]
    async fn document_attachment_is_materialized_with_marker() {
        let config = default_pipeline_config(true);
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let pipeline =
            MediaPipeline::new(&config, None, false).with_attachment_dir(temp.path().to_path_buf());

        let result = pipeline.process("", &[sample_document()]).await;
        assert!(
            result.contains("[Document: forms.pdf attached]"),
            "missing document annotation: {result}"
        );
        assert!(
            result.contains("[DOCUMENT:"),
            "missing document marker: {result}"
        );
        assert!(
            temp.path().join("forms.pdf").exists(),
            "document was not written to disk"
        );
    }

    #[tokio::test]
    async fn document_attachment_filename_is_sanitized() {
        let config = default_pipeline_config(true);
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let pipeline =
            MediaPipeline::new(&config, None, false).with_attachment_dir(temp.path().to_path_buf());
        let attachment = MediaAttachment {
            file_name: "../forms.pdf".to_string(),
            data: b"pdf bytes".to_vec(),
            mime_type: Some("application/pdf".to_string()),
        };

        let result = pipeline.process("", &[attachment]).await;
        assert!(result.contains("[DOCUMENT:"), "missing marker: {result}");
        assert!(temp.path().join("_forms.pdf").exists());
        assert!(!temp.path().join("..").join("forms.pdf").exists());
    }

    #[tokio::test]
    async fn disabled_pipeline_still_materializes_documents() {
        let config = default_pipeline_config(false);
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let pipeline =
            MediaPipeline::new(&config, None, false).with_attachment_dir(temp.path().to_path_buf());

        let result = pipeline.process("[Document]", &[sample_document()]).await;
        assert!(result.contains("[DOCUMENT:"), "missing marker: {result}");
        assert!(
            result.contains("[Document]"),
            "missing original text: {result}"
        );
    }

    #[tokio::test]
    async fn multiple_attachments_produce_multiple_annotations() {
        let config = default_pipeline_config(true);
        let pipeline = MediaPipeline::new(&config, None, false);

        let attachments = vec![sample_audio(), sample_image(), sample_video()];
        let result = pipeline.process("context", &attachments).await;

        assert!(
            result.contains("[Audio: attached]"),
            "missing audio annotation"
        );
        assert!(
            result.contains("[Image: photo.jpg attached]"),
            "missing image annotation"
        );
        assert!(
            result.contains("[Video: clip.mp4 attached]"),
            "missing video annotation"
        );
        assert!(result.contains("context"), "missing original text");
    }

    #[tokio::test]
    async fn disabled_sub_features_skip_processing() {
        let config = MediaPipelineConfig {
            enabled: true,
            transcribe_audio: false,
            describe_images: false,
            summarize_video: false,
        };
        let pipeline = MediaPipeline::new(&config, None, false);

        let attachments = vec![sample_audio(), sample_image(), sample_video()];
        let result = pipeline.process("hello", &attachments).await;
        assert_eq!(result, "hello");
    }
}
