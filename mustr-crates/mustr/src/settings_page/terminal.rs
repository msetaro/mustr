//! Terminal: the size of its text (the code's, unless set), its shell, and its cursor.

use super::{
    SettingsView, merged,
    rows::{self, Callback, Group, SettingRow},
    update_settings,
};
use editor::Editor;
use gpui::{
    App, Context, Entity, Focusable as _, InteractiveElement as _, ParentElement as _,
    SharedString, Styled as _, Window, px,
};
use settings::{CursorShapeContent, Settings as _, Shell, TerminalBlink};
use std::rc::Rc;
use terminal::terminal_settings::TerminalSettings;
use theme_settings::{ThemeSettings, clamp_font_size};
use ui::prelude::*;

/// The program the settings run as the shell, if it's one of the user's.
fn shell_program(cx: &App) -> Option<String> {
    match merged(cx)
        .terminal
        .as_ref()
        .and_then(|terminal| terminal.project.shell.clone())
    {
        Some(Shell::Program(program)) => Some(program),
        Some(Shell::WithArguments { program, .. }) => Some(program),
        Some(Shell::System) | None => None,
    }
}

/// Puts the shell the settings name in the field, unless the user is typing in it.
pub(super) fn fill_shell(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    let program = shell_program(cx).unwrap_or_default();
    editor.update(cx, |editor, cx| {
        if !editor.is_focused(window) && editor.text(cx) != program {
            editor.set_text(program, window, cx);
        }
    });
}

impl SettingsView {
    fn save_shell(&mut self, cx: &mut Context<Self>) {
        let program = self.shell.read(cx).text(cx).trim().to_string();
        let shell = if program.is_empty() {
            Shell::System
        } else {
            Shell::Program(program)
        };
        self.custom_shell = false;
        update_settings(cx, move |settings, _| {
            settings.terminal.get_or_insert_default().project.shell = Some(shell)
        });
        cx.notify();
    }
}

pub(super) fn groups(
    view: &mut SettingsView,
    window: &mut Window,
    cx: &mut Context<SettingsView>,
) -> Vec<Group> {
    let settings = TerminalSettings::get_global(cx);
    let own_size = settings.font_size;
    let code = ThemeSettings::get_global(cx).buffer_font_size(cx);
    let size = own_size.unwrap_or(code);
    let step = |delta: f32| -> Option<Callback> {
        let new = clamp_font_size(size + px(delta));
        (new != size).then(|| {
            Rc::new(move |_: &mut Window, cx: &mut App| {
                update_settings(cx, move |settings, _| {
                    settings.terminal.get_or_insert_default().font_size =
                        Some(f32::from(new).into())
                })
            }) as Callback
        })
    };
    let font_size = h_flex()
        .gap_2()
        .child(rows::stepper(
            "settings-terminal-font-size",
            format!("{}", f32::from(size)),
            step(-1.),
            step(1.),
            cx,
        ))
        .child(rows::button(
            "settings-terminal-font-size-reset",
            "Use the code's",
            own_size
                .is_none()
                .then(|| "The terminal follows the code size".into()),
            |_, cx| {
                update_settings(cx, |settings, _| {
                    if let Some(terminal) = settings.terminal.as_mut() {
                        terminal.font_size = None;
                    }
                })
            },
        ));

    let program = shell_program(cx);
    let custom = program.is_some() || view.custom_shell;
    let this = cx.entity().downgrade();
    let shell_kind = rows::segmented(
        "settings-shell",
        &["Your login shell", "Another program"],
        usize::from(custom),
        None,
        move |choice, window, cx| {
            this.update(cx, |this, cx| {
                if choice == 0 {
                    this.custom_shell = false;
                    update_settings(cx, |settings, _| {
                        settings.terminal.get_or_insert_default().project.shell =
                            Some(Shell::System)
                    });
                } else {
                    this.custom_shell = true;
                    window.focus(&this.shell.focus_handle(cx), cx);
                }
                cx.notify();
            })
            .ok();
        },
    );
    let mut shell_rows = vec![
        SettingRow::new(
            "settings-row-shell",
            "Shell",
            "What terminals on this computer run: your login shell, or a program you name.",
            shell_kind,
        )
        .keywords("bash zsh fish program command"),
    ];
    if custom {
        let typed = view.shell.read(cx).text(cx);
        let changed = typed.trim() != program.as_deref().unwrap_or_default();
        let this = cx.entity().downgrade();
        shell_rows.push(
            SettingRow::new(
                "settings-row-shell-program",
                "Program",
                "Run with no arguments. Enter saves it.",
                h_flex()
                    .gap_1()
                    .child(
                        rows::field(&view.shell, Some(220.), cx)
                            .key_context("MustrShellProgram")
                            .on_action(
                                cx.listener(|this, _: &menu::Confirm, _, cx| this.save_shell(cx)),
                            ),
                    )
                    .child(rows::button(
                        "settings-shell-save",
                        "Save",
                        (!changed).then(|| SharedString::from("Type a program to save it")),
                        move |_, cx| {
                            this.update(cx, |this, cx| this.save_shell(cx)).ok();
                        },
                    )),
            )
            .keywords("shell bash zsh fish command"),
        );
    }

    const CURSORS: [(CursorShapeContent, &str); 4] = [
        (CursorShapeContent::Block, "Block"),
        (CursorShapeContent::Bar, "Bar"),
        (CursorShapeContent::Underline, "Underline"),
        (CursorShapeContent::Hollow, "Hollow"),
    ];
    let terminal = merged(cx).terminal.as_ref();
    let cursor = terminal
        .and_then(|terminal| terminal.cursor_shape)
        .unwrap_or_default();
    let blinking = terminal
        .and_then(|terminal| terminal.blinking)
        .unwrap_or(TerminalBlink::TerminalControlled);
    const BLINKS: [(TerminalBlink, &str); 3] = [
        (TerminalBlink::TerminalControlled, "When the program asks"),
        (TerminalBlink::On, "Always"),
        (TerminalBlink::Off, "Never"),
    ];
    let blink_selected = BLINKS.iter().position(|(choice, _)| *choice == blinking);
    let blink_choices = BLINKS
        .iter()
        .map(|(blink, label)| {
            let blink = *blink;
            (
                SharedString::from(*label),
                Rc::new(move |_: &mut Window, cx: &mut App| {
                    update_settings(cx, move |settings, _| {
                        settings.terminal.get_or_insert_default().blinking = Some(blink)
                    })
                }) as Callback,
            )
        })
        .collect();
    let mut rows = vec![
        SettingRow::new(
            "settings-row-terminal-font-size",
            "Text size",
            if own_size.is_some() {
                "Terminals' own size. Zooming scales it with the rest of mustr."
            } else {
                "The same as the code, until you change it here."
            },
            font_size,
        )
        .keywords("font size zoom"),
    ];
    rows.extend(shell_rows);
    rows.push(
        SettingRow::new(
            "settings-row-terminal-cursor",
            "Cursor",
            "The cursor's shape, unless the program running in the terminal picks one.",
            rows::segmented(
                "settings-terminal-cursor",
                &CURSORS.map(|(_, label)| label),
                CURSORS
                    .iter()
                    .position(|(choice, _)| *choice == cursor)
                    .unwrap_or(0),
                None,
                |choice, _, cx| {
                    let shape = CURSORS[choice].0;
                    update_settings(cx, move |settings, _| {
                        settings.terminal.get_or_insert_default().cursor_shape = Some(shape)
                    })
                },
            ),
        )
        .keywords("caret shape"),
    );
    rows.push(
        SettingRow::new(
            "settings-row-terminal-blinking",
            "Cursor blinks",
            "Whether the terminal's cursor blinks.",
            rows::dropdown(
                "settings-terminal-blinking",
                blink_selected.map_or("When the program asks", |index| BLINKS[index].1),
                blink_choices,
                blink_selected,
                None,
                window,
                cx,
            ),
        )
        .keywords("blink caret"),
    );
    vec![Group::new("Terminals", rows)]
}
