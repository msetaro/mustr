//! Terminals on T3's machine, for threads whose folder isn't on this one: T3's terminal stream
//! shown in Zed's display-only `Terminal`, with keystrokes, pastes and resizes sent back, as T3's
//! terminal drawer does (`components/ThreadTerminalDrawer.tsx`,
//! `packages/client-runtime/src/state/terminal.ts`).
//!
//! - `terminal.attach` opens the shell (or attaches to the one T3 already runs under that id) and
//!   streams a snapshot of its history, then its output, exit and label changes. A dropped
//!   connection attaches again with `resume`, and the new snapshot replaces what's shown.
//! - Input goes out with `terminal.write`, one call at a time, so keystrokes arrive in order;
//!   keys typed while a write is in flight go out together in the next one. The grid's size goes
//!   out with `terminal.resize` whenever it changes.
//! - Zed's terminal encodes keys only for a local PTY, so the view's keys, pastes, `SendText`
//!   bindings, typed text (including an IME's) and the wheel in full-screen programs are caught
//!   on their way to the `TerminalView` and sent to T3 instead. So are clicks, drags, moves and
//!   the wheel for programs that ask for mouse reports (`tmux`'s or `vim`'s mouse mode), encoded
//!   as a local terminal would (`mouse.rs`); Shift keeps the local selection, as in T3's
//!   terminal. An IME's text being composed shows at the cursor, underlined, until it's
//!   committed.
//! - Closing a terminal ends its shell and deletes its history (`terminal.close` with
//!   `deleteHistory`), as T3's app does. A shell that exits on its own is closed the same way,
//!   keeping its history. Terminals T3 still runs for a thread are found again with
//!   `subscribeTerminalMetadata`, so they outlive mustr, as they outlive T3's app.
//! - Ids are T3's (`term-1`, `term-2`, …: `packages/shared/src/terminalLabels.ts`), so mustr and
//!   T3's app share a thread's terminals.

mod keys;
mod mouse;

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, channel::mpsc, channel::oneshot};
use gpui::{
    App, AppContext as _, Bounds, ClipboardEntry, Context, DispatchPhase, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Keystroke,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta,
    ScrollWheelEvent, SharedString, Subscription, Task, TextRun, TouchPhase, UTF16Selection,
    UnderlineStyle, WeakEntity, Window, canvas, fill, point,
};
use project::Project;
use serde_json::{Map, Value};
use settings::Settings as _;
use std::{
    cell::Cell, collections::HashSet, fmt::Debug, ops::Range, rc::Rc, sync::Arc, time::Duration,
};
use t3_client::t3_api::{
    rpc,
    tagged::{TerminalAttachStreamEvent, TerminalMetadataStreamEvent},
    types::{
        SubscribeTerminalMetadataPayload, TerminalAttachInput, TerminalCloseInput,
        TerminalResizeInput, TerminalSessionStatus, TerminalSummary, TerminalWriteInput,
    },
};
use terminal::{Modes, Terminal, TerminalBuilder, terminal_settings::TerminalSettings};
use terminal_view::TerminalView;
use thread_chat::{Access, ThreadBackend, ThreadBackendExt as _};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::Workspace;

/// The most T3 takes in one `terminal.write` is 65,536 UTF-16 units; mustr sends less, in bytes.
const MAX_WRITE_BYTES: usize = 16 * 1024;
/// T3's limits on a terminal's grid (`TerminalColsSchema`, `TerminalRowsSchema`).
const MAX_COLS: i64 = 1000;
const MAX_ROWS: i64 = 500;
/// How long the grid stays at one size before mustr tells T3, so dragging the panel's edge
/// doesn't resize the shell on every frame.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(60);
/// How long mustr waits for T3's list of terminals before starting a new one.
const METADATA_TIMEOUT: Duration = Duration::from_secs(5);

/// Where and how T3 starts a thread's terminal, as T3's app launches one
/// (`components/terminal/useThreadTerminalActions.ts`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Launch {
    pub thread_id: String,
    pub terminal_id: String,
    /// The thread's worktree, or its project's checkout (`projectScriptCwd`).
    pub cwd: String,
    pub worktree_path: Option<String>,
    /// The project's checkout, for `T3CODE_PROJECT_ROOT`.
    pub project_root: String,
}

impl Launch {
    /// What `terminal.attach` gets. The first attach starts the shell if T3 doesn't run one under
    /// this id (`restartIfNotRunning`); later ones, after a dropped connection, only resume it.
    pub(crate) fn attach_input(
        &self,
        resume: bool,
        size: Option<(i64, i64)>,
    ) -> TerminalAttachInput {
        TerminalAttachInput {
            thread_id: self.thread_id.clone(),
            terminal_id: self.terminal_id.clone(),
            cwd: Some(self.cwd.clone()),
            worktree_path: self.worktree_path.clone(),
            env: Some(self.env()),
            cols: size.map(|(cols, _)| cols),
            rows: size.map(|(_, rows)| rows),
            provider_instance_id: None,
            restart_if_not_running: (!resume).then_some(true),
            resume: resume.then_some(true),
        }
    }

    /// `projectScriptRuntimeEnv` in T3's `packages/shared/src/projectScripts.ts`.
    fn env(&self) -> Map<String, Value> {
        let mut env = Map::new();
        env.insert(
            "T3CODE_PROJECT_ROOT".into(),
            Value::String(self.project_root.clone()),
        );
        if let Some(worktree) = &self.worktree_path {
            env.insert(
                "T3CODE_WORKTREE_PATH".into(),
                Value::String(worktree.clone()),
            );
        }
        env
    }
}

/// The number in a T3 terminal id, as `terminalNumber` in T3's `terminalLabels.ts` reads it.
fn terminal_number(terminal_id: &str) -> Option<u32> {
    let rest = terminal_id
        .strip_prefix("terminal-")
        .or_else(|| terminal_id.strip_prefix("term-"))?;
    let (number, suffix) = rest.split_once('-').unwrap_or((rest, ""));
    let uuid = |suffix: &str| {
        let groups = suffix.split('-').map(str::len).collect::<Vec<_>>();
        groups == [8, 4, 4, 4, 12] && suffix.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
    };
    (!number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && (suffix.is_empty() || uuid(suffix)))
    .then(|| number.parse().ok())
    .flatten()
}

/// The first `term-N` that none of `existing` uses (`nextTerminalId`). Without T3's list of the
/// thread's terminals, `unique_suffix` keeps the id from attaching to one mustr can't see.
pub(crate) fn next_terminal_id<'a>(
    existing: impl IntoIterator<Item = &'a str>,
    unique_suffix: Option<&str>,
) -> String {
    let used = existing
        .into_iter()
        .filter_map(terminal_number)
        .collect::<HashSet<_>>();
    let next = (1..).find(|number| !used.contains(number)).unwrap_or(1);
    match unique_suffix {
        Some(suffix) => format!("term-{next}-{suffix}"),
        None => format!("term-{next}"),
    }
}

/// `getTerminalLabel`: `Terminal 2` for `term-2`.
fn default_label(terminal_id: &str) -> String {
    match terminal_number(terminal_id) {
        Some(number) => format!("Terminal {number}"),
        None => terminal_id.to_string(),
    }
}

/// The terminals T3 has for `thread_id`, from the first snapshot of `subscribeTerminalMetadata`
/// (which needs `terminal:read`).
pub(crate) fn thread_terminals(
    backend: Arc<dyn ThreadBackend>,
    thread_id: String,
    cx: &App,
) -> Task<Result<Vec<TerminalSummary>>> {
    let timer = cx.background_executor().timer(METADATA_TIMEOUT);
    cx.background_spawn(async move {
        let mut stream = backend.stream::<rpc::SubscribeTerminalMetadata>(
            SubscribeTerminalMetadataPayload(Map::new()),
            Access::TerminalRead,
        )?;
        let first = futures::select_biased! {
            first = futures::FutureExt::fuse(stream.next()) => first,
            _ = futures::FutureExt::fuse(timer) => {
                return Err(anyhow!("T3 didn't list its terminals in time"));
            }
        };
        match first.context("T3 ended the list of terminals")?? {
            TerminalMetadataStreamEvent::Snapshot(snapshot) => Ok(snapshot
                .terminals
                .into_iter()
                .filter(|terminal| terminal.thread_id == thread_id)
                .collect()),
            other => Err(anyhow!(
                "T3's list of terminals didn't start with a snapshot: {:?}",
                other.tag()
            )),
        }
    })
}

pub(crate) enum RemoteTerminalEvent {
    /// T3's label for it changed, such as the command that runs in it.
    TitleChanged,
    /// Its shell exited, or it was closed elsewhere, such as in T3's app.
    Ended,
}

/// One terminal on T3's machine.
pub struct RemoteTerminal {
    launch: Launch,
    backend: Arc<dyn ThreadBackend>,
    terminal: Entity<Terminal>,
    view: Entity<TerminalView>,
    /// T3's label: the command running in it, or `Terminal N`.
    label: Option<String>,
    /// Whether T3 streams its output now.
    attached: bool,
    /// Whether T3 has sent its first snapshot; later ones replace what's shown.
    opened: bool,
    ended: bool,
    /// The grid size T3 was told, and the one it'll be told after the debounce.
    sent_size: Option<(i64, i64)>,
    resize: Option<Task<()>>,
    input: mpsc::UnboundedSender<String>,
    /// An IME's text being composed, sent when it's committed.
    marked_text: Option<String>,
    /// The wheel's movement in lines that hasn't been sent yet, in full-screen programs.
    scroll_lines: Rc<Cell<f32>>,
    /// The cell the last mouse report was about, so a move within a cell sends nothing.
    last_mouse_cell: Option<mouse::Cell>,
    /// The button whose press went to the program, so its release does too.
    reported_press: Option<MouseButton>,
    _attach: Task<()>,
    _input: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RemoteTerminalEvent> for RemoteTerminal {}

impl RemoteTerminal {
    /// Opens (or attaches to) the terminal `launch` names, and resolves once T3 has sent its
    /// first snapshot. `size` is the grid it'll show in, when the panel knows it.
    pub(crate) fn open(
        launch: Launch,
        size: Option<(i64, i64)>,
        workspace: WeakEntity<Workspace>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(backend) = thread_chat::backend(cx) else {
            return Task::ready(Err(anyhow!("mustr isn't connected to T3")));
        };
        if let Some(reason) = backend.blocked_reason(Access::TerminalOperate) {
            return Task::ready(Err(anyhow!(reason)));
        }
        let settings = TerminalSettings::get_global(cx);
        let builder = TerminalBuilder::new_display_only(
            settings.cursor_shape,
            settings.alternate_scroll,
            settings.max_scroll_history_lines,
            window.window_handle().window_id().as_u64(),
            cx.background_executor(),
            project.read(cx).path_style(cx),
        );
        let terminal = cx.new(|cx| builder.subscribe(cx));
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal.clone(),
                workspace,
                None,
                project.downgrade(),
                window,
                cx,
            )
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let entity = cx.new(|cx| Self::new(launch, size, backend, terminal, view, ready_tx, cx));
        cx.spawn(async move |_| {
            ready_rx
                .await
                .unwrap_or_else(|_| Err(anyhow!("the terminal closed before it opened")))?;
            Ok(entity)
        })
    }

    fn new(
        launch: Launch,
        size: Option<(i64, i64)>,
        backend: Arc<dyn ThreadBackend>,
        terminal: Entity<Terminal>,
        view: Entity<TerminalView>,
        ready: oneshot::Sender<Result<()>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (input_tx, input_rx) = mpsc::unbounded();
        let size = size.map(clamp_size);
        let attach = cx.spawn({
            let backend = backend.clone();
            let launch = launch.clone();
            async move |this, cx| attach(this, backend, launch, size, ready, cx).await
        });
        let input = cx.spawn({
            let backend = backend.clone();
            let launch = launch.clone();
            async move |this, cx| send_input(this, backend, launch, input_rx, cx).await
        });
        let subscriptions = vec![cx.observe(&view, |_, _, cx| cx.notify())];
        Self {
            launch,
            backend,
            terminal,
            view,
            label: None,
            attached: false,
            opened: false,
            ended: false,
            sent_size: size,
            resize: None,
            input: input_tx,
            marked_text: None,
            scroll_lines: Rc::default(),
            last_mouse_cell: None,
            reported_press: None,
            _attach: attach,
            _input: input,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn view(&self) -> &Entity<TerminalView> {
        &self.view
    }

    pub(crate) fn launch(&self) -> &Launch {
        &self.launch
    }

    /// The tab's title: the user's name for it, T3's label, or `Terminal N`.
    pub(crate) fn title(&self, cx: &App) -> SharedString {
        let custom = self
            .view
            .read(cx)
            .custom_title()
            .filter(|title| !title.trim().is_empty());
        custom
            .map(str::to_string)
            .or_else(|| self.label.clone())
            .unwrap_or_else(|| default_label(&self.launch.terminal_id))
            .into()
    }

    /// The size of the grid, in columns and rows, as the view last laid it out.
    pub(crate) fn grid_size(&self, cx: &App) -> Option<(i64, i64)> {
        let bounds = self.terminal.read(cx).last_content().terminal_bounds;
        let size = (bounds.num_columns() as i64, bounds.num_lines() as i64);
        (size.0 > 0 && size.1 > 0).then(|| clamp_size(size))
    }

    /// Types `command` into the shell and runs it, as T3 runs a project script: with a space
    /// first, which keeps it out of the shell's history.
    pub(crate) fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        self.send(format!(" {command}\r"), cx);
    }

    /// Ends the shell and deletes its history, when the user closes the terminal. If T3 can't
    /// close it, it types `exit`, as T3's app does.
    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        self._attach = Task::ready(());
        let backend = self.backend.clone();
        let launch = self.launch.clone();
        cx.background_spawn(async move {
            let input = TerminalCloseInput {
                thread_id: launch.thread_id.clone(),
                terminal_id: Some(launch.terminal_id.clone()),
                delete_history: Some(true),
            };
            if let Err(error) = backend
                .call::<rpc::TerminalClose>(input, Access::TerminalOperate)
                .await
            {
                log::warn!("couldn't close terminal {}: {error:#}", launch.terminal_id);
                let exit = TerminalWriteInput {
                    thread_id: launch.thread_id,
                    terminal_id: launch.terminal_id,
                    data: "exit\n".into(),
                };
                backend
                    .call::<rpc::TerminalWrite>(exit, Access::TerminalOperate)
                    .await
                    .log_err();
            }
        })
        .detach();
    }

    fn apply(&mut self, event: TerminalAttachStreamEvent, cx: &mut Context<Self>) -> bool {
        self.attached = true;
        let first = !std::mem::replace(&mut self.opened, true);
        match event {
            TerminalAttachStreamEvent::Snapshot(event) => {
                let snapshot = event.snapshot;
                self.set_label(&snapshot.label, cx);
                self.replace_output(snapshot.history.as_bytes(), first, cx);
                if !first && snapshot.status == TerminalSessionStatus::Exited {
                    // The shell exited while mustr was away.
                    return self.shell_exited(cx);
                }
            }
            TerminalAttachStreamEvent::Restarted(event) => {
                self.set_label(&event.snapshot.label, cx);
                self.replace_output(event.snapshot.history.as_bytes(), false, cx);
            }
            TerminalAttachStreamEvent::Output(event) => {
                self.terminal.update(cx, |terminal, cx| {
                    terminal.write_raw_output(event.data.as_bytes(), cx)
                });
            }
            TerminalAttachStreamEvent::Cleared(_) => self.replace_output(b"", false, cx),
            TerminalAttachStreamEvent::Activity(event) => self.set_label(&event.label, cx),
            TerminalAttachStreamEvent::Error(event) => {
                self.system_message(&event.message, cx);
            }
            TerminalAttachStreamEvent::Exited(_) => return self.shell_exited(cx),
            TerminalAttachStreamEvent::Closed(_) => {
                self.ended = true;
                cx.emit(RemoteTerminalEvent::Ended);
                return true;
            }
            TerminalAttachStreamEvent::Unrecognized(_) => {}
        }
        false
    }

    /// T3's app closes a terminal whose shell exits while it's attached, keeping its history,
    /// and removes it from the drawer, as a local terminal's tab closes when its shell exits.
    fn shell_exited(&mut self, cx: &mut Context<Self>) -> bool {
        self.ended = true;
        let backend = self.backend.clone();
        let input = TerminalCloseInput {
            thread_id: self.launch.thread_id.clone(),
            terminal_id: Some(self.launch.terminal_id.clone()),
            delete_history: None,
        };
        cx.background_spawn(async move {
            backend
                .call::<rpc::TerminalClose>(input, Access::TerminalOperate)
                .await
                .log_err();
        })
        .detach();
        cx.emit(RemoteTerminalEvent::Ended);
        true
    }

    fn set_label(&mut self, label: &str, cx: &mut Context<Self>) {
        let label = Some(label.trim())
            .filter(|label| !label.is_empty())
            .map(str::to_string);
        if label != self.label {
            self.label = label;
            cx.emit(RemoteTerminalEvent::TitleChanged);
            cx.notify();
        }
    }

    fn replace_output(&mut self, bytes: &[u8], fresh: bool, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            if fresh {
                terminal.write_raw_output(bytes, cx);
            } else if let Err(error) = terminal.replace_display_output(bytes, cx) {
                log::error!("couldn't replace a terminal's output: {error:#}");
            }
        });
    }

    /// A line of mustr's own in the terminal, as T3's app writes `[terminal] …`.
    fn system_message(&mut self, message: &str, cx: &mut Context<Self>) {
        let line = format!("\r\n[terminal] {message}\r\n");
        self.terminal.update(cx, |terminal, cx| {
            terminal.write_raw_output(line.as_bytes(), cx)
        });
    }

    fn disconnected(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.attached, false) {
            self.system_message("Reconnecting to T3…", cx);
        }
    }

    // Input.

    /// Sends what the user typed or pasted, and does locally what Zed's terminal does on input:
    /// scrolls to the bottom and clears the selection.
    fn send(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        if text.is_empty() || self.ended {
            return;
        }
        // A display-only terminal's input goes nowhere but its local side effects.
        self.terminal
            .update(cx, |terminal, _| terminal.input(text.clone().into_bytes()));
        self.input.unbounded_send(text).ok();
        self.view.update(cx, |_, cx| cx.notify());
    }

    fn mode(&self, cx: &App) -> Modes {
        self.terminal.read(cx).last_content().mode
    }

    /// Sends a mouse report: no local side effects, unlike typed input.
    fn send_report(&mut self, report: Option<Vec<u8>>) {
        if self.ended {
            return;
        }
        if let Some(text) = report.and_then(mouse::as_text) {
            self.input.unbounded_send(text).ok();
        }
    }

    /// The cell under a mouse position.
    fn mouse_cell(&self, position: gpui::Point<Pixels>, cx: &App) -> mouse::Cell {
        let content = self.terminal.read(cx).last_content();
        let bounds = content.terminal_bounds;
        mouse::cell_at(
            position - bounds.bounds.origin,
            &bounds,
            content.display_offset,
        )
    }

    /// A press, for a program that asks for mouse reports. Returns whether it went to it.
    fn mouse_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        let mode = self.mode(cx);
        if !mouse::reports(mode, &event.modifiers) {
            return false;
        }
        let cell = self.mouse_cell(event.position, cx);
        self.last_mouse_cell = Some(cell);
        self.reported_press = Some(event.button);
        self.send_report(mouse::button_report(
            cell,
            event.button,
            &event.modifiers,
            true,
            mode,
        ));
        true
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) -> bool {
        if self.reported_press.take().is_none() {
            return false;
        }
        let mode = self.mode(cx);
        let cell = self.mouse_cell(event.position, cx);
        self.last_mouse_cell = Some(cell);
        self.send_report(mouse::button_report(
            cell,
            event.button,
            &event.modifiers,
            false,
            mode,
        ));
        true
    }

    fn mouse_moved(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) -> bool {
        let mode = self.mode(cx);
        if !mouse::reports(mode, &event.modifiers) {
            return false;
        }
        let cell = self.mouse_cell(event.position, cx);
        if self.last_mouse_cell == Some(cell) {
            return true;
        }
        self.last_mouse_cell = Some(cell);
        self.send_report(mouse::moved_report(
            cell,
            event.pressed_button,
            &event.modifiers,
            mode,
        ));
        true
    }

    /// Keys that send escape sequences (arrows, Enter, `ctrl-c`, …). Plain text goes on to the
    /// input handler, which an IME uses too. Vi mode moves the local selection, so it stays
    /// local.
    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.read(cx).vi_mode_enabled()
            || (event.prefer_character_input && event.keystroke.key_char.is_some())
        {
            return;
        }
        if self.send_keystroke(&event.keystroke, cx) {
            cx.stop_propagation();
        }
    }

    fn send_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let option_as_meta = TerminalSettings::get_global(cx).option_as_meta;
        match keys::to_esc_str(keystroke, self.mode(cx), option_as_meta) {
            Some(text) => {
                self.send(text.into_owned(), cx);
                true
            }
            None => false,
        }
    }

    /// Zed's `Terminal::paste`: bracketed when the program asks for it.
    fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = if self.mode(cx).contains(Modes::BRACKETED_PASTE) {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.send(text, cx);
    }

    /// Zed's `TerminalView::paste`: text, or a raw `ctrl-v` for an image, so agents' TUIs can
    /// read the clipboard themselves. Local file paths mean nothing on T3's machine.
    fn paste_clipboard(&mut self, text_only: bool, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else {
            return;
        };
        match clipboard.entries().first() {
            Some(ClipboardEntry::Image(image)) if !text_only && !image.bytes.is_empty() => {
                self.send("\x16", cx);
            }
            _ => {
                if let Some(text) = clipboard.text() {
                    self.paste_text(&text, cx);
                }
            }
        }
    }

    /// The wheel in a full-screen program (`less`, `man`, an agent's TUI) sends arrow keys, as
    /// Zed's terminal does with alternate scroll. Elsewhere it scrolls the local history.
    fn scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) -> bool {
        let content = self.terminal.read(cx).last_content();
        let mode = content.mode;
        let reports = mouse::reports(mode, &event.modifiers);
        if event.shift || (!reports && !mode.contains(Modes::ALT_SCREEN | Modes::ALTERNATE_SCROLL))
        {
            return false;
        }
        let line_height = content.terminal_bounds.line_height;
        let cell = mouse::cell_at(
            event.position - content.terminal_bounds.bounds.origin,
            &content.terminal_bounds,
            content.display_offset,
        );
        if event.touch_phase == TouchPhase::Started {
            self.scroll_lines.set(0.);
            return true;
        }
        let delta = match event.delta {
            ScrollDelta::Lines(lines) => lines.y,
            ScrollDelta::Pixels(pixels) if line_height > px(0.) => pixels.y / line_height,
            ScrollDelta::Pixels(_) => 0.,
        };
        let total = self.scroll_lines.get() + delta;
        let lines = total.trunc();
        self.scroll_lines.set(total - lines);
        if lines != 0. {
            if reports {
                // One report per line, at most three a step, as Zed's terminal sends them.
                let steps = (lines.abs() as usize).min(3);
                for _ in 0..steps {
                    self.send_report(mouse::scroll_report(
                        cell,
                        lines > 0.,
                        &event.modifiers,
                        mode,
                    ));
                }
            } else {
                self.send(alt_scroll(lines as i32), cx);
            }
        }
        true
    }

    /// Where an IME's text being composed shows: at the cursor, a line high.
    fn marked_text_at(&self, cx: &App) -> Option<(String, gpui::Point<Pixels>, Pixels)> {
        let text = self.marked_text.clone().filter(|text| !text.is_empty())?;
        let content = self.terminal.read(cx).last_content();
        let bounds = content.terminal_bounds;
        let cursor = content.cursor.point;
        let line = cursor.line + content.display_offset as i32;
        let origin = bounds.bounds.origin
            + point(
                bounds.cell_width * cursor.column as f32,
                bounds.line_height * line as f32,
            );
        Some((text, origin, bounds.line_height))
    }

    fn grid_changed(&mut self, cx: &mut Context<Self>) {
        let Some(size) = self.grid_size(cx) else {
            return;
        };
        if self.sent_size == Some(size) || !self.attached || self.ended {
            if self.sent_size == Some(size) {
                self.resize = None;
            }
            return;
        }
        let backend = self.backend.clone();
        let launch = self.launch.clone();
        self.resize = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_DEBOUNCE).await;
            let Ok(Some(size)) = this.update(cx, |this, cx| {
                let size = this.grid_size(cx)?;
                this.sent_size = Some(size);
                Some(size)
            }) else {
                return;
            };
            let input = TerminalResizeInput {
                thread_id: launch.thread_id,
                terminal_id: launch.terminal_id,
                cols: size.0,
                rows: size.1,
            };
            if let Err(error) = backend
                .call::<rpc::TerminalResize>(input, Access::TerminalOperate)
                .await
            {
                log::warn!("couldn't resize a terminal on T3's machine: {error:#}");
            }
        }));
    }
}

/// Paints an IME's text being composed over the cell it covers, underlined, as Zed's terminal
/// does for a local shell (`crates/terminal_view/src/terminal_element.rs`).
fn paint_marked_text(
    text: &str,
    origin: gpui::Point<Pixels>,
    line_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let theme = theme_settings::ThemeSettings::get_global(cx);
    let terminal = TerminalSettings::get_global(cx);
    let mut font = theme.buffer_font.clone();
    if let Some(family) = &terminal.font_family {
        font.family = family.0.clone().into();
    }
    let font_size = terminal
        .font_size
        .unwrap_or_else(|| theme.buffer_font_size(cx));
    let colors = cx.theme().colors();
    let color = colors.terminal_foreground;
    let background = colors.terminal_background;
    let shaped = window.text_system().shape_line(
        text.to_string().into(),
        font_size,
        &[TextRun {
            len: text.len(),
            font,
            color,
            underline: Some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.),
                wavy: false,
            }),
            ..Default::default()
        }],
        None,
    );
    window.paint_quad(fill(
        Bounds::new(origin, gpui::size(shaped.width, line_height)),
        background,
    ));
    shaped
        .paint(origin, line_height, gpui::TextAlign::Left, None, window, cx)
        .log_err();
}

fn clamp_size((cols, rows): (i64, i64)) -> (i64, i64) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

/// Zed's `alt_scroll` (`crates/terminal/src/mappings/mouse.rs`): an up or down arrow per line.
fn alt_scroll(lines: i32) -> String {
    let key = if lines > 0 { "\x1bOA" } else { "\x1bOB" };
    key.repeat(lines.unsigned_abs() as usize)
}

/// The text of a `terminal::SendText` or `SendKeystroke` binding. Their field is private, so
/// it's read back from their `Debug` output, `SendText("\u{1b}b")`.
pub(crate) fn action_text(action: &dyn Debug) -> Option<String> {
    let debug = format!("{action:?}");
    let inner = debug.split_once("(\"")?.1.strip_suffix("\")")?;
    let mut text = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        match chars.next()? {
            'n' => text.push('\n'),
            'r' => text.push('\r'),
            't' => text.push('\t'),
            '0' => text.push('\0'),
            c @ ('\\' | '"' | '\'') => text.push(c),
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let hex = chars.by_ref().take_while(|c| *c != '}').collect::<String>();
                text.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(text)
}

/// Follows the terminal: attaches, applies what T3 streams, and attaches again with `resume`
/// when the connection drops. Ends when the shell does.
async fn attach(
    this: WeakEntity<RemoteTerminal>,
    backend: Arc<dyn ThreadBackend>,
    launch: Launch,
    size: Option<(i64, i64)>,
    ready: oneshot::Sender<Result<()>>,
    cx: &mut gpui::AsyncApp,
) {
    let mut ready = Some(ready);
    let mut resume = false;
    let mut attempt = 0;
    loop {
        let size = if resume {
            this.read_with(cx, |this, cx| this.grid_size(cx).or(this.sent_size))
                .ok()
                .flatten()
        } else {
            size
        };
        let input = launch.attach_input(resume, size);
        let error = match backend.stream::<rpc::TerminalAttach>(input, Access::TerminalOperate) {
            Ok(mut stream) => loop {
                match stream.next().await {
                    Some(Ok(event)) => {
                        attempt = 0;
                        resume = true;
                        let Ok(ended) = this.update(cx, |this, cx| this.apply(event, cx)) else {
                            return;
                        };
                        if let Some(ready) = ready.take() {
                            ready.send(Ok(())).ok();
                        }
                        if ended {
                            return;
                        }
                    }
                    Some(Err(error)) => break Some(error),
                    None => break None,
                }
            },
            Err(error) => Some(error),
        };
        // It never opened: the panel says why.
        if let Some(ready) = ready.take() {
            let error = error.unwrap_or_else(|| anyhow!("T3 ended the terminal before it opened"));
            ready.send(Err(error)).ok();
            return;
        }
        if let Some(error) = &error {
            log::info!("terminal {} lost T3: {error:#}", launch.terminal_id);
        }
        if this.update(cx, |this, cx| this.disconnected(cx)).is_err() {
            return;
        }
        cx.background_executor()
            .timer(reconnect_delay(attempt))
            .await;
        attempt += 1;
    }
}

fn reconnect_delay(attempt: usize) -> Duration {
    const DELAYS: [Duration; 4] = [
        Duration::from_millis(500),
        Duration::from_secs(2),
        Duration::from_secs(5),
        Duration::from_secs(10),
    ];
    DELAYS[attempt.min(DELAYS.len() - 1)]
}

/// Sends input in order, one `terminal.write` at a time, joining what was typed meanwhile.
async fn send_input(
    this: WeakEntity<RemoteTerminal>,
    backend: Arc<dyn ThreadBackend>,
    launch: Launch,
    mut input: mpsc::UnboundedReceiver<String>,
    cx: &mut gpui::AsyncApp,
) {
    while let Some(mut data) = input.next().await {
        while data.len() < MAX_WRITE_BYTES {
            match input.try_recv() {
                Ok(more) => data.push_str(&more),
                Err(_) => break,
            }
        }
        for chunk in write_chunks(&data) {
            let write = TerminalWriteInput {
                thread_id: launch.thread_id.clone(),
                terminal_id: launch.terminal_id.clone(),
                data: chunk.to_string(),
            };
            if let Err(error) = backend
                .call::<rpc::TerminalWrite>(write, Access::TerminalOperate)
                .await
            {
                log::warn!("couldn't type in a terminal on T3's machine: {error:#}");
                let message = format!("Couldn't send input: {error:#}");
                if this
                    .update(cx, |this, cx| this.system_message(&message, cx))
                    .is_err()
                {
                    return;
                }
                break;
            }
        }
    }
}

/// `data` in pieces of at most [`MAX_WRITE_BYTES`], split between characters.
fn write_chunks(data: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_WRITE_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        chunks.push(chunk);
        rest = tail;
    }
    chunks
}

impl Focusable for RemoteTerminal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

/// Typed text, as the platform's input handler (and an IME) sends it, instead of Zed's handler
/// for the view, which would write it to a local PTY. The text being composed isn't drawn.
impl EntityInputHandler for RemoteTerminal {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        self.send(text, cx);
        window.invalidate_character_coordinates();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    /// Where the IME's window goes: at the cursor.
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let content = self.terminal.read(cx).last_content();
        let bounds = content.terminal_bounds;
        let cursor = content.cursor.point;
        let line = cursor.line + content.display_offset as i32;
        let column = cursor.column + range.start;
        let origin = bounds.bounds.origin
            + point(
                bounds.cell_width * column as f32,
                bounds.line_height * line as f32,
            );
        Some(Bounds::new(
            origin,
            gpui::size(bounds.cell_width, bounds.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for RemoteTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let focus_handle = self.view.focus_handle(cx);
        div()
            .relative()
            .size_full()
            .capture_key_down(cx.listener(Self::key_down))
            .capture_action(cx.listener(|this, _: &terminal::Paste, _, cx| {
                this.paste_clipboard(false, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &editor::actions::Paste, _, cx| {
                this.paste_clipboard(false, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &terminal::PasteText, _, cx| {
                this.paste_clipboard(true, cx);
                cx.stop_propagation();
            }))
            .capture_action(
                cx.listener(|this, action: &terminal_view::SendText, _, cx| {
                    if let Some(text) = action_text(action) {
                        this.send(text, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .capture_action(
                cx.listener(|this, action: &terminal_view::SendKeystroke, _, cx| {
                    if let Some(keystroke) =
                        action_text(action).and_then(|text| Keystroke::parse(&text).log_err())
                    {
                        this.send_keystroke(&keystroke, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            // Zed's terminal element sets the grid's size while it lays out.
            .on_children_prepainted({
                let this = entity.downgrade();
                move |_, _, cx| {
                    this.update(cx, |this, cx| this.grid_changed(cx)).ok();
                }
            })
            .child(self.view.clone())
            // Painted after the terminal, so its input handler and wheel listener win.
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, cx| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                        let this = entity.downgrade();
                        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                            if phase != DispatchPhase::Capture || !bounds.contains(&event.position)
                            {
                                return;
                            }
                            let handled = this
                                .update(cx, |this, cx| this.scroll_wheel(event, cx))
                                .unwrap_or(false);
                            if handled {
                                cx.stop_propagation();
                            }
                        });
                        // Clicks, drags and moves go to programs that ask for mouse reports,
                        // instead of selecting.
                        let this = entity.downgrade();
                        let focus = focus_handle.clone();
                        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                            if phase != DispatchPhase::Capture || !bounds.contains(&event.position)
                            {
                                return;
                            }
                            let handled = this
                                .update(cx, |this, cx| this.mouse_down(event, cx))
                                .unwrap_or(false);
                            if handled {
                                window.focus(&focus, cx);
                                cx.stop_propagation();
                            }
                        });
                        let this = entity.downgrade();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let handled = this
                                .update(cx, |this, cx| this.mouse_up(event, cx))
                                .unwrap_or(false);
                            if handled {
                                cx.stop_propagation();
                            }
                        });
                        let this = entity.downgrade();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase != DispatchPhase::Capture
                                || (!bounds.contains(&event.position)
                                    && event.pressed_button.is_none())
                            {
                                return;
                            }
                            let handled = this
                                .update(cx, |this, cx| this.mouse_moved(event, cx))
                                .unwrap_or(false);
                            if handled {
                                cx.stop_propagation();
                            }
                        });
                        let marked = entity.read(cx).marked_text_at(cx);
                        if let Some((text, origin, line_height)) = marked {
                            paint_marked_text(&text, origin, line_height, window, cx);
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests;
