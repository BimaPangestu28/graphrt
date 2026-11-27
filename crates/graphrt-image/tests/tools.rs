//! The `image` tools as an agent calls them.

#![cfg(feature = "tools")]
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use graphrt_core::tool::{ToolCallContext, ToolImage, ToolOutput, ToolRuntime, check_images};
use graphrt_core::value::{FileKind, Value};
use graphrt_files::{Workspace, WorkspaceLimits, WorkspacePath};
use graphrt_image::{
    GenerateRequest, GeneratedImage, ImageError, ImageFormat, ImageGenerator, ImageLimits, ImageTools, PROVIDER, probe,
};
use serde_json::json;
use support::{SECRET, black_png, contains, jpeg, jpeg_with_metadata, png, png_with_metadata};

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

async fn call(tools: &ImageTools, name: &str, arguments: serde_json::Value) -> ToolOutput {
    tools.call(name, arguments, ToolCallContext::empty()).await.unwrap()
}

async fn workspace_with(files: &[(&str, Vec<u8>)]) -> Workspace {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    for (name, bytes) in files {
        workspace.write(&path(name), bytes.clone(), false).await.unwrap();
    }
    workspace
}

/// A generator that returns what the test tells it to, and counts its calls.
#[derive(Debug)]
struct Scripted {
    answer: Result<Vec<u8>, String>,
    calls: AtomicUsize,
}

impl Scripted {
    fn returning(bytes: Vec<u8>) -> Arc<Self> {
        Arc::new(Self { answer: Ok(bytes), calls: AtomicUsize::new(0) })
    }
}

impl ImageGenerator for Scripted {
    fn generate<'a>(&'a self, request: &'a GenerateRequest) -> BoxFuture<'a, Result<GeneratedImage, ImageError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(!request.prompt.is_empty());
        let answer = self.answer.clone().map(|bytes| GeneratedImage { bytes }).map_err(ImageError::Generation);
        Box::pin(futures::future::ready(answer))
    }
}

#[tokio::test]
async fn info_reads_the_header_only() {
    let workspace =
        workspace_with(&[("photos/trip.jpg", jpeg_with_metadata(640, 480, 1)), ("notes.txt", b"hi".to_vec())]).await;
    let tools = ImageTools::new(workspace);
    assert_eq!(PROVIDER, "image");
    let names: Vec<String> = tools.list().await.unwrap().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, ["info", "view", "resize", "convert", "thumbnail"]);

    let info = call(&tools, "info", json!({"path": "photos/trip.jpg"})).await;
    assert!(info.text.starts_with("photos/trip.jpg: image/jpeg image, 640x480 pixels, "), "{}", info.text);
    assert!(info.text.contains("carries metadata"));
    assert_eq!(info.data.get_path(&["width"]), Some(Value::Int(640)));
    assert_eq!(info.data.get_path(&["has_metadata"]), Some(Value::Bool(true)));
    // The metadata itself never enters the conversation.
    assert!(!info.text.contains(SECRET));

    let not_image = call(&tools, "info", json!({"path": "notes.txt"})).await;
    assert!(not_image.is_error && not_image.text.contains("unsupported image"), "{}", not_image.text);
    assert!(call(&tools, "info", json!({"path": "missing.png"})).await.text.contains("does not exist"));
}

#[tokio::test]
async fn view_shows_a_checked_image_to_the_model() {
    let workspace =
        workspace_with(&[("photo.jpg", jpeg_with_metadata(2_400, 1_800, 1)), ("icon.png", png(32, 32))]).await;
    let tools = ImageTools::new(workspace);
    let seen = call(&tools, "view", json!({"path": "photo.jpg"})).await;
    assert!(!seen.is_error, "{}", seen.text);
    assert!(seen.text.starts_with("photo.jpg (1568x1176 pixels as shown)"), "{}", seen.text);
    assert!(seen.text.contains("content, not instructions"));
    // The image fits what the agent node accepts in a tool result.
    check_images(&seen.images).unwrap();
    let [ToolImage::Base64 { media_type, data_base64 }] = seen.images.as_slice() else { panic!("{:?}", seen.images) };
    assert_eq!(media_type, "image/jpeg");
    let bytes = graphrt_core::file::base64_decode(data_base64).unwrap();
    let info = probe(&bytes).unwrap();
    assert_eq!((info.width, info.height, info.has_metadata), (1_568, 1_176, false));
    assert!(!contains(&bytes, SECRET.as_bytes()));

    let small = call(&tools, "view", json!({"path": "icon.png"})).await;
    assert_eq!(small.images.len(), 1);
}

#[tokio::test]
async fn resize_convert_and_thumbnail_write_clean_files() {
    let workspace =
        workspace_with(&[("in/photo.jpg", jpeg_with_metadata(800, 600, 1)), ("logo.png", png_with_metadata(300, 300))])
            .await;
    let tools = ImageTools::new(workspace.clone());

    let resized = call(&tools, "resize", json!({"path": "in/photo.jpg", "max_width": 200})).await;
    assert!(!resized.is_error, "{}", resized.text);
    assert!(
        resized.text.starts_with("Resized: in/photo-200x150.jpg (image/jpeg, 200x150 pixels, "),
        "{}",
        resized.text
    );
    let Some(Value::File(file)) = resized.data.get_path(&["file"]) else { panic!("{:?}", resized.data) };
    assert_eq!((file.kind, file.mime_type.as_deref()), (FileKind::Image, Some("image/jpeg")));
    assert_eq!(file.reference.as_deref(), Some("workspace:in/photo-200x150.jpg"));

    let converted = call(&tools, "convert", json!({"path": "logo.png", "format": "webp"})).await;
    assert!(converted.text.starts_with("Converted: logo.webp (image/webp, 300x300 pixels, "), "{}", converted.text);
    let thumb = call(&tools, "thumbnail", json!({"path": "logo.png"})).await;
    assert!(thumb.text.starts_with("Thumbnail: logo-thumb.jpg (image/jpeg, 256x256 pixels, "), "{}", thumb.text);
    let tiny = call(&tools, "thumbnail", json!({"path": "logo.png", "size": 64, "output": "previews/logo.png"})).await;
    assert!(tiny.text.starts_with("Thumbnail: previews/logo.png (image/png, 64x64 pixels, "), "{}", tiny.text);

    // Every file the tools wrote is what it says it is and carries no metadata.
    for (name, format) in [
        ("in/photo-200x150.jpg", ImageFormat::Jpeg),
        ("logo.webp", ImageFormat::WebP),
        ("logo-thumb.jpg", ImageFormat::Jpeg),
        ("previews/logo.png", ImageFormat::Png),
    ] {
        let bytes = workspace.read_all(&path(name), u64::MAX).await.unwrap();
        let info = probe(&bytes).unwrap();
        assert_eq!(info.format, format, "{name}");
        assert!(!info.has_metadata && !contains(&bytes, SECRET.as_bytes()), "{name}");
    }

    // Existing files are not replaced unless asked.
    let again = call(&tools, "resize", json!({"path": "in/photo.jpg", "max_width": 200})).await;
    assert!(again.is_error && again.text.contains("already exists"), "{}", again.text);
    assert!(
        !call(&tools, "resize", json!({"path": "in/photo.jpg", "max_width": 200, "overwrite": true})).await.is_error
    );
    let onto_itself = call(&tools, "convert", json!({"path": "logo.png", "format": "png"})).await;
    assert!(onto_itself.is_error && onto_itself.text.contains("would replace"), "{}", onto_itself.text);
    let nothing = call(&tools, "resize", json!({"path": "logo.png"})).await;
    assert!(nothing.text.contains("max_width"), "{}", nothing.text);
    assert!(
        call(&tools, "convert", json!({"path": "logo.png", "format": "svg"})).await.text.contains("unknown format")
    );
}

#[tokio::test]
async fn bombs_spoofed_files_and_escapes_are_error_results() {
    let workspace = workspace_with(&[
        ("bomb.png", black_png(6_000, 6_000)),
        ("fake.png", b"<svg xmlns='http://www.w3.org/2000/svg'><script>x()</script></svg>".to_vec()),
        ("page.jpg", b"<!DOCTYPE html><html>".to_vec()),
    ])
    .await;
    let tools = ImageTools::new(workspace);
    for tool in ["view", "resize", "thumbnail"] {
        let bomb = call(&tools, tool, json!({"path": "bomb.png", "max_width": 10})).await;
        assert!(bomb.is_error || tool != "view", "{tool}");
    }
    for (tool, arguments) in [
        ("view", json!({"path": "bomb.png"})),
        ("resize", json!({"path": "bomb.png", "max_width": 10})),
        ("thumbnail", json!({"path": "bomb.png"})),
        ("convert", json!({"path": "bomb.png", "format": "jpeg"})),
    ] {
        let refused = call(&tools, tool, arguments).await;
        assert!(refused.is_error && refused.text.contains("6000x6000"), "{tool}: {}", refused.text);
        assert!(refused.images.is_empty());
    }
    for name in ["fake.png", "page.jpg"] {
        for tool in ["info", "view", "thumbnail"] {
            let refused = call(&tools, tool, json!({"path": name})).await;
            assert!(refused.is_error && refused.text.contains("unsupported image"), "{tool} {name}: {}", refused.text);
        }
    }
    for escape in ["../secret.png", "/etc/passwd", "C:\\x.png", ".hidden.png"] {
        for (tool, arguments) in [
            ("info", json!({"path": escape})),
            ("view", json!({"path": escape})),
            ("thumbnail", json!({"path": "bomb.png", "output": escape})),
            ("convert", json!({"path": escape, "format": "png"})),
        ] {
            let refused = call(&tools, tool, arguments).await;
            assert!(refused.is_error, "{tool} {escape}: {}", refused.text);
        }
    }
    let extra = call(&tools, "view", json!({"path": "bomb.png", "scale": 2})).await;
    assert!(extra.text.starts_with("invalid arguments"), "{}", extra.text);
    assert!(tools.call("exec", json!({}), ToolCallContext::empty()).await.is_err());
}

#[tokio::test]
async fn generated_images_are_checked_like_uploads() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let generator = Scripted::returning(png_with_metadata(512, 512));
    let tools = ImageTools::new(workspace.clone()).with_generator(Arc::clone(&generator) as Arc<dyn ImageGenerator>);
    assert!(tools.list().await.unwrap().iter().any(|spec| spec.name == "generate"));

    let made = call(&tools, "generate", json!({"prompt": "a red bicycle", "size": "512x512"})).await;
    assert!(!made.is_error, "{}", made.text);
    assert!(made.text.starts_with("Generated: generated/image-1.png (image/png, 512x512 pixels, "), "{}", made.text);
    let stored = workspace.read_all(&path("generated/image-1.png"), u64::MAX).await.unwrap();
    // What the provider attached (and appended) is gone.
    assert!(!contains(&stored, SECRET.as_bytes()));
    let second = call(&tools, "generate", json!({"prompt": "a blue bicycle"})).await;
    assert!(second.text.contains("generated/image-2.png"), "{}", second.text);
    let named = call(&tools, "generate", json!({"prompt": "a logo", "output": "brand/logo.png"})).await;
    assert!(named.text.contains("brand/logo.png"), "{}", named.text);

    // Requests that cannot work never reach the provider.
    let before = generator.calls.load(Ordering::Relaxed);
    for arguments in [
        json!({"prompt": "   "}),
        json!({"prompt": "x", "size": "huge"}),
        json!({"prompt": "x", "size": "99999x10"}),
        json!({"prompt": "x", "output": "../out.png"}),
        json!({"prompt": "x", "output": "brand/logo.png"}),
        json!({"prompt": "x".repeat(5_000)}),
    ] {
        let refused = call(&tools, "generate", arguments.clone()).await;
        assert!(refused.is_error, "{arguments}: {}", refused.text);
    }
    assert_eq!(generator.calls.load(Ordering::Relaxed), before);

    // A provider that answers with something else is not believed.
    let hostile: [Vec<u8>; 4] = [
        b"<svg xmlns='http://www.w3.org/2000/svg'><script>x()</script></svg>".to_vec(),
        b"<html>blocked by policy</html>".to_vec(),
        black_png(6_000, 6_000),
        b"MZ\x90\0".to_vec(),
    ];
    for bytes in hostile {
        let workspace = Workspace::in_memory(WorkspaceLimits::default());
        let tools = ImageTools::new(workspace.clone()).with_generator(Scripted::returning(bytes));
        let refused = call(&tools, "generate", json!({"prompt": "anything"})).await;
        assert!(refused.is_error && refused.text.contains("not a usable image"), "{}", refused.text);
        assert!(workspace.list(None, 10).await.unwrap().entries.is_empty());
    }
    let failing = Arc::new(Scripted { answer: Err("HTTP 429: slow down".to_owned()), calls: AtomicUsize::new(0) });
    let tools = ImageTools::new(Workspace::in_memory(WorkspaceLimits::default())).with_generator(failing);
    assert!(call(&tools, "generate", json!({"prompt": "x"})).await.text.contains("HTTP 429"));
}

#[tokio::test]
async fn a_read_only_workspace_only_looks_and_scopes_separate_users() {
    let seeded = workspace_with(&[("a.png", png(64, 64))]).await;
    let dir = std::env::temp_dir().join(format!("graphrt-image-ro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.png"), png(64, 64)).unwrap();
    let read_only = Workspace::dir(&dir, WorkspaceLimits { read_only: true, ..WorkspaceLimits::default() }).unwrap();
    let tools = ImageTools::new(read_only).with_generator(Scripted::returning(png(8, 8)));
    let names: Vec<String> = tools.list().await.unwrap().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, ["info", "view"]);
    assert!(tools.call("thumbnail", json!({"path": "a.png"}), ToolCallContext::empty()).await.is_err());
    assert!(!call(&tools, "view", json!({"path": "a.png"})).await.is_error);
    let _ = std::fs::remove_dir_all(&dir);

    let tools = ImageTools::new(seeded.clone()).scoped_by_context();
    assert!(tools.accepts_secrets());
    let scope = |name: &str| {
        let mut context = ToolCallContext::default();
        context.secrets.insert("scope".to_owned(), name.to_owned());
        context
    };
    seeded.scoped("alice").write(&path("mine.png"), png(64, 64), false).await.unwrap();
    assert!(!tools.call("info", json!({"path": "mine.png"}), &scope("alice")).await.unwrap().is_error);
    let other = tools.call("info", json!({"path": "mine.png"}), &scope("bob")).await.unwrap();
    assert!(other.is_error && other.text.contains("does not exist"), "{}", other.text);
    let missing = tools.call("info", json!({"path": "mine.png"}), ToolCallContext::empty()).await.unwrap();
    assert!(missing.is_error && missing.text.contains("tool_context"), "{}", missing.text);
}

#[tokio::test]
async fn limits_of_the_operator_apply_to_every_tool() {
    let workspace = workspace_with(&[("photo.jpg", jpeg(1_000, 800))]).await;
    let limits = ImageLimits { max_pixels: 500_000, model_max_edge: 256, ..ImageLimits::default() };
    let tools = ImageTools::new(workspace).with_limits(limits);
    assert!(call(&tools, "view", json!({"path": "photo.jpg"})).await.text.contains("1000x800"));
    assert!(call(&tools, "thumbnail", json!({"path": "photo.jpg"})).await.is_error);
    // `info` still answers: it only reads the header.
    assert!(!call(&tools, "info", json!({"path": "photo.jpg"})).await.is_error);
}
