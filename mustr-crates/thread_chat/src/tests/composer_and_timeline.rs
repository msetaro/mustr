//! The composer's images (paste, drop, and the picker, with uploads, retries, and removal),
//! images in the timeline, plans' Implement and Refine, and Resume, against the recording
//! backend. Every payload is asserted as T3 gets it.

use super::*;
use crate::{
    attachments::{
        AssetKey, ImagePreview, ImageType, MAX_EDGE, MAX_IMAGE_BYTES, NewImage, UploadState,
        fit_to_max_edge, format_size,
    },
    commands::PLAN_IMPLEMENTATION_PREFIX,
};
use gpui::{AppContext as _, ClipboardItem, ExternalPaths, FileDropEvent, ImageFormat};
use pretty_assertions::assert_eq;
use std::path::PathBuf;

/// A real PNG of `width` × `height`.
pub(super) fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x * 7) as u8, (y * 5) as u8, 90, 255])
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

/// What `attachments.createUploadUrl` answers.
fn minted(attachment_id: &str) -> Value {
    json!({
        "attachmentId": attachment_id,
        "relativeUrl": format!("/api/attachments/upload/{attachment_id}.signature"),
        "expiresAt": 1_800_000_000_000_i64
    })
}

fn upload_url_call(name: &str, mime_type: &str, size: usize) -> RawCall {
    RawCall {
        tag: "attachments.createUploadUrl".into(),
        payload: json!({ "name": name, "mimeType": mime_type, "sizeBytes": size }),
        access: Access::Operate,
    }
}

fn delete_call(attachment_id: &str) -> RawCall {
    RawCall {
        tag: "attachments.delete".into(),
        payload: json!({ "attachmentId": attachment_id }),
        access: Access::Operate,
    }
}

fn without_ids(mut command: Value) -> Value {
    let object = command.as_object_mut().unwrap();
    object.remove("commandId");
    object.remove("messageId");
    command
}

fn upload_states(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> Vec<UploadState> {
    view.read_with(cx, |view, _| {
        view.composer_images()
            .iter()
            .map(|image| image.upload.clone())
            .collect()
    })
}

/// A scratch directory for files to drop or pick.
fn scratch_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("mustr-thread-chat-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// Preparing images.

#[test]
fn images_are_checked_and_named_as_t3_does() {
    let small = png(40, 30);
    let image = NewImage::prepare(None, Some(ImageFormat::Png), small.clone()).unwrap();
    assert_eq!(
        (image.name.as_str(), image.image_type, image.bytes),
        ("image.png", ImageType::Png, small)
    );

    // The bytes decide the type, whatever the clipboard said.
    let gif = b"GIF89a\x01\x00\x01\x00tiny".to_vec();
    let image = NewImage::prepare(Some("wave.gif".into()), Some(ImageFormat::Png), gif).unwrap();
    assert_eq!(image.image_type, ImageType::Gif);

    assert_eq!(
        NewImage::prepare(Some("notes.txt".into()), None, b"hello".to_vec()).unwrap_err(),
        "notes.txt isn't a supported image type. Attach GIF, JPEG, PNG, or WebP images."
    );
    assert_eq!(
        NewImage::prepare(Some("empty.png".into()), None, Vec::new()).unwrap_err(),
        "empty.png is empty."
    );
    // A GIF over the limit can't be shrunk, as in T3.
    let mut big_gif = b"GIF89a".to_vec();
    big_gif.resize(MAX_IMAGE_BYTES + 1, 0);
    assert_eq!(
        NewImage::prepare(Some("big.gif".into()), None, big_gif).unwrap_err(),
        "big.gif is 10.0 MB; images can be up to 10 MB."
    );
}

#[test]
fn big_images_are_scaled_down_and_bmps_become_pngs() {
    assert_eq!(fit_to_max_edge(1999, 800), None);
    assert_eq!(fit_to_max_edge(4000, 3000), Some((MAX_EDGE, 1500)));
    assert_eq!(fit_to_max_edge(1000, 2500), Some((800, MAX_EDGE)));

    let image = NewImage::prepare(Some("wide.png".into()), None, png(2400, 1200)).unwrap();
    let decoded = image::load_from_memory(&image.bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (2000, 1000));
    assert_eq!(
        (image.name.as_str(), image.image_type),
        ("wide.png", ImageType::Png)
    );

    let mut bmp = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
        .write_to(&mut bmp, image::ImageFormat::Bmp)
        .unwrap();
    let image = NewImage::prepare(Some("Screenshot.bmp".into()), None, bmp.into_inner()).unwrap();
    assert_eq!(
        (image.name.as_str(), image.image_type),
        ("Screenshot.png", ImageType::Png)
    );
    assert!(image.bytes.starts_with(b"\x89PNG"));

    assert_eq!(format_size(MAX_IMAGE_BYTES), "10 MB");
    assert_eq!(format_size(1536), "2 KB");
}

// The composer.

#[gpui::test]
async fn pasted_images_upload_then_go_with_the_message(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().push(minted("attachment-1"));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));

    let screenshot = png(40, 30);
    type_message(&view, "Why is this button red?", cx);
    cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
        ImageFormat::Png,
        screenshot.clone(),
    )));
    cx.dispatch_action(editor::actions::Paste);
    cx.run_until_parked();

    // The image isn't pasted as text; it's minted a URL and uploaded at once.
    assert_eq!(composer_text(&view, cx), "Why is this button red?");
    assert_eq!(
        *backend.raw_calls.lock(),
        [upload_url_call("image.png", "image/png", screenshot.len())]
    );
    assert_eq!(
        *backend.uploads.lock(),
        [Upload {
            relative_url: "/api/attachments/upload/attachment-1.signature".into(),
            mime_type: "image/png".into(),
            bytes: screenshot.clone(),
        }]
    );
    assert_eq!(upload_states(&view, cx), [UploadState::Uploaded]);
    assert!(cx.debug_bounds("thread-image-1").is_some());

    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(
        without_ids(commands[0].clone()),
        json!({
            "type": "message.dispatch",
            "createdBy": "user",
            "creationSource": "web",
            "threadId": THREAD,
            "text": "Why is this button red?",
            "attachments": [{
                "type": "image",
                "id": "attachment-1",
                "name": "image.png",
                "mimeType": "image/png",
                "sizeBytes": screenshot.len()
            }],
            "deliveryIntent": "auto",
            "dispatchMode": { "type": "start_immediately" }
        })
    );
    // The message took the image: it leaves the composer, and isn't deleted from T3.
    assert!(view.read_with(cx, |view, _| view.composer_images().is_empty()));
    assert_eq!(composer_text(&view, cx), "");
    assert_eq!(backend.raw_calls.lock().len(), 1);

    // Text pastes as text.
    cx.write_to_clipboard(ClipboardItem::new_string("plain words".into()));
    cx.dispatch_action(editor::actions::Paste);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "plain words");
    assert_eq!(backend.uploads.lock().len(), 1);
}

#[gpui::test]
async fn an_image_alone_is_a_message(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().push(minted("attachment-1"));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update_in(cx, |view, window, cx| {
        view.add_images(
            vec![NewImage::prepare(Some("only.png".into()), None, png(4, 4))],
            window,
            cx,
        )
    });
    cx.run_until_parked();
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    cx.focus(&composer);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(commands[0]["text"], "");
    assert_eq!(commands[0]["attachments"][0]["id"], "attachment-1");
}

#[gpui::test]
async fn new_threads_start_with_their_images(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open_view(backend.clone(), None, cx);
    // After the new-thread form has loaded T3's settings, which the fake answers in order.
    backend.raw_responses.lock().push(minted("attachment-1"));
    let bytes = png(16, 9);
    type_message(&view, "Make the header match this", cx);
    cx.write_to_clipboard(ClipboardItem::new_image(&gpui::Image::from_bytes(
        ImageFormat::Png,
        bytes.clone(),
    )));
    cx.dispatch_action(editor::actions::Paste);
    cx.run_until_parked();
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();

    let launches = backend.launches.lock().clone();
    assert_eq!(launches.len(), 1);
    let launch = serde_json::to_value(&launches[0]).unwrap();
    assert_eq!(
        launch["initialMessage"]["attachments"],
        json!([{
            "type": "image",
            "id": "attachment-1",
            "name": "image.png",
            "mimeType": "image/png",
            "sizeBytes": bytes.len()
        }])
    );
    assert!(view.read_with(cx, |view, _| view.composer_images().is_empty()));
}

#[gpui::test]
async fn dropped_files_attach_their_images(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().push(minted("attachment-1"));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();

    let dir = scratch_dir("drop");
    let image = dir.join("diagram.png");
    let notes = dir.join("notes.txt");
    std::fs::write(&image, png(20, 10)).unwrap();
    std::fs::write(&notes, "not an image").unwrap();

    let composer = cx.debug_bounds("thread-composer").unwrap();
    cx.simulate_event(FileDropEvent::Entered {
        position: composer.center(),
        paths: ExternalPaths(vec![image, notes].into()),
    });
    cx.simulate_event(FileDropEvent::Submit {
        position: composer.center(),
    });
    cx.run_until_parked();

    assert_eq!(
        *backend.raw_calls.lock(),
        [upload_url_call(
            "diagram.png",
            "image/png",
            png(20, 10).len()
        )]
    );
    assert_eq!(upload_states(&view, cx), [UploadState::Uploaded]);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.images.intake_error.as_deref(),
            Some("notes.txt isn't a supported image type. Attach GIF, JPEG, PNG, or WebP images.")
        );
    });
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
async fn the_picker_attaches_the_chosen_images(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().push(minted("attachment-1"));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();

    let dir = scratch_dir("pick");
    let photo = dir.join("photo.jpg");
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&image::DynamicImage::ImageRgb8(image::RgbImage::new(12, 8)))
        .unwrap();
    std::fs::write(&photo, &jpeg).unwrap();

    let attach = cx.debug_bounds("thread-attach").unwrap();
    cx.simulate_click(attach.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.files && options.multiple && !options.directories);
        Some(vec![photo.clone()])
    });
    cx.run_until_parked();

    assert_eq!(
        *backend.raw_calls.lock(),
        [upload_url_call("photo.jpg", "image/jpeg", jpeg.len())]
    );
    assert_eq!(upload_states(&view, cx), [UploadState::Uploaded]);
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
async fn failed_uploads_retry_and_removed_images_are_deleted(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().extend([
        minted("attachment-1"),
        Value::Null,
        minted("attachment-2"),
        Value::Null,
    ]);
    *backend.failing_uploads.lock() = 1;
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    let bytes = png(8, 8);
    view.update_in(cx, |view, window, cx| {
        view.add_images(
            vec![NewImage::prepare(
                Some("chart.png".into()),
                None,
                bytes.clone(),
            )],
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(
        upload_states(&view, cx),
        [UploadState::Failed {
            reason: "Upload failed: T3 refused the attachment (400 Bad Request)".into()
        }]
    );

    // Retry deletes the failed mint and starts over with a fresh URL.
    let retry = cx.debug_bounds("thread-image-retry-1").unwrap();
    cx.simulate_click(retry.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(upload_states(&view, cx), [UploadState::Uploaded]);

    // Removing it deletes what T3 minted, since no message took it.
    let remove = cx.debug_bounds("thread-image-remove-1").unwrap();
    cx.simulate_click(remove.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.composer_images().is_empty()));
    assert_eq!(
        *backend.raw_calls.lock(),
        [
            upload_url_call("chart.png", "image/png", bytes.len()),
            delete_call("attachment-1"),
            upload_url_call("chart.png", "image/png", bytes.len()),
            delete_call("attachment-2"),
        ]
    );
    assert_eq!(
        backend
            .uploads
            .lock()
            .iter()
            .map(|upload| upload.relative_url.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/attachments/upload/attachment-1.signature",
            "/api/attachments/upload/attachment-2.signature"
        ]
    );
    assert!(backend.commands().is_empty());
}

#[gpui::test]
async fn send_retries_failed_uploads_first(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend.raw_responses.lock().extend([
        minted("attachment-1"),
        Value::Null,
        minted("attachment-2"),
    ]);
    *backend.failing_uploads.lock() = 1;
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update_in(cx, |view, window, cx| {
        view.add_images(
            vec![NewImage::prepare(Some("chart.png".into()), None, png(8, 8))],
            window,
            cx,
        )
    });
    cx.run_until_parked();
    type_message(&view, "See the chart", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["attachments"][0]["id"], "attachment-2");
    assert_eq!(
        backend.raw_calls.lock()[1],
        delete_call("attachment-1"),
        "the failed mint is deleted before the retry"
    );
}

#[gpui::test]
async fn dry_runs_refuse_uploads_but_load_images(cx: &mut TestAppContext) {
    let recording = FakeBackend::with_thread(thread_with_images(), 10);
    recording
        .raw_responses
        .lock()
        .push(json!({ "relativeUrl": "/api/assets/a.sig/failing-test.png", "expiresAt": 1 }));
    let backend = Arc::new(ReadOnlyBackend::new(
        recording.clone(),
        "MUSTR_DRY_RUN_WRITES is set",
    ));
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    view.update_in(cx, |view, window, cx| {
        view.add_images(vec![NewImage::prepare(None, None, png(4, 4))], window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        upload_states(&view, cx),
        [UploadState::Failed {
            reason: "This connection cannot upload attachments.".into()
        }]
    );
    assert!(recording.uploads.lock().is_empty());
    // Nothing was minted, so removing it deletes nothing.
    let remove = cx.debug_bounds("thread-image-remove-1").unwrap();
    cx.simulate_click(remove.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    // Reads go through: the message's image loads.
    assert_eq!(
        recording
            .raw_calls
            .lock()
            .iter()
            .map(|call| call.tag.as_str())
            .collect::<Vec<_>>(),
        ["assets.createUrl"]
    );
    assert_eq!(recording.fetched_assets.lock().len(), 1);
}

// Images in the timeline.

fn thread_with_images() -> Projection {
    let mut projection = finished_thread();
    projection.items[0]["attachments"] = json!([{
        "type": "image",
        "id": "attachment-9",
        "name": "failing-test.png",
        "mimeType": "image/png",
        "sizeBytes": 2048
    }]);
    projection.items.insert(
        4,
        item(
            "dynamic_tool",
            "i4b",
            4,
            Some("run-1"),
            "completed",
            json!({
                "toolName": "browser_screenshot",
                "input": { "url": "http://localhost:3000" },
                "output": { "content": [
                    { "type": "text", "text": "Took a screenshot" },
                    { "type": "image", "mimeType": "image/png", "data": "iVBORw0KGgo=" },
                    { "type": "image", "mimeType": "image/svg+xml", "data": "PHN2Zz4=" }
                ] },
                "outputOmitted": false
            }),
        ),
    );
    projection
}

fn asset_call(resource: Value) -> RawCall {
    RawCall {
        tag: "assets.createUrl".into(),
        payload: json!({ "resource": resource }),
        access: Access::Read,
    }
}

#[test]
fn tools_images_are_counted_and_their_data_left_out_of_the_text() {
    let state = ThreadState::new(thread_with_images().decode(), 10);
    let tool = state.items().iter().find(|item| item.id == "i4b").unwrap();
    let ItemKind::Tool { images, output, .. } = &tool.kind else {
        panic!("not a tool: {tool:?}");
    };
    // An SVG isn't served as an image, as in T3.
    assert_eq!(*images, 1);
    let output = output.as_deref().unwrap();
    assert!(output.contains("Took a screenshot"), "{output}");
    assert!(!output.contains("iVBORw0KGgo"), "{output}");

    let ItemKind::UserMessage { images, .. } = &state.items()[0].kind else {
        panic!("not a message");
    };
    assert_eq!(images[0].attachment_id, "attachment-9");
}

#[gpui::test]
async fn messages_and_tools_show_their_images(cx: &mut TestAppContext) {
    init_test(cx);
    let backend = FakeBackend::with_thread(thread_with_images(), 10);
    backend.raw_responses.lock().extend([
        json!({ "relativeUrl": "/api/assets/a.sig/failing-test.png", "expiresAt": 1 }),
        json!({ "relativeUrl": "/api/assets/b.sig/image-1.png", "expiresAt": 1 }),
    ]);
    cx.update(|cx| crate::set_backend(backend.clone(), cx));
    let fs = project::FakeFs::new(cx.executor());
    let project = project::Project::test(fs, [], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(project.clone(), window, cx)
    });
    cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(1000.)));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        ThreadView::open(THREAD.into(), "Images".into(), workspace, window, cx)
    });
    cx.run_until_parked();

    // The header's banners follow T3's server config too (`usage.rs`).
    let asset_calls = || {
        backend
            .raw_calls
            .lock()
            .iter()
            .filter(|call| call.tag == "assets.createUrl")
            .cloned()
            .collect::<Vec<_>>()
    };
    // The message's image loads by its attachment id alone, as T3's timeline asks for it.
    assert_eq!(
        asset_calls(),
        [asset_call(
            json!({ "_tag": "attachment", "attachmentId": "attachment-9" })
        )]
    );
    assert_eq!(
        *backend.fetched_assets.lock(),
        ["/api/assets/a.sig/failing-test.png"]
    );
    let thumbnail = cx
        .debug_bounds("thread-message-image-i1-0")
        .expect("the message's thumbnail");
    cx.simulate_click(thumbnail.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let preview = workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ImagePreview>(cx)
    });
    assert_eq!(
        preview.map(|preview| preview.read_with(cx, |preview, _| preview.name().clone())),
        Some("failing-test.png".into())
    );
    // Escape closes it.
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<ImagePreview>(cx).is_none()
    }));

    // A tool's screenshots load when its output is shown.
    let work = cx.debug_bounds("thread-work-header-i2").unwrap();
    cx.simulate_click(work.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    // Expanding the run doesn't expand its first call.
    let view = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<ThreadView>())
            .unwrap()
    });
    view.read_with(cx, |view, _| {
        assert!(view.expanded.contains("work:i2"));
        assert!(!view.expanded.contains("i2"));
    });
    let tool = cx.debug_bounds("thread-work-item-i4b").unwrap();
    cx.simulate_click(tool.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        asset_calls()[1],
        asset_call(json!({
            "_tag": "tool-output-image",
            "threadId": THREAD,
            "itemId": "i4b",
            "index": 0
        }))
    );
    assert!(cx.debug_bounds("thread-tool-image-i4b-0").is_some());
    assert!(cx.debug_bounds("thread-tool-image-i4b-1").is_none());

    // Each image loads once.
    cx.simulate_resize(gpui::size(gpui::px(1100.), gpui::px(1000.)));
    cx.run_until_parked();
    assert_eq!(asset_calls().len(), 2);
    let _ = AssetKey::ToolImage {
        item_id: "i4b".into(),
        index: 0,
    };
}

// Plans.

const PLAN_MARKDOWN: &str =
    "# Retry webhooks with backoff\n\n1. Wrap `post()` in `withRetry`.\n2. Add tests.\n";

/// A thread in plan mode whose finished run proposed a plan.
fn planned_thread(plan_status: &str, interaction_mode: &str) -> Value {
    let mut projection = finished_thread();
    projection.items.push(item(
        "proposed_plan",
        "i6",
        6,
        Some("run-1"),
        "completed",
        json!({ "planId": "plan-1", "markdown": PLAN_MARKDOWN, "streaming": false }),
    ));
    let mut json = projection.json();
    json["thread"]["interactionMode"] = json!(interaction_mode);
    json["plans"] = json!([{
        "kind": "proposed_plan",
        "id": "plan-1",
        "threadId": THREAD,
        "runId": "run-1",
        "nodeId": "node-1",
        "status": plan_status,
        "markdown": PLAN_MARKDOWN
    }]);
    json
}

fn backend_with(projection: Value) -> Arc<FakeBackend> {
    let backend = FakeBackend::default();
    *backend.snapshot.lock() = Some(json!({ "snapshotSequence": 10, "projection": projection }));
    Arc::new(backend)
}

#[test]
fn only_an_active_plan_of_a_settled_run_in_plan_mode_is_offered() {
    let state =
        |projection: Value| ThreadState::new(serde_json::from_value(projection).unwrap(), 10);
    let plan = state(planned_thread("active", "plan"))
        .actionable_plan()
        .unwrap();
    assert_eq!(
        (plan.plan_id.as_str(), plan.item_id.as_str()),
        ("plan-1", "i6")
    );
    assert!(
        state(planned_thread("completed", "plan"))
            .actionable_plan()
            .is_none()
    );
    assert!(
        state(planned_thread("superseded", "plan"))
            .actionable_plan()
            .is_none()
    );
    assert!(
        state(planned_thread("active", "default"))
            .actionable_plan()
            .is_none()
    );
    // Not while a run is working.
    let mut running = planned_thread("active", "plan");
    running["runs"]
        .as_array_mut()
        .unwrap()
        .push(run("run-2", 2, "running", "m2"));
    assert!(state(running).actionable_plan().is_none());
}

fn plan_message(thread_id: &str, title_seed: &str) -> Value {
    json!({
        "type": "message.dispatch",
        "createdBy": "user",
        "creationSource": "web",
        "threadId": thread_id,
        "text": format!("{PLAN_IMPLEMENTATION_PREFIX}{}", PLAN_MARKDOWN.trim()),
        "attachments": [],
        "modelSelection": model_selection(),
        "sourcePlanRef": { "threadId": THREAD, "planId": "plan-1" },
        "titleSeed": title_seed,
        "deliveryIntent": "auto",
        "dispatchMode": { "type": "start_immediately" }
    })
}

#[gpui::test]
async fn implement_leaves_plan_mode_then_sends_the_plan(cx: &mut TestAppContext) {
    let backend = backend_with(planned_thread("active", "plan"));
    let (_view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();
    // The plan card and the empty composer both offer it.
    assert!(cx.debug_bounds("thread-implement").is_some());
    let implement = cx.debug_bounds("thread-plan-implement").unwrap();
    cx.simulate_click(implement.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    let commands = backend
        .commands()
        .into_iter()
        .map(without_ids)
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        [
            json!({
                "type": "thread.interaction-mode.set",
                "threadId": THREAD,
                "interactionMode": "default"
            }),
            plan_message(THREAD, "Add retries to the webhook dispatcher"),
        ]
    );
}

#[gpui::test]
async fn implement_in_a_new_thread_creates_it_on_the_same_branch(cx: &mut TestAppContext) {
    let backend = backend_with(planned_thread("active", "plan"));
    let (_view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();
    let implement = cx.debug_bounds("thread-plan-implement-new").unwrap();
    cx.simulate_click(implement.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    let commands = backend.commands();
    assert_eq!(commands.len(), 2);
    let new_thread_id = commands[0]["threadId"].as_str().unwrap().to_string();
    assert_ne!(new_thread_id, THREAD);
    assert!(uuid::Uuid::parse_str(&new_thread_id).is_ok());
    assert_eq!(
        without_ids(commands[0].clone()),
        json!({
            "type": "thread.create",
            "createdBy": "user",
            "creationSource": "web",
            "threadId": new_thread_id,
            "projectId": "project-1",
            "title": "Implement Retry webhooks with backoff",
            "modelSelection": model_selection(),
            "runtimeMode": "approval-required",
            "interactionMode": "default",
            "branch": "t3/retry-backoff",
            "worktreePath": "/tmp/worktrees/retry-backoff"
        })
    );
    assert_eq!(
        without_ids(commands[1].clone()),
        plan_message(&new_thread_id, "Implement Retry webhooks with backoff")
    );
}

#[gpui::test]
async fn a_plan_thread_that_cant_start_is_deleted(cx: &mut TestAppContext) {
    let backend = backend_with(planned_thread("active", "plan"));
    backend
        .refused_commands
        .lock()
        .push("message.dispatch".into());
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update_in(cx, |view, window, cx| view.implement_plan(true, window, cx));
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(
        commands
            .iter()
            .map(|command| command["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["thread.create", "message.dispatch", "thread.delete"]
    );
    assert_eq!(commands[2]["threadId"], commands[0]["threadId"]);
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.error().map(|error| error.to_string()),
            Some(
                "Could not start the implementation thread: T3 refused message.dispatch"
                    .to_string()
            )
        );
    });
}

#[gpui::test]
async fn refine_and_copy_a_plan(cx: &mut TestAppContext) {
    let backend = backend_with(planned_thread("active", "plan"));
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();

    let copy = cx.debug_bounds("thread-plan-copy-i6").unwrap();
    cx.simulate_click(copy.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(format!("{}\n", PLAN_MARKDOWN.trim_end()))
    );

    let refine = cx.debug_bounds("thread-plan-refine").unwrap();
    cx.simulate_click(refine.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    // With text, the composer's button refines: an ordinary message, still in plan mode.
    type_message(&view, "Also cover 429s", cx);
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-implement").is_none());
    assert!(cx.debug_bounds("thread-send").is_some());
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let commands = backend.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["type"], "message.dispatch");
    assert_eq!(commands[0]["text"], "Also cover 429s");
    assert!(commands[0].get("sourcePlanRef").is_none());
}

// Resume.

fn interrupted_thread() -> Projection {
    let mut projection = finished_thread();
    projection.runs = vec![run("run-1", 1, "interrupted", "m1")];
    projection
}

#[gpui::test]
async fn resume_continues_an_interrupted_run(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(interrupted_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-send").is_none());
    let resume = cx.debug_bounds("thread-resume").unwrap();
    cx.simulate_click(resume.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        backend
            .commands()
            .into_iter()
            .map(without_ids)
            .collect::<Vec<_>>(),
        [json!({
            "type": "message.dispatch",
            "createdBy": "user",
            "creationSource": "web",
            "threadId": THREAD,
            "manualContinuationOfRunId": "run-1",
            "text": "Continue where you left off.",
            "attachments": [],
            "modelSelection": model_selection(),
            "dispatchMode": { "type": "start_immediately" }
        })]
    );

    // Only in an empty composer.
    type_message(&view, "Do something else", cx);
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-resume").is_none());
    assert!(cx.debug_bounds("thread-send").is_some());
}

#[gpui::test]
async fn resume_restarts_a_held_queue(cx: &mut TestAppContext) {
    let mut projection = interrupted_thread();
    let mut held = run("run-2", 2, "queued", "m2");
    held["queueHeld"] = json!(true);
    projection.runs.push(held);
    projection
        .messages
        .push(message("m2", "run-2", "Then update the docs"));
    let backend = FakeBackend::with_thread(projection, 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update_in(cx, |view, window, cx| view.resume(window, cx));
    cx.run_until_parked();
    let commands = backend
        .commands()
        .into_iter()
        .map(without_ids)
        .collect::<Vec<_>>();
    assert_eq!(
        commands
            .iter()
            .map(|command| command["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["message.dispatch", "queue.resume"]
    );
    assert_eq!(
        commands[1],
        json!({ "type": "queue.resume", "threadId": THREAD })
    );

    // A held queue alone just restarts.
    let mut projection = finished_thread();
    let mut held = run("run-2", 2, "queued", "m2");
    held["queueHeld"] = json!(true);
    projection.runs.push(held);
    let state = ThreadState::new(projection.decode(), 10);
    assert_eq!(
        state.resumable(),
        Some(crate::model::Resumable {
            continue_run_id: None,
            held_queue: true
        })
    );
}

#[test]
fn only_stopped_or_usage_limited_runs_resume() {
    assert!(
        ThreadState::new(finished_thread().decode(), 1)
            .resumable()
            .is_none()
    );
    assert_eq!(
        ThreadState::new(interrupted_thread().decode(), 1)
            .resumable()
            .and_then(|resumable| resumable.continue_run_id),
        Some("run-1".to_string())
    );

    let limited = |class: &str| {
        let mut projection = finished_thread();
        let mut failed = run("run-1", 1, "failed", "m1");
        failed["rootNodeId"] = json!("node-root");
        projection.runs = vec![failed];
        let mut error = item(
            "error",
            "i9",
            9,
            Some("run-1"),
            "failed",
            json!({
                "failure": {
                    "class": class,
                    "code": null,
                    "message": "You've hit your limit",
                    "retryable": false
                },
                "retry": null
            }),
        );
        error["nodeId"] = json!("node-root");
        projection.items.push(error);
        ThreadState::new(projection.decode(), 1).resumable()
    };
    assert_eq!(
        limited("usage_limit").and_then(|resumable| resumable.continue_run_id),
        Some("run-1".to_string())
    );
    assert!(limited("provider_error").is_none());
}

// Drafts and recalling prompts.

#[gpui::test]
async fn drafts_survive_closing_the_thread(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    type_message(&view, "half-written thought", cx);
    cx.run_until_parked();

    let reopened = view.update_in(cx, |_, window, cx| {
        cx.new(|cx| {
            ThreadView::new(
                backend.clone(),
                None,
                Some(THREAD.to_string()),
                "Again".into(),
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    assert_eq!(composer_text(&reopened, cx), "half-written thought");

    // Sending clears it.
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let fresh = view.update_in(cx, |_, window, cx| {
        cx.new(|cx| {
            ThreadView::new(
                backend.clone(),
                None,
                Some(THREAD.to_string()),
                "Again".into(),
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    assert_eq!(composer_text(&fresh, cx), "");
}

#[gpui::test]
async fn arrow_up_recalls_sent_prompts(cx: &mut TestAppContext) {
    let mut projection = running_thread();
    // A plan's implementation isn't a prompt to recall.
    projection.items.push(user_message(
        "i9",
        9,
        "run-3",
        &format!("{PLAN_IMPLEMENTATION_PREFIX}# Plan"),
    ));
    let backend = FakeBackend::with_thread(projection, 20);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    type_message(&view, "", cx);

    cx.dispatch_action(zed_actions::editor::MoveUp);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "Now handle 429s");
    cx.dispatch_action(zed_actions::editor::MoveUp);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "Add retries with backoff");
    // The oldest stays.
    cx.dispatch_action(zed_actions::editor::MoveUp);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "Add retries with backoff");
    cx.dispatch_action(zed_actions::editor::MoveDown);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "Now handle 429s");
    cx.dispatch_action(zed_actions::editor::MoveDown);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "");

    // Not over text the user typed.
    type_message(&view, "my own words", cx);
    cx.dispatch_action(zed_actions::editor::MoveUp);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "my own words");
}

#[gpui::test]
async fn rows_are_measured_again_when_the_ui_font_size_changes(cx: &mut TestAppContext) {
    let mut projection = finished_thread();
    projection.items.extend((0..30).map(|index| {
        assistant_message(
            &format!("a{index}"),
            6 + index,
            "run-1",
            "Another paragraph of the agent's reply.",
            false,
        )
    }));
    let backend = FakeBackend::with_thread(projection, 10);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(600.)));
    let list = view.read_with(cx, |view, _| view.list_state.clone());
    list.pause_following_tail();
    list.scroll_to(gpui::ListOffset::default());
    cx.run_until_parked();

    // A row below the window is measured for the scrollbar but not drawn, so its height is
    // only right if the list measures it again at the new size.
    let below = (0..list.item_count())
        .find(|row| list.item_is_below_viewport(*row) == Some(true))
        .unwrap();
    let height = |list: &gpui::ListState| list.bounds_for_item(below).unwrap().size.height;
    let before = height(&list);
    cx.update(|window, _| {
        window.set_rem_size(window.rem_size() * 2.);
        window.refresh();
    });
    cx.run_until_parked();
    let after = height(&list);
    assert!(after > before * 1.5, "{before:?} became {after:?}");
}

#[gpui::test]
async fn messages_copy_their_text(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    view.update(cx, |view, cx| {
        view.copy_text(
            "i5".into(),
            "Done. `withRetry` now wraps `post()`.".into(),
            cx,
        )
    });
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("Done. `withRetry` now wraps `post()`.".to_string())
    );
    view.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.copied.as_deref(), Some("i5"))
    });
}

#[gpui::test]
async fn queued_messages_can_be_edited(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(running_thread(), 20);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();
    let edit = cx.debug_bounds("thread-edit-queued-run-3").unwrap();
    cx.simulate_click(edit.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), "Then update the docs");
    // Save replaces Stop, Steer, and Queue while editing.
    assert!(cx.debug_bounds("thread-save-queued").is_some());
    assert!(cx.debug_bounds("thread-queue").is_none());

    type_message(&view, "Then update the docs and the changelog", cx);
    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    assert_eq!(
        backend
            .commands()
            .into_iter()
            .map(without_ids)
            .collect::<Vec<_>>(),
        [json!({
            "type": "queued-run.edit",
            "threadId": THREAD,
            "runId": "run-3",
            "text": "Then update the docs and the changelog"
        })]
    );
    assert_eq!(composer_text(&view, cx), "");
    view.read_with(cx, |view, _| {
        assert_eq!(view.composer_state.editing_queued, None)
    });
}

// Secret requests.

fn thread_asking_for_a_secret() -> Projection {
    let mut projection = finished_thread();
    projection.items.push(item(
        "secret_request",
        "i9",
        9,
        Some("run-1"),
        "waiting",
        json!({
            "label": "a GitHub token",
            "reason": "To open the pull request",
            "placeholder": "ghp_…",
            "secretStatus": "pending"
        }),
    ));
    projection
}

#[gpui::test]
async fn secret_requests_are_answered_with_a_masked_input(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(thread_asking_for_a_secret(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(1200.)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("thread-secret-input-i9").is_some());

    let input = view.read_with(cx, |view, _| {
        view.composer_state
            .secret_inputs
            .get("i9")
            .cloned()
            .unwrap()
    });
    input.update_in(cx, |editor, window, cx| {
        editor.set_text("ghp_example", window, cx)
    });
    let save = cx.debug_bounds("thread-secret-save-i9").unwrap();
    cx.simulate_click(save.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let decline = cx.debug_bounds("thread-secret-decline-i9").unwrap();
    cx.simulate_click(decline.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert_eq!(
        *backend.raw_calls.lock(),
        [
            RawCall {
                tag: "secrets.answerRequest".into(),
                payload: json!({
                    "threadId": THREAD,
                    "turnItemId": "i9",
                    "answer": { "type": "save", "secret": "ghp_example" }
                }),
                access: Access::Operate,
            },
            RawCall {
                tag: "secrets.answerRequest".into(),
                payload: json!({
                    "threadId": THREAD,
                    "turnItemId": "i9",
                    "answer": { "type": "decline" }
                }),
                access: Access::Operate,
            },
        ]
    );
    // The input is cleared once T3 has the secret.
    assert_eq!(input.read_with(cx, |editor, cx| editor.text(cx)), "");
    assert!(backend.commands().is_empty());
}

// Live command output.

#[gpui::test]
async fn running_commands_stream_their_output(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(running_thread(), 20);
    backend.raw_stream_items.lock().extend([
        json!({ "kind": "replace", "text": "Compiling webhooks\n", "truncated": true, "running": true }),
        json!({ "kind": "append", "text": "test result: ok\n", "truncated": false, "running": false }),
    ]);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update(cx, |view, cx| view.toggle_tool("i7".into(), cx));
    cx.run_until_parked();

    assert_eq!(
        *backend.raw_calls.lock(),
        [RawCall {
            tag: "orchestration.subscribeCommandOutput".into(),
            payload: json!({ "threadId": THREAD, "itemId": "i7" }),
            access: Access::Read,
        }]
    );
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_state.live_outputs.get("i7"),
            Some(&crate::composer::LiveOutput {
                text: "Compiling webhooks\ntest result: ok\n".into(),
                truncated: true,
                running: false,
            })
        );
        assert!(view.composer_state.output_streams.is_empty());
    });
    // A running command's output isn't fetched whole while it streams.
    assert!(backend.turn_items.lock().is_empty());
}
