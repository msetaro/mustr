//! Attachments: the images and files the composer sends with a message, and the images the
//! timeline shows.
//!
//! - **Sending.** An image or file pasted, dropped, or picked goes up as soon as it's added, as
//!   in T3's web app (`chat/useComposerAttachmentIntake.ts`, `lib/attachmentUploadQueue.ts`):
//!   `attachments.createUploadUrl` mints a URL for its size and type (`type: "file"` for a
//!   file), the bytes go to it ([`ThreadBackend::upload`]), and the message names it by the
//!   attachment id T3 gave (`message.dispatch.attachments`). The strip in the composer shows each
//!   one's progress; a failed upload can be retried, and removing one deletes what T3 minted
//!   for it. T3's limits hold: 100 attachments, images up to 10 MiB (shrunk to fit), files up to
//!   50 MiB (`chat/composerAttachmentFiles.ts`).
//! - **Big pastes.** Pasted text of 32 KiB or more goes in as a file, `pasted-text.txt`, as
//!   T3's composer folds it (`client-runtime/src/textPaste.ts`); `mod-shift-v` pastes it as
//!   text.
//! - **Showing.** An image in a message, or a screenshot in a tool's output, loads through
//!   `assets.createUrl`, which signs a URL for it, then [`ThreadBackend::fetch_asset`]. Clicking
//!   a thumbnail opens it full size ([`ImagePreview`]).

use crate::{
    backend::{Access, AttachmentUpload, ThreadBackend, ThreadBackendExt as _},
    commands::{self, Delivery},
    thread_view::ThreadView,
};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{
    App, AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, Image,
    ImageFormat, IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString,
    Styled as _, Task, Window, img,
};
use image::{DynamicImage, ImageDecoder as _, ImageReader, imageops::FilterType};
use mustr_ui::{palette, tokens, tokens::scaled};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_api::{
    rpc,
    tagged::{AssetResource, ChatAttachment},
    types::{
        AssetCreateUrlInput, AssetResourceAttachment, AssetResourceAttachmentTag,
        AssetResourceToolOutputImage, AssetResourceToolOutputImageTag,
        AttachmentCreateUploadUrlInput, AttachmentCreateUploadUrlInputVariant0MimeType,
        AttachmentCreateUploadUrlInputVariant1Type, AttachmentDeleteInput,
        OrchestrationV2ThreadLaunchInput,
    },
};
use ui::{Color, Label, LabelCommon as _, LabelSize, prelude::*};
use workspace::{ModalView, Workspace};

/// The largest image T3 sends to an agent (`PROVIDER_SEND_TURN_MAX_IMAGE_BYTES` in T3's
/// `packages/contracts/src/chatAttachment.ts`).
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// The most attachments one message takes (`PROVIDER_SEND_TURN_MAX_ATTACHMENTS`).
pub const MAX_ATTACHMENTS: usize = 100;
/// The most image bytes one message takes (`PROVIDER_SEND_TURN_MAX_TOTAL_IMAGE_BYTES`).
pub const MAX_TOTAL_IMAGE_BYTES: usize = 80 * 1024 * 1024;
/// The largest file other than an image T3 takes (`PROVIDER_SEND_TURN_MAX_FILE_BYTES`).
pub const MAX_FILE_BYTES: usize = 50 * 1024 * 1024;
/// Pasted text this long, in characters or UTF-8 bytes, goes in as a file
/// (`PASTED_TEXT_ATTACHMENT_THRESHOLD_BYTES` in T3's `client-runtime/src/textPaste.ts`).
pub const PASTED_TEXT_THRESHOLD: usize = 32 * 1024;
/// A big paste's type, as T3 makes the file.
pub const PASTED_TEXT_MIME_TYPE: &str = "text/plain;charset=utf-8";
/// The largest image mustr will shrink to fit; larger ones are refused
/// (`MAX_COMPRESSIBLE_SOURCE_BYTES` in T3's `apps/web/src/lib/imageCompression.ts`).
pub const MAX_SOURCE_BYTES: usize = 50 * 1024 * 1024;
/// PNG and JPEG images are scaled down to this longest edge before they go up
/// (`IMAGE_ATTACHMENT_MAX_EDGE` in T3's `packages/client-runtime/src/imageAttachmentSize.ts`).
pub const MAX_EDGE: u32 = 2000;
/// The JPEG qualities, then scales, tried to bring an image under [`MAX_IMAGE_BYTES`]
/// (`QUALITY_STEPS` and `FALLBACK_SCALE_STEPS` in T3).
const JPEG_QUALITIES: [u8; 4] = [92, 85, 78, 68];
const FALLBACK_SCALES: [f32; 2] = [0.75, 0.55];

/// The image types agents take (`PROVIDER_SEND_TURN_SUPPORTED_IMAGE_MIME_TYPES`). A BMP or
/// TIFF, which a clipboard can hold, is converted to PNG first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImageType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageType {
    pub fn mime_type(self) -> &'static str {
        self.format().mime_type()
    }

    pub fn format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Gif => ImageFormat::Gif,
            Self::Webp => ImageFormat::Webp,
        }
    }

    fn from_format(format: ImageFormat) -> Option<Self> {
        match format {
            ImageFormat::Png => Some(Self::Png),
            ImageFormat::Jpeg => Some(Self::Jpeg),
            ImageFormat::Gif => Some(Self::Gif),
            ImageFormat::Webp => Some(Self::Webp),
            _ => None,
        }
    }

    fn codec(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::Gif => image::ImageFormat::Gif,
            Self::Webp => image::ImageFormat::WebP,
        }
    }

    fn wire(self) -> AttachmentCreateUploadUrlInputVariant0MimeType {
        match self {
            Self::Png => AttachmentCreateUploadUrlInputVariant0MimeType::ImagePng,
            Self::Jpeg => AttachmentCreateUploadUrlInputVariant0MimeType::ImageJpeg,
            Self::Gif => AttachmentCreateUploadUrlInputVariant0MimeType::ImageGif,
            Self::Webp => AttachmentCreateUploadUrlInputVariant0MimeType::ImageWebp,
        }
    }
}

/// The format of an image's bytes, from their signature.
pub fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if bytes.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some(ImageFormat::Tiff)
    } else {
        None
    }
}

/// An image or another file, ready to upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewAttachment {
    Image(NewImage),
    File(NewFile),
}

/// Whether a file is named like an image agents take, or one mustr converts.
pub(crate) fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
            )
        })
}

/// An image ready to upload: a type agents take, within T3's size limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewImage {
    pub name: String,
    pub image_type: ImageType,
    pub bytes: Vec<u8>,
}

impl NewImage {
    /// Prepares an image as T3's web app does (`prepareImageForAttachment`): a PNG or JPEG
    /// whose longest edge is over [`MAX_EDGE`] is scaled down, and one still over
    /// [`MAX_IMAGE_BYTES`] is re-encoded as a JPEG. A BMP or TIFF becomes a PNG first. `name` is
    /// its file name, if it has one; pasted images are named after their type, `image.png`.
    /// Decoding is slow, so call it off the main thread.
    pub fn prepare(
        name: Option<String>,
        format: Option<ImageFormat>,
        bytes: Vec<u8>,
    ) -> Result<Self, String> {
        let shown_name = name.clone().unwrap_or_else(|| "The image".to_string());
        let unsupported = || {
            format!(
                "{shown_name} isn't a supported image type. Attach GIF, JPEG, PNG, or WebP images."
            )
        };
        let too_large = |size: usize| {
            format!(
                "{shown_name} is {}; images can be up to {}.",
                format_size(size),
                format_size(MAX_IMAGE_BYTES)
            )
        };
        if bytes.is_empty() {
            return Err(format!("{shown_name} is empty."));
        }
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(too_large(bytes.len()));
        }
        let format = sniff_format(&bytes).or(format).ok_or_else(unsupported)?;
        let mut name = name.unwrap_or_else(|| "image".to_string());
        let (image_type, mut bytes, mut decoded) = match ImageType::from_format(format) {
            Some(image_type) => (image_type, bytes, None),
            None => {
                let codec = match format {
                    ImageFormat::Bmp => image::ImageFormat::Bmp,
                    ImageFormat::Tiff => image::ImageFormat::Tiff,
                    _ => return Err(unsupported()),
                };
                let decoded = decode(&bytes, codec).map_err(|error| {
                    log::warn!("couldn't read a {format:?} image: {error:#}");
                    unsupported()
                })?;
                let png = encode(&decoded, ImageType::Png).map_err(|_| unsupported())?;
                name = with_extension(&name, "png");
                (ImageType::Png, png, Some(decoded))
            }
        };
        let mut image_type = image_type;

        // Scale big PNGs and JPEGs down, as T3 does; it leaves GIFs and WebPs alone.
        if matches!(image_type, ImageType::Png | ImageType::Jpeg)
            && let Some((width, height)) = dimensions(&bytes, image_type)
            && let Some((fit_width, fit_height)) = fit_to_max_edge(width, height)
        {
            let source = match decoded.take() {
                Some(decoded) => decoded,
                None => decode(&bytes, image_type.codec()).map_err(|_| unsupported())?,
            };
            let fitted = source.resize_exact(fit_width, fit_height, FilterType::CatmullRom);
            bytes = encode(&fitted, image_type).map_err(|_| unsupported())?;
            decoded = Some(fitted);
        }

        if bytes.len() > MAX_IMAGE_BYTES {
            if !matches!(image_type, ImageType::Png | ImageType::Jpeg) {
                return Err(too_large(bytes.len()));
            }
            let source = match decoded.take() {
                Some(decoded) => decoded,
                None => decode(&bytes, image_type.codec()).map_err(|_| unsupported())?,
            };
            bytes = jpeg_within(&source, MAX_IMAGE_BYTES).ok_or_else(|| too_large(bytes.len()))?;
            image_type = ImageType::Jpeg;
            name = with_extension(&name, "jpg");
        }

        if !name.contains('.') {
            name = format!("{name}.{}", image_type.format().extension());
        }
        Ok(Self {
            name,
            image_type,
            bytes,
        })
    }

    /// Reads and prepares an image file, as one dropped or picked.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let shown_name = name.clone().unwrap_or_else(|| path.display().to_string());
        let metadata = std::fs::metadata(path).map_err(|error| format!("{shown_name}: {error}"))?;
        if metadata.is_dir() {
            return Err(format!("{shown_name} is a folder, not an image."));
        }
        if metadata.len() as usize > MAX_SOURCE_BYTES {
            return Err(format!(
                "{shown_name} is {}; images can be up to {}.",
                format_size(metadata.len() as usize),
                format_size(MAX_IMAGE_BYTES)
            ));
        }
        let bytes = std::fs::read(path).map_err(|error| format!("{shown_name}: {error}"))?;
        Self::prepare(name, None, bytes)
    }
}

/// Whether pasted text is big enough to go in as a file (`pastedTextDisposition` in T3).
pub fn is_big_paste(text: &str) -> bool {
    !text.is_empty()
        && (text.len() >= PASTED_TEXT_THRESHOLD || text.chars().count() >= PASTED_TEXT_THRESHOLD)
}

/// The name of the next big paste: `pasted-text.txt`, then `pasted-text-2.txt` and on, past the
/// names the composer has (`nextPastedTextFileName` in T3).
pub fn next_pasted_text_name<'a>(existing: impl IntoIterator<Item = &'a str>) -> String {
    let names = existing
        .into_iter()
        .map(str::to_lowercase)
        .collect::<collections::HashSet<_>>();
    if !names.contains("pasted-text.txt") {
        return "pasted-text.txt".to_string();
    }
    (2..)
        .map(|sequence| format!("pasted-text-{sequence}.txt"))
        .find(|name| !names.contains(name))
        .expect("an unused name")
}

/// What T3 says about a file over its limit (`fileAttachmentTooLargeMessage` in T3's
/// `client-runtime/src/state/attachments.ts`).
pub fn file_too_large_message(name: &str, max_bytes: usize) -> String {
    const MB: usize = 1024 * 1024;
    let limit = if max_bytes >= MB && max_bytes.is_multiple_of(MB) {
        format!("{} MB", max_bytes / MB)
    } else if max_bytes >= 1024 && max_bytes.is_multiple_of(1024) {
        format!("{} KB", max_bytes / 1024)
    } else if max_bytes == 1 {
        "1 byte".to_string()
    } else {
        format!("{max_bytes} bytes")
    };
    format!("'{name}' exceeds the {limit} attachment limit.")
}

/// A file's type from its name, as a browser would give it to T3's composer, or
/// `application/octet-stream` (`composerFileAttachmentMimeType` in T3).
pub fn mime_type_for(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" | "mts" | "cts" => "text/typescript",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "svg" => "image/svg+xml",
        "dng" => "image/x-adobe-dng",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "sh" | "bash" | "zsh" => "application/x-sh",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "hpp" => "text/x-c++",
        "swift" => "text/x-swift",
        "rb" => "text/x-ruby",
        "sql" => "application/sql",
        "diff" | "patch" => "text/x-diff",
        _ => "application/octet-stream",
    }
}

/// A file other than an image, ready to upload: within T3's limit, and not empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewFile {
    pub name: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    /// It's a big paste, which T3 hands the agent as a path only.
    pub pasted_text: bool,
}

impl NewFile {
    /// Reads a file, as one dropped, picked, or copied.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let metadata = std::fs::metadata(path)
            .map_err(|_| format!("'{name}' is empty or could not be read."))?;
        if metadata.is_dir() {
            return Err(format!(
                "'{name}' is a folder; attach the files in it instead."
            ));
        }
        if metadata.len() as usize > MAX_FILE_BYTES {
            return Err(file_too_large_message(&name, MAX_FILE_BYTES));
        }
        let bytes =
            std::fs::read(path).map_err(|_| format!("'{name}' is empty or could not be read."))?;
        Self::new(name, mime_type_for_path(path).to_string(), bytes, false)
    }

    /// A big paste as a file named `name` (`foldPastedText` in T3's
    /// `chat/useComposerAttachmentIntake.ts`).
    pub fn pasted_text(name: String, text: &str) -> Result<Self, String> {
        Self::new(
            name,
            PASTED_TEXT_MIME_TYPE.to_string(),
            text.as_bytes().to_vec(),
            true,
        )
    }

    fn new(
        name: String,
        mime_type: String,
        bytes: Vec<u8>,
        pasted_text: bool,
    ) -> Result<Self, String> {
        if bytes.is_empty() {
            return Err(format!("'{name}' is empty or could not be read."));
        }
        if bytes.len() > MAX_FILE_BYTES {
            return Err(file_too_large_message(&name, MAX_FILE_BYTES));
        }
        Ok(Self {
            name,
            mime_type,
            bytes,
            pasted_text,
        })
    }
}

fn mime_type_for_path(path: &Path) -> &'static str {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or("application/octet-stream", mime_type_for)
}

/// Whether T3 takes files other than images, from its config (`fileAttachmentStagingLimit` in
/// T3's `chat/composerAttachmentFiles.ts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileUploads {
    /// Its config isn't read yet: files up to T3's limit go, as T3's composer lets them.
    #[default]
    Unknown,
    /// Files up to this many bytes.
    UpTo(usize),
    /// It takes images only.
    Unsupported,
}

impl FileUploads {
    /// Reads `server.getConfig`'s `environment.capabilities`: uploads must be on and a file
    /// limit given; a limit over T3's own is cut to it (`clampFileAttachmentUploadBytes`).
    pub fn from_config(config: &serde_json::Value) -> Self {
        let capabilities = &config["environment"]["capabilities"];
        if !capabilities.is_object() {
            return Self::Unknown;
        }
        let max = capabilities["fileAttachments"]["maxUploadBytes"].as_u64();
        match (capabilities["attachmentUploads"].as_bool(), max) {
            (Some(true), Some(max)) => Self::UpTo((max as usize).min(MAX_FILE_BYTES)),
            _ => Self::Unsupported,
        }
    }

    /// The largest file that goes, or `None` when none do.
    pub fn limit(self) -> Option<usize> {
        match self {
            Self::Unknown => Some(MAX_FILE_BYTES),
            Self::UpTo(max) => Some(max),
            Self::Unsupported => None,
        }
    }
}

fn with_extension(name: &str, extension: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "image".to_string());
    format!("{stem}.{extension}")
}

/// Decodes an image upright, following its EXIF orientation, as browsers draw it.
fn decode(bytes: &[u8], format: image::ImageFormat) -> Result<DynamicImage> {
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format).into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn dimensions(bytes: &[u8], image_type: ImageType) -> Option<(u32, u32)> {
    ImageReader::with_format(Cursor::new(bytes), image_type.codec())
        .into_dimensions()
        .ok()
}

/// The size an image is scaled to so its longest edge is [`MAX_EDGE`], or `None` if it fits
/// (`fitImageAttachmentSize` in T3).
pub fn fit_to_max_edge(width: u32, height: u32) -> Option<(u32, u32)> {
    let longest = width.max(height);
    if longest <= MAX_EDGE {
        return None;
    }
    let scale = MAX_EDGE as f64 / longest as f64;
    let fit = |edge: u32| ((edge as f64 * scale).round() as u32).clamp(1, MAX_EDGE);
    Some((fit(width), fit(height)))
}

fn encode(image: &DynamicImage, image_type: ImageType) -> Result<Vec<u8>> {
    match image_type {
        ImageType::Jpeg => encode_jpeg(image, JPEG_QUALITIES[0]),
        ImageType::Png => {
            let mut bytes = Cursor::new(Vec::new());
            image.write_to(&mut bytes, image::ImageFormat::Png)?;
            Ok(bytes.into_inner())
        }
        other => Err(anyhow!("mustr doesn't re-encode {other:?} images")),
    }
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let rgb = DynamicImage::ImageRgb8(image.to_rgb8());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality).encode_image(&rgb)?;
    Ok(bytes)
}

/// A JPEG of `image` within `max_bytes`, trying lower qualities, then smaller sizes.
fn jpeg_within(image: &DynamicImage, max_bytes: usize) -> Option<Vec<u8>> {
    let fits = |image: &DynamicImage| {
        JPEG_QUALITIES
            .iter()
            .filter_map(|quality| encode_jpeg(image, *quality).ok())
            .find(|bytes| bytes.len() <= max_bytes)
    };
    fits(image).or_else(|| {
        FALLBACK_SCALES.iter().find_map(|scale| {
            let width = ((image.width() as f32 * scale).round() as u32).max(1);
            let height = ((image.height() as f32 * scale).round() as u32).max(1);
            fits(&image.resize_exact(width, height, FilterType::CatmullRom))
        })
    })
}

/// "1.2 MB", for limits and errors.
pub fn format_size(bytes: usize) -> String {
    const MB: f64 = 1024. * 1024.;
    if bytes as f64 >= MB {
        let megabytes = bytes as f64 / MB;
        if megabytes.fract() == 0. {
            format!("{megabytes:.0} MB")
        } else {
            format!("{megabytes:.1} MB")
        }
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// How an image in the composer is getting to T3.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadState {
    /// Being minted a URL, then sent; `progress` is a fraction, in steps of 5%, as T3 shows it.
    Uploading {
        progress: f32,
    },
    Uploaded,
    /// Why it failed, as T3 says it ("Upload could not start", "Upload rejected (400)").
    Failed {
        reason: SharedString,
    },
}

/// An image in the composer, to send with the next message.
#[derive(Clone, Debug)]
pub struct ComposerImage {
    pub key: u64,
    pub name: String,
    pub image_type: ImageType,
    /// Its bytes, which are both what goes up and the thumbnail.
    pub image: Arc<Image>,
    /// The id T3 minted for it, once it has.
    pub attachment_id: Option<String>,
    pub upload: UploadState,
}

impl ComposerImage {
    pub fn size(&self) -> usize {
        self.image.bytes.len()
    }
}

/// A file other than an image in the composer, to send with the next message.
#[derive(Clone, Debug)]
pub struct ComposerFile {
    pub key: u64,
    pub name: String,
    pub mime_type: String,
    pub bytes: Arc<Vec<u8>>,
    /// It's a big paste.
    pub pasted_text: bool,
    /// The id T3 minted for it, once it has.
    pub attachment_id: Option<String>,
    pub upload: UploadState,
}

impl ComposerFile {
    pub fn size(&self) -> usize {
        self.bytes.len()
    }
}

/// An image the timeline shows, as `assets.createUrl` names it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AssetKey {
    /// An image attached to a message, by its attachment id.
    Attachment {
        attachment_id: String,
        mime_type: String,
    },
    /// The `index`th image in a tool's output, such as a screenshot.
    ToolImage { item_id: String, index: usize },
}

impl AssetKey {
    /// The resource T3 signs a URL for: for an attachment, only its id, as T3's timeline asks
    /// (`session-logic.ts`); for a tool's image, the thread, item, and index.
    fn resource(&self, thread_id: &str) -> AssetResource {
        match self {
            Self::Attachment { attachment_id, .. } => {
                AssetResource::Attachment(AssetResourceAttachment {
                    attachment_id: attachment_id.clone(),
                    disposition: None,
                    file_name: None,
                    mime_type: None,
                    render_intent: None,
                    tag: AssetResourceAttachmentTag::Attachment,
                })
            }
            Self::ToolImage { item_id, index } => {
                AssetResource::ToolOutputImage(AssetResourceToolOutputImage {
                    index: *index as i64,
                    item_id: item_id.clone(),
                    tag: AssetResourceToolOutputImageTag::ToolOutputImage,
                    thread_id: thread_id.to_string(),
                })
            }
        }
    }
}

/// An image of the timeline, loading or loaded.
#[derive(Clone, Debug)]
pub enum AssetState {
    Loading,
    Loaded(Arc<Image>),
    Failed(SharedString),
}

/// `assets.createUrl` needs only `orchestration:read` for attachments and tools' images
/// (`RpcAuthorization.ts` in T3), so dry runs load them too.
pub const ASSET_ACCESS: Access = Access::Read;

/// Signs a URL for an image (`assets.createUrl`), then loads it.
pub fn load_asset(
    backend: &Arc<dyn ThreadBackend>,
    thread_id: &str,
    key: &AssetKey,
) -> impl Future<Output = Result<Arc<Image>>> + use<> {
    let signed = backend.call::<rpc::AssetsCreateUrl>(
        AssetCreateUrlInput {
            resource: key.resource(thread_id),
        },
        ASSET_ACCESS,
    );
    let backend = backend.clone();
    let hint = match key {
        AssetKey::Attachment { mime_type, .. } => ImageFormat::from_mime_type(mime_type),
        AssetKey::ToolImage { .. } => None,
    };
    async move {
        let signed = signed.await.context("signing the image's URL")?;
        let bytes = backend.fetch_asset(&signed.relative_url).await?;
        let format = sniff_format(&bytes)
            .or(hint)
            .ok_or_else(|| anyhow!("T3 sent an image mustr can't read"))?;
        Ok(Arc::new(Image::from_bytes(format, bytes)))
    }
}

/// A thread view's attachments: the composer's images and files, and the timeline's images as
/// they load.
#[derive(Default)]
pub struct Images {
    pub(crate) composer: Vec<ComposerImage>,
    /// The composer's other files, after its images in the strip and in the message.
    pub(crate) files: Vec<ComposerFile>,
    next_key: u64,
    /// Images and files being read or shrunk before they're added.
    pub(crate) preparing: usize,
    /// Why the last attachments couldn't be added, until more are.
    pub(crate) intake_error: Option<SharedString>,
    /// What happened to the last big paste, until the next intake.
    pub(crate) intake_note: Option<SharedString>,
    /// A send waiting on retried uploads, as T3's Send waits for them.
    pub(crate) send_when_uploaded: Option<Delivery>,
    pub(crate) assets: HashMap<AssetKey, AssetState>,
    uploads: HashMap<u64, Task<()>>,
    loads: HashMap<AssetKey, Task<()>>,
    _intake: Vec<Task<()>>,
}

impl Images {
    /// An attachment's upload and the id T3 minted for it, by key: an image's or a file's.
    fn upload_mut(&mut self, key: u64) -> Option<(&mut UploadState, &mut Option<String>)> {
        if let Some(image) = self.composer.iter_mut().find(|image| image.key == key) {
            return Some((&mut image.upload, &mut image.attachment_id));
        }
        self.files
            .iter_mut()
            .find(|file| file.key == key)
            .map(|file| (&mut file.upload, &mut file.attachment_id))
    }

    fn uploads(&self) -> impl Iterator<Item = &UploadState> {
        self.composer
            .iter()
            .map(|image| &image.upload)
            .chain(self.files.iter().map(|file| &file.upload))
    }

    /// Whether the composer holds an image or a file.
    pub fn has_attachments(&self) -> bool {
        !self.composer.is_empty() || !self.files.is_empty()
    }

    /// Whether the composer has no attachment, not even one on its way.
    pub fn is_empty(&self) -> bool {
        self.composer.is_empty() && self.files.is_empty() && self.preparing == 0
    }

    /// How many attachments the composer holds or is preparing, against T3's limit.
    pub fn count(&self) -> usize {
        self.composer.len() + self.files.len() + self.preparing
    }

    /// Whether any attachment is still going up.
    pub fn uploading(&self) -> bool {
        self.preparing > 0
            || self
                .uploads()
                .any(|upload| matches!(upload, UploadState::Uploading { .. }))
    }

    pub fn any_failed(&self) -> bool {
        self.uploads()
            .any(|upload| matches!(upload, UploadState::Failed { .. }))
    }

    /// Why Send waits, if it does: "Attachment still uploading", as T3 says it.
    pub fn send_blocked_reason(&self) -> Option<&'static str> {
        let uploading = self.preparing
            + self
                .uploads()
                .filter(|upload| matches!(upload, UploadState::Uploading { .. }))
                .count();
        match uploading {
            0 => None,
            1 => Some("Attachment still uploading"),
            _ => Some("Attachments still uploading"),
        }
    }

    /// The uploaded images, then files, as the next message names them.
    pub fn attachments(&self) -> Vec<ChatAttachment> {
        let images = self
            .composer
            .iter()
            .filter(|image| image.upload == UploadState::Uploaded)
            .filter_map(|image| {
                Some(commands::image_attachment(
                    image.attachment_id.as_deref()?,
                    &image.name,
                    image.image_type.mime_type(),
                    image.size(),
                ))
            });
        let files = self
            .files
            .iter()
            .filter(|file| file.upload == UploadState::Uploaded)
            .filter_map(|file| {
                Some(commands::file_attachment(
                    file.attachment_id.as_deref()?,
                    &file.name,
                    &file.mime_type,
                    file.size(),
                    file.pasted_text,
                ))
            });
        images.chain(files).collect()
    }

    /// The keys of the uploaded attachments, which a message takes.
    pub(crate) fn uploaded_keys(&self) -> Vec<u64> {
        self.composer
            .iter()
            .filter(|image| image.upload == UploadState::Uploaded)
            .map(|image| image.key)
            .chain(
                self.files
                    .iter()
                    .filter(|file| file.upload == UploadState::Uploaded)
                    .map(|file| file.key),
            )
            .collect()
    }

    /// Forgets the attachments a message took, without deleting them from T3.
    pub(crate) fn clear_sent(&mut self, keys: &[u64]) {
        self.composer.retain(|image| !keys.contains(&image.key));
        self.files.retain(|file| !keys.contains(&file.key));
        for key in keys {
            self.uploads.remove(key);
        }
        self.intake_error = None;
        self.intake_note = None;
        self.send_when_uploaded = None;
    }
}

/// The context records of a message's images and files, as T3 0.1.3 sends them besides its
/// attachments (`attachmentContextRecord` in T3's `lib/composerContextRecords.ts`): each names
/// its attachment by id, never its bytes.
pub(crate) fn attachment_records(attachments: &[ChatAttachment]) -> Vec<serde_json::Value> {
    attachments
        .iter()
        .filter_map(|attachment| {
            let (kind, id, name, mime_type, size_bytes) = match attachment {
                ChatAttachment::Image(image) => (
                    "image",
                    &image.id,
                    &image.name,
                    &image.mime_type,
                    image.size_bytes,
                ),
                ChatAttachment::File(file) => (
                    "file",
                    &file.id,
                    &file.name,
                    &file.mime_type,
                    file.size_bytes,
                ),
                ChatAttachment::Unrecognized(_) => return None,
            };
            Some(serde_json::json!({
                "version": 1,
                "contextId": crate::mentions::kind_scoped_context_id(kind, id),
                "label": crate::mentions::sanitize_label(name, kind),
                "kind": kind,
                "attachmentId": id,
                "name": name,
                "mimeType": mime_type,
                "sizeBytes": size_bytes,
            }))
        })
        .collect()
}

impl ThreadView {
    /// A message's `context` with the records of its images and files after the others, as
    /// T3's `buildMessageContext` orders them, on servers that take records inline
    /// (`inlineMessageContext`); older ones get the attachments alone, as before.
    pub(crate) fn with_attachment_records(
        &self,
        context: Option<t3_api::types::OrchestrationMessageContext>,
        attachments: &[ChatAttachment],
        cx: &App,
    ) -> Option<t3_api::types::OrchestrationMessageContext> {
        let records = attachment_records(attachments);
        if records.is_empty() || !self.server_takes_inline_context(cx) {
            return context;
        }
        match context {
            Some(mut context) => {
                context.records.extend(records);
                Some(context)
            }
            None => crate::mentions::message_context_of(records),
        }
    }

    /// The composer's images, for tests.
    pub fn composer_images(&self) -> &[ComposerImage] {
        &self.images.composer
    }

    /// The composer's other files, for tests.
    pub fn composer_files(&self) -> &[ComposerFile] {
        &self.images.files
    }

    /// Puts the composer's uploaded images and files on a new thread's first message,
    /// returning which they were, to forget once it starts.
    pub(crate) fn attach_composer_images(
        &self,
        input: &mut OrchestrationV2ThreadLaunchInput,
        cx: &App,
    ) -> Vec<u64> {
        let Some(message) = input.initial_message.as_mut() else {
            return Vec::new();
        };
        message.attachments = self.images.attachments();
        message.context =
            self.with_attachment_records(message.context.take(), &message.attachments, cx);
        self.images.uploaded_keys()
    }

    /// Adds images from the clipboard, as pasted.
    pub(crate) fn add_clipboard_images(
        &mut self,
        images: Vec<Image>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prepare = cx.background_spawn(async move {
            images
                .into_iter()
                .map(|image| {
                    NewImage::prepare(None, Some(image.format), image.bytes)
                        .map(NewAttachment::Image)
                })
                .collect::<Vec<_>>()
        });
        self.prepare_attachments(prepare, window, cx);
    }

    /// Adds files, as dropped, picked or copied: images by their names, which mustr shrinks to
    /// fit, and any other file as it is.
    pub(crate) fn add_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prepare = cx.background_spawn(async move {
            paths
                .iter()
                .map(|path| {
                    if has_image_extension(path) {
                        NewImage::from_path(path).map(NewAttachment::Image)
                    } else {
                        NewFile::from_path(path).map(NewAttachment::File)
                    }
                })
                .collect::<Vec<_>>()
        });
        self.prepare_attachments(prepare, window, cx);
    }

    /// Takes a paste of text as a file when it's big, as T3's composer folds it
    /// (`foldPastedText`): returns whether it did. It stays text when the server takes no files
    /// or the composer is full, and when it's too big even for a file, it says so instead.
    pub(crate) fn fold_big_paste(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !is_big_paste(text) {
            return false;
        }
        let Some(limit) = self.file_uploads().limit() else {
            return false;
        };
        if self.images.count() >= MAX_ATTACHMENTS {
            return false;
        }
        if text.len() > limit {
            self.images.intake_note = None;
            self.images.intake_error = Some(
                "Pasted text is too large to attach. Reduce the clipboard contents or save a \
                 smaller excerpt as a file."
                    .into(),
            );
            cx.notify();
            return true;
        }
        let name = next_pasted_text_name(self.images.files.iter().map(|file| file.name.as_str()));
        let file = NewFile::pasted_text(name, text).map(NewAttachment::File);
        let size = format_size(text.len());
        let added = self.add_attachments(vec![file], window, cx);
        if let Some(name) = added.first() {
            self.images.intake_note = Some(
                format!(
                    "Large paste attached as {name} ({size}). Use {} to keep a large paste inline.",
                    if cfg!(target_os = "macos") {
                        "⌘⇧V"
                    } else {
                        "Ctrl+Shift+V"
                    }
                )
                .into(),
            );
        }
        true
    }

    /// Whether T3 takes files, as far as the composer has read its config.
    pub(crate) fn file_uploads(&self) -> FileUploads {
        self.composer_state.menu.file_uploads()
    }

    fn prepare_attachments(
        &mut self,
        prepare: Task<Vec<Result<NewAttachment, String>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.images.preparing += 1;
        self.images.intake_error = None;
        self.images.intake_note = None;
        cx.notify();
        self.images._intake.retain(|task| !task.is_ready());
        self.images
            ._intake
            .push(cx.spawn_in(window, async move |this, cx| {
                let prepared = prepare.await;
                this.update_in(cx, |this, window, cx| {
                    this.images.preparing -= 1;
                    this.add_attachments(prepared, window, cx);
                })
                .ok();
            }));
    }

    /// Adds prepared images, as [`Self::add_attachments`] does.
    #[cfg(test)]
    pub(crate) fn add_images(
        &mut self,
        images: Vec<Result<NewImage, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let attachments = images
            .into_iter()
            .map(|image| image.map(NewAttachment::Image))
            .collect();
        self.add_attachments(attachments, window, cx);
    }

    /// Adds prepared images and files and starts their uploads, refusing any past T3's limits
    /// (`addComposerAttachments` in T3). Returns the names of the ones it added.
    pub(crate) fn add_attachments(
        &mut self,
        attachments: Vec<Result<NewAttachment, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        let mut added = Vec::new();
        for attachment in attachments {
            let attachment = match attachment {
                Ok(attachment) => attachment,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            if self.images.composer.len() + self.images.files.len() >= MAX_ATTACHMENTS {
                errors.push(format!(
                    "You can attach up to {MAX_ATTACHMENTS} files per message."
                ));
                continue;
            }
            self.images.next_key += 1;
            let key = self.images.next_key;
            match attachment {
                NewAttachment::Image(image) => {
                    let total = self
                        .images
                        .composer
                        .iter()
                        .map(ComposerImage::size)
                        .sum::<usize>()
                        + image.bytes.len();
                    if total > MAX_TOTAL_IMAGE_BYTES {
                        errors.push(format!(
                            "{} doesn't fit: a message's images can be up to {} in all.",
                            image.name,
                            format_size(MAX_TOTAL_IMAGE_BYTES)
                        ));
                        continue;
                    }
                    added.push(image.name.clone());
                    self.images.composer.push(ComposerImage {
                        key,
                        name: image.name,
                        image_type: image.image_type,
                        image: Arc::new(Image::from_bytes(image.image_type.format(), image.bytes)),
                        attachment_id: None,
                        upload: UploadState::Uploading { progress: 0. },
                    });
                }
                NewAttachment::File(file) => {
                    let Some(limit) = self.file_uploads().limit() else {
                        errors.push("This server does not support file attachments.".into());
                        continue;
                    };
                    if file.bytes.len() > limit {
                        errors.push(file_too_large_message(&file.name, limit));
                        continue;
                    }
                    added.push(file.name.clone());
                    self.images.files.push(ComposerFile {
                        key,
                        name: file.name,
                        mime_type: file.mime_type,
                        bytes: Arc::new(file.bytes),
                        pasted_text: file.pasted_text,
                        attachment_id: None,
                        upload: UploadState::Uploading { progress: 0. },
                    });
                }
            }
            self.start_upload(key, window, cx);
        }
        errors.dedup();
        self.images.intake_error = (!errors.is_empty()).then(|| errors.join(" ").into());
        cx.notify();
        added
    }

    /// Mints an upload URL for an image or file (`attachments.createUploadUrl`), then sends its
    /// bytes.
    fn start_upload(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let (input, mime_type, bytes) =
            if let Some(image) = self.images.composer.iter().find(|image| image.key == key) {
                let bytes = image.image.bytes.clone();
                // T3 leaves `type` out for images; only files say `"file"`.
                let input = AttachmentCreateUploadUrlInput::Variant0 {
                    mime_type: image.image_type.wire(),
                    name: image.name.clone(),
                    size_bytes: bytes.len() as i64,
                    type_: None,
                };
                (input, image.image_type.mime_type().to_string(), bytes)
            } else if let Some(file) = self.images.files.iter().find(|file| file.key == key) {
                let input = AttachmentCreateUploadUrlInput::Variant1 {
                    mime_type: file.mime_type.clone(),
                    name: file.name.clone(),
                    size_bytes: file.size() as i64,
                    type_: AttachmentCreateUploadUrlInputVariant1Type::File,
                };
                (input, file.mime_type.clone(), file.bytes.as_ref().clone())
            } else {
                return;
            };
        if let Some((upload, attachment_id)) = self.images.upload_mut(key) {
            *upload = UploadState::Uploading { progress: 0. };
            *attachment_id = None;
        }
        let backend = self.backend.clone();
        let mint = backend.call::<rpc::AttachmentsCreateUploadUrl>(input, Access::Operate);
        let total = bytes.len() as u64;
        let task = cx.spawn_in(window, async move |this, cx| {
            let minted = match mint.await {
                Ok(minted) => minted,
                Err(error) => {
                    log::warn!("couldn't mint an upload URL: {error:#}");
                    let reason = match backend.blocked_reason(Access::Operate) {
                        Some(_) => "This connection cannot upload attachments.".to_string(),
                        None => format!("Upload could not start: {error:#}"),
                    };
                    this.update_in(cx, |this, window, cx| {
                        this.finish_upload(key, Err(reason), window, cx)
                    })
                    .ok();
                    return;
                }
            };
            let attachment_id = minted.attachment_id.clone();
            if this
                .update(cx, |this, _| {
                    if let Some((_, minted_id)) = this.images.upload_mut(key) {
                        *minted_id = Some(attachment_id);
                    }
                })
                .is_err()
            {
                return;
            }
            let (progress_tx, mut progress_rx) = mpsc::unbounded::<u64>();
            let mut upload = backend
                .upload(AttachmentUpload {
                    relative_url: minted.relative_url,
                    mime_type,
                    bytes,
                    on_progress: Some(Arc::new(move |sent| {
                        progress_tx.unbounded_send(sent).ok();
                    })),
                })
                .fuse();
            let result = loop {
                futures::select_biased! {
                    result = upload => break result,
                    sent = progress_rx.next() => {
                        let Some(sent) = sent else { continue };
                        let progress = sent as f32 / total.max(1) as f32;
                        this.update(cx, |this, cx| this.set_upload_progress(key, progress, cx))
                            .ok();
                    }
                }
            };
            this.update_in(cx, |this, window, cx| {
                let result = result.map_err(|error| {
                    log::warn!("couldn't upload an attachment: {error:#}");
                    format!("Upload failed: {error:#}")
                });
                this.finish_upload(key, result, window, cx)
            })
            .ok();
        });
        self.images.uploads.insert(key, task);
    }

    fn set_upload_progress(&mut self, key: u64, progress: f32, cx: &mut Context<Self>) {
        let Some((upload, _)) = self.images.upload_mut(key) else {
            return;
        };
        // In steps of 5%, as T3 shows it, so a big upload doesn't redraw for every chunk.
        let stepped = (progress.clamp(0., 1.) * 20.).floor() / 20.;
        if let UploadState::Uploading { progress } = upload
            && *progress != stepped
        {
            *progress = stepped;
            cx.notify();
        }
    }

    fn finish_upload(
        &mut self,
        key: u64,
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.images.uploads.remove(&key);
        let Some((upload, _)) = self.images.upload_mut(key) else {
            return;
        };
        *upload = match result {
            Ok(()) => UploadState::Uploaded,
            Err(reason) => UploadState::Failed {
                reason: reason.into(),
            },
        };
        cx.notify();
        let Some(delivery) = self.images.send_when_uploaded else {
            return;
        };
        if self.images.any_failed() {
            self.images.send_when_uploaded = None;
            self.error = Some("Retry or remove failed uploads before sending.".into());
        } else if !self.images.uploading() {
            self.images.send_when_uploaded = None;
            self.send(delivery, window, cx);
        }
    }

    /// Retries every failed upload, then sends once they're all up; for Send with failed
    /// uploads, as T3's Send retries them.
    pub(crate) fn retry_failed_then_send(
        &mut self,
        delivery: Delivery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let failed = self
            .images
            .composer
            .iter()
            .map(|image| (image.key, &image.upload))
            .chain(
                self.images
                    .files
                    .iter()
                    .map(|file| (file.key, &file.upload)),
            )
            .filter(|(_, upload)| matches!(upload, UploadState::Failed { .. }))
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        self.images.send_when_uploaded = Some(delivery);
        self.error = None;
        for key in failed {
            self.retry_image(key, window, cx);
        }
        cx.notify();
    }

    /// Uploads a failed image again with a fresh URL, deleting what T3 minted for the failed
    /// try (`retryAttachmentUpload` in T3).
    pub(crate) fn retry_image(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((upload, attachment_id)) = self.images.upload_mut(key) else {
            return;
        };
        if !matches!(upload, UploadState::Failed { .. }) {
            return;
        }
        if let Some(attachment_id) = attachment_id.take() {
            self.delete_attachment(attachment_id, cx);
        }
        self.start_upload(key, window, cx);
        cx.notify();
    }

    /// Takes an image out of the composer, stopping its upload and deleting whatever T3
    /// minted for it (`releaseAttachmentUpload` in T3).
    pub(crate) fn remove_image(&mut self, key: u64, cx: &mut Context<Self>) {
        let attachment_id = if let Some(index) = self
            .images
            .composer
            .iter()
            .position(|image| image.key == key)
        {
            self.images.composer.remove(index).attachment_id
        } else if let Some(index) = self.images.files.iter().position(|file| file.key == key) {
            self.images.files.remove(index).attachment_id
        } else {
            return;
        };
        self.images.uploads.remove(&key);
        if let Some(attachment_id) = attachment_id {
            self.delete_attachment(attachment_id, cx);
        }
        if self.images.composer.is_empty() && self.images.files.is_empty() {
            self.images.send_when_uploaded = None;
        }
        cx.notify();
    }

    /// Deletes an unsent attachment (`attachments.delete`), without waiting. It's skipped when
    /// writes are refused, as T3 skips it without `orchestration:operate`.
    fn delete_attachment(&self, attachment_id: String, cx: &mut Context<Self>) {
        if self.backend.blocked_reason(Access::Operate).is_some() {
            return;
        }
        let delete = self.backend.call::<rpc::AttachmentsDelete>(
            AttachmentDeleteInput {
                attachment_id: attachment_id.clone(),
            },
            Access::Operate,
        );
        cx.background_spawn(async move {
            if let Err(error) = delete.await {
                log::warn!("couldn't delete attachment {attachment_id}: {error:#}");
            }
        })
        .detach();
    }

    /// Asks for files to attach with the system's file picker (on Linux, its desktop portal).
    pub(crate) fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        self.images._intake.retain(|task| !task.is_ready());
        self.images
            ._intake
            .push(cx.spawn_in(window, async move |this, cx| {
                let paths = match paths.await {
                    Ok(Ok(Some(paths))) => paths,
                    Ok(Ok(None)) | Err(_) => return,
                    Ok(Err(error)) => {
                        log::warn!("couldn't open the file picker: {error:#}");
                        this.update(cx, |this, cx| {
                            this.images.intake_error =
                                Some(format!("Couldn't open the file picker: {error:#}").into());
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                };
                this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx))
                    .ok();
            }));
    }

    /// The state of an image of the timeline, starting to load it the first time it's asked
    /// for.
    pub(crate) fn asset(&mut self, key: &AssetKey, cx: &mut Context<Self>) -> AssetState {
        if let Some(state) = self.images.assets.get(key) {
            return state.clone();
        }
        let Some(thread_id) = self.thread_id.clone() else {
            return AssetState::Failed("The thread hasn't started".into());
        };
        self.images.assets.insert(key.clone(), AssetState::Loading);
        let load = load_asset(&self.backend, &thread_id, key);
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                let state = match result {
                    Ok(image) => AssetState::Loaded(image),
                    Err(error) => {
                        log::warn!("couldn't load an image of the thread: {error:#}");
                        AssetState::Failed(format!("{error:#}").into())
                    }
                };
                this.images.loads.remove(&task_key);
                this.images.assets.insert(task_key, state);
                cx.notify();
            })
            .ok();
        });
        self.images.loads.insert(key.clone(), task);
        AssetState::Loading
    }

    /// Shows an image full size over the window.
    pub(crate) fn open_image_preview(
        &mut self,
        name: SharedString,
        image: Arc<Image>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |_, cx| ImagePreview::new(name, image, cx))
        });
    }
}

/// An image shown full size over the window, from a thumbnail. Clicking anywhere or pressing
/// Escape closes it, as T3's `chat/ExpandedImageDialog.tsx` does.
pub struct ImagePreview {
    focus_handle: FocusHandle,
    name: SharedString,
    image: Arc<Image>,
}

impl ImagePreview {
    pub fn new(name: SharedString, image: Arc<Image>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            name,
            image,
        }
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    fn dismiss(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ImagePreview {}

impl Focusable for ImagePreview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ImagePreview {
    /// It covers the whole window itself, rather than sitting in the modal layer's column.
    fn render_bare(&self) -> bool {
        true
    }
}

impl Render for ImagePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = palette(cx);
        let viewport = window.viewport_size();
        v_flex()
            .id("thread-image-preview")
            .key_context("ImagePreview")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dismiss))
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .bg(gpui::black().opacity(0.72))
            .cursor_pointer()
            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .child(
                img(self.image.clone())
                    .max_w(viewport.width * 0.92)
                    .max_h(viewport.height * 0.84)
                    .rounded(scaled(tokens::CORNER_RADIUS)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .rounded(scaled(tokens::CONTROL_RADIUS))
                    .border(tokens::BORDER_WIDTH)
                    .border_color(palette.border.hsla())
                    .bg(palette.surface.hsla())
                    .child(Label::new(self.name.clone()).size(LabelSize::Small))
                    .child(
                        Label::new("Esc or click to close")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }
}
