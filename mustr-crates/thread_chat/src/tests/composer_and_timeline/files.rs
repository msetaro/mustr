//! The composer's files and big pastes, against the recording backend: every payload is
//! asserted as T3 gets it.

use super::*;
use crate::{
    attachments::{
        FileUploads, MAX_FILE_BYTES, NewAttachment, NewFile, file_too_large_message, is_big_paste,
        mime_type_for, next_pasted_text_name,
    },
    composer::PasteAsText,
};
use pretty_assertions::assert_eq;

/// What `attachments.createUploadUrl` gets for a file.
fn file_upload_url_call(name: &str, mime_type: &str, size: usize) -> RawCall {
    RawCall {
        tag: "attachments.createUploadUrl".into(),
        payload: json!({ "type": "file", "name": name, "mimeType": mime_type, "sizeBytes": size }),
        access: Access::Operate,
    }
}

/// A log long enough to go in as a file: 36,000 bytes.
fn big_log() -> String {
    "error: boom\n".repeat(3000)
}

// Files and big pastes.

#[test]
fn pastes_and_files_follow_t3s_rules() {
    assert!(!is_big_paste(""));
    assert!(!is_big_paste(&"a".repeat(32 * 1024 - 1)));
    assert!(is_big_paste(&"a".repeat(32 * 1024)));
    // Bytes count, so text heavy in multi-byte characters folds sooner.
    assert!(is_big_paste(&"é".repeat(16 * 1024)));

    assert_eq!(next_pasted_text_name([]), "pasted-text.txt");
    assert_eq!(
        next_pasted_text_name(["Pasted-Text.txt", "pasted-text-2.txt", "notes.md"]),
        "pasted-text-3.txt"
    );

    assert_eq!(
        file_too_large_message("dump.bin", MAX_FILE_BYTES),
        "'dump.bin' exceeds the 50 MB attachment limit."
    );
    assert_eq!(
        file_too_large_message("a", 1536),
        "'a' exceeds the 1536 bytes attachment limit."
    );
    assert_eq!(
        NewFile::pasted_text("pasted-text.txt".into(), "").unwrap_err(),
        "'pasted-text.txt' is empty or could not be read."
    );
    assert_eq!(mime_type_for("trace.LOG"), "text/plain");
    assert_eq!(mime_type_for("report.pdf"), "application/pdf");
    assert_eq!(mime_type_for("Makefile"), "application/octet-stream");

    // T3's config: uploads on with a file limit (cut to T3's own), images only, or not read.
    let config = |capabilities: Value| json!({ "environment": { "capabilities": capabilities } });
    assert_eq!(
        FileUploads::from_config(&config(json!({
            "attachmentUploads": true, "fileAttachments": { "maxUploadBytes": 1024 }
        }))),
        FileUploads::UpTo(1024)
    );
    assert_eq!(
        FileUploads::from_config(&config(json!({
            "attachmentUploads": true,
            "fileAttachments": { "maxUploadBytes": 200 * 1024 * 1024 }
        }))),
        FileUploads::UpTo(MAX_FILE_BYTES)
    );
    assert_eq!(
        FileUploads::from_config(&config(json!({ "attachmentUploads": true }))),
        FileUploads::Unsupported
    );
    assert_eq!(FileUploads::from_config(&json!({})), FileUploads::Unknown);
    assert_eq!(FileUploads::Unknown.limit(), Some(MAX_FILE_BYTES));
    assert_eq!(FileUploads::Unsupported.limit(), None);
}

#[gpui::test]
async fn big_pastes_go_in_as_files(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    backend
        .raw_responses
        .lock()
        .extend([minted("attachment-1"), minted("attachment-2")]);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    cx.simulate_resize(gpui::size(gpui::px(1000.), gpui::px(900.)));

    type_message(&view, "Why does this fail?", cx);
    let log = big_log();
    cx.write_to_clipboard(ClipboardItem::new_string(log.clone()));
    cx.dispatch_action(editor::actions::Paste);
    cx.run_until_parked();

    // The text stays out of the composer: it goes up as `pasted-text.txt`, as T3 folds it.
    assert_eq!(composer_text(&view, cx), "Why does this fail?");
    assert_eq!(
        *backend.raw_calls.lock(),
        [file_upload_url_call(
            "pasted-text.txt",
            "text/plain;charset=utf-8",
            log.len()
        )]
    );
    assert_eq!(
        *backend.uploads.lock(),
        [Upload {
            relative_url: "/api/attachments/upload/attachment-1.signature".into(),
            mime_type: "text/plain;charset=utf-8".into(),
            bytes: log.as_bytes().to_vec(),
        }]
    );
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.images.intake_note.as_deref(),
            Some(
                "Large paste attached as pasted-text.txt (36 KB). Use Ctrl+Shift+V to keep a \
                 large paste inline."
            )
        );
    });
    assert!(cx.debug_bounds("thread-file-1").is_some());
    assert!(cx.debug_bounds("thread-intake-note").is_some());

    // Another big paste gets the next name.
    cx.dispatch_action(editor::actions::Paste);
    cx.run_until_parked();
    assert_eq!(
        backend.raw_calls.lock()[1].payload["name"],
        json!("pasted-text-2.txt")
    );

    cx.dispatch_action(SendMessage);
    cx.run_until_parked();
    let file = |id: &str, name: &str| {
        json!({
            "type": "file",
            "id": id,
            "name": name,
            "mimeType": "text/plain;charset=utf-8",
            "sizeBytes": log.len(),
            "source": { "_tag": "pasted-text" }
        })
    };
    assert_eq!(
        without_ids(backend.commands()[0].clone()),
        json!({
            "type": "message.dispatch",
            "createdBy": "user",
            "creationSource": "web",
            "threadId": THREAD,
            "text": "Why does this fail?",
            "attachments": [
                file("attachment-1", "pasted-text.txt"),
                file("attachment-2", "pasted-text-2.txt")
            ],
            "deliveryIntent": "auto",
            "dispatchMode": { "type": "start_immediately" }
        })
    );
    view.read_with(cx, |view, _| {
        assert!(view.composer_files().is_empty(), "the message took them");
        assert_eq!(view.images.intake_note, None);
    });

    // `mod-shift-v` keeps it as text, as T3's does.
    cx.dispatch_action(PasteAsText);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), log);
    assert_eq!(backend.uploads.lock().len(), 2);
}

#[gpui::test]
async fn big_pastes_stay_text_when_t3_takes_no_files(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend.clone(), Some(THREAD), cx);
    view.update(cx, |view, _| {
        view.composer_state
            .menu
            .set_file_uploads(FileUploads::Unsupported)
    });
    let composer = view.read_with(cx, |view, _| view.composer().clone());
    cx.focus(&composer);
    cx.write_to_clipboard(ClipboardItem::new_string(big_log()));
    cx.dispatch_action(editor::actions::Paste);
    cx.run_until_parked();
    assert_eq!(composer_text(&view, cx), big_log());
    assert!(backend.raw_calls.lock().is_empty());

    // And a dropped file says why it can't go.
    view.update_in(cx, |view, window, cx| {
        view.add_attachments(
            vec![NewFile::pasted_text("notes.txt".into(), "notes").map(NewAttachment::File)],
            window,
            cx,
        );
    });
    view.read_with(cx, |view, _| {
        assert!(view.composer_files().is_empty());
        assert_eq!(
            view.images.intake_error.as_deref(),
            Some("This server does not support file attachments.")
        );
    });

    // A server with a smaller limit refuses files over it, in T3's words.
    view.update_in(cx, |view, window, cx| {
        view.composer_state
            .menu
            .set_file_uploads(FileUploads::UpTo(4));
        view.add_attachments(
            vec![NewFile::pasted_text("notes.txt".into(), "notes").map(NewAttachment::File)],
            window,
            cx,
        );
    });
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.images.intake_error.as_deref(),
            Some("'notes.txt' exceeds the 4 bytes attachment limit.")
        );
    });
}

#[gpui::test]
async fn a_message_takes_at_most_a_hundred_attachments(cx: &mut TestAppContext) {
    let backend = FakeBackend::with_thread(finished_thread(), 10);
    let (view, cx) = open_view(backend, Some(THREAD), cx);
    let files = |count: usize| {
        (0..count)
            .map(|index| {
                NewFile::pasted_text(format!("part-{index}.txt"), "x").map(NewAttachment::File)
            })
            .collect::<Vec<_>>()
    };
    view.update_in(cx, |view, window, cx| {
        view.add_attachments(files(99), window, cx);
        view.add_images(
            vec![NewImage::prepare(Some("last.png".into()), None, png(2, 2))],
            window,
            cx,
        );
        view.add_attachments(files(1), window, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |view, _| {
        assert_eq!(
            view.composer_files().len() + view.composer_images().len(),
            100
        );
        assert_eq!(
            view.images.intake_error.as_deref(),
            Some("You can attach up to 100 files per message.")
        );
    });
}
