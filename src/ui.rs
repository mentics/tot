use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use tui_textarea::{CursorRenderMode, TextArea};

use crate::app::{
    App, BranchLockedAction, CredentialPromptKind, EditFocus, FieldPromptKind, SettingsFocus,
    StaleWorktreeAction, View,
};
use crate::credentials;
use crate::dirty;
use crate::note_util;
use crate::settings::{
    ISSUE_ID_PLACEHOLDER, NAMESPACE_PLACEHOLDER, PR_NUMBER_PLACEHOLDER, REPOSITORY_PLACEHOLDER,
};
use crate::treehouse::LeasePathConflictKind;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let footer_h = footer_height(app, frame.area().width);
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(footer_h)]).areas(frame.area());

    match app.view {
        View::TaskList => draw_task_list(frame, body, app),
        View::Archive => draw_archive_list(frame, body, app),
        View::Edit => draw_edit(frame, body, app),
        View::Settings => draw_settings(frame, body, app),
        View::CreatePrompt => draw_create_prompt(frame, body, app),
        View::FieldPrompt => draw_field_prompt(frame, body, app),
        View::ModulePrPicker => draw_module_pr_picker(frame, body, app),
        View::CredentialPrompt => draw_credential_prompt(frame, body, app),
        View::CredentialFileBroken => draw_credential_file_broken(frame, body, app),
        View::SwitchModules => draw_switch_modules(frame, body, app),
        View::SwitchBranch => draw_switch_branch(frame, body, app),
        View::DirtyWarning => draw_dirty_warning(frame, body, app),
        View::StaleWorktree => draw_stale_worktree(frame, body, app),
        View::BranchLocked => draw_branch_locked(frame, body, app),
        View::NoteDetail => draw_note_view(frame, body, app),
        View::NoteCompose => draw_note_compose(frame, body, app),
        View::ConfirmDelete => draw_confirm_delete(frame, body, app),
    }
    draw_footer(frame, footer, app);
}

const TITLE_MAX_CHARS: usize = 80;

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn format_task_row(
    title: &str,
    issue_id: Option<&str>,
    branch: Option<&str>,
    wt_num: Option<i32>,
) -> ListItem<'static> {
    let title = truncate_chars(title, TITLE_MAX_CHARS);
    let first = match issue_id {
        Some(id) => format!("{id} {title}"),
        None => title,
    };

    let mut lines = vec![Line::from(Span::raw(first))];

    if let Some(number) = wt_num {
        let second = match branch {
            Some(branch) => format!("  {number}: {branch}"),
            None => format!("  {number}"),
        };
        lines.push(Line::from(Span::styled(
            second,
            Style::default().fg(Color::DarkGray),
        )));
    } else if let Some(branch) = branch {
        // No worktree yet, but still show the branch on the secondary line.
        lines.push(Line::from(Span::styled(
            format!("  {branch}"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    ListItem::new(lines)
}

fn draw_task_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .active_tasks()
        .map(|(_, task)| {
            format_task_row(
                &task.title,
                task.issue_id.as_deref(),
                task.branch.as_deref(),
                task.worktree.as_ref().map(|wt| wt.number),
            )
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().title("Tasks").borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .bg(Color::DarkGray),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn draw_archive_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .archived_tasks()
        .map(|(_, task)| {
            format_task_row(
                &task.title,
                task.issue_id.as_deref(),
                task.branch.as_deref(),
                task.worktree.as_ref().map(|wt| wt.number),
            )
        })
        .collect();

    let title = if items.is_empty() {
        "Archive (empty)"
    } else {
        "Archive"
    };

    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .bg(Color::DarkGray),
        )
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut app.archive_state);
}

fn style_field(ta: &mut TextArea<'static>, title: impl Into<String>, focused: bool) {
    let border = if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    ta.set_block(
        Block::default()
            .title(title.into())
            .borders(Borders::ALL)
            .border_style(border),
    );
    if focused {
        ta.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        ta.set_cursor_render_mode(CursorRenderMode::Cell);
    } else {
        ta.set_cursor_style(Style::default());
        ta.set_cursor_render_mode(CursorRenderMode::Hidden);
    }
}

fn draw_edit(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(edit) = app.edit.as_mut() else {
        frame.render_widget(
            Paragraph::new("No task selected").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    if app.tasks.get(edit.task_idx).is_none() {
        frame.render_widget(
            Paragraph::new("Task missing").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    }

    let [
        title_area,
        branch_area,
        issue_area,
        modules,
        notes_area,
        readonly,
    ] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(4),
    ])
    .areas(area);

    let focus = edit.focus;
    style_field(&mut edit.title_input, "Title", focus == EditFocus::Title);
    style_field(&mut edit.branch_input, "Branch", focus == EditFocus::Branch);
    style_field(
        &mut edit.issue_input,
        "Issue ID",
        focus == EditFocus::IssueId,
    );

    frame.render_widget(&edit.title_input, title_area);
    frame.render_widget(&edit.branch_input, branch_area);
    frame.render_widget(&edit.issue_input, issue_area);

    let task = &app.tasks[edit.task_idx];
    let focus_style = |focused: bool| {
        if focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        }
    };

    let module_items: Vec<ListItem> = edit
        .available_modules
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let selected = task.modules.iter().any(|m| m == name);
            let mark = if selected { "[x]" } else { "[ ]" };
            let pr_label = task
                .module_prs
                .get(name)
                .map(|n| format!("  PR #{n}"))
                .unwrap_or_default();
            let cursor = if edit.focus == EditFocus::Modules && i == edit.module_cursor {
                "> "
            } else {
                "  "
            };
            let style = if edit.focus == EditFocus::Modules && i == edit.module_cursor {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{cursor}{mark} {name}{pr_label}"),
                style,
            )))
        })
        .collect();

    let modules_title = if edit.focus == EditFocus::Modules {
        "Modules (Space toggle, Enter/P set PR)"
    } else {
        "Modules"
    };
    let modules_list = List::new(module_items).block(
        Block::default()
            .title(modules_title)
            .borders(Borders::ALL)
            .border_style(focus_style(edit.focus == EditFocus::Modules)),
    );
    frame.render_widget(modules_list, modules);

    let notes_inner_width = notes_area.width.saturating_sub(4) as usize;
    let preview_width = notes_inner_width.saturating_sub(2); // cursor prefix
    let note_items: Vec<ListItem> = if task.notes.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  (no notes — press N to add)",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        task.notes
            .iter()
            .enumerate()
            .map(|(i, note)| {
                let preview = note_util::one_line_preview(&note.body, preview_width.max(1));
                let cursor = if edit.focus == EditFocus::Notes && i == edit.note_cursor {
                    "> "
                } else {
                    "  "
                };
                let style = if edit.focus == EditFocus::Notes && i == edit.note_cursor {
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .bg(Color::DarkGray)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{cursor}{preview}"),
                    style,
                )))
            })
            .collect()
    };

    let notes_title = if edit.focus == EditFocus::Notes {
        "Notes (Enter view, N add, Del delete)"
    } else {
        "Notes"
    };
    let notes_list = List::new(note_items).block(
        Block::default()
            .title(notes_title)
            .borders(Borders::ALL)
            .border_style(focus_style(edit.focus == EditFocus::Notes)),
    );
    frame.render_widget(notes_list, notes_area);

    let wt_focused = focus == EditFocus::Worktree;
    let (wt_text, wt_hint) = match &task.worktree {
        Some(wt) => {
            let line = format!("Worktree: {} ({})", wt.number, wt.path.display());
            let hint = if wt_focused {
                "Del/Backspace/D clear association (does not return to Treehouse)"
            } else {
                "↓ focus · Del/D clear association"
            };
            (line, hint)
        }
        None => (
            "Worktree: (none)".to_string(),
            if wt_focused {
                "No association to clear"
            } else {
                ""
            },
        ),
    };
    let wt_style = if wt_focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut wt_lines = vec![Line::from(Span::styled(wt_text, wt_style))];
    if !wt_hint.is_empty() {
        wt_lines.push(Line::from(Span::styled(
            wt_hint,
            Style::default().fg(Color::DarkGray),
        )));
    }
    let wt_title = if wt_focused {
        "Worktree (focused)"
    } else {
        "Worktree"
    };
    let readonly_widget = Paragraph::new(wt_lines).block(
        Block::default()
            .title(wt_title)
            .borders(Borders::ALL)
            .border_style(focus_style(wt_focused)),
    );
    frame.render_widget(readonly_widget, readonly);
}

fn draw_note_view(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(state) = app.note_view.as_ref() else {
        frame.render_widget(
            Paragraph::new("No note selected").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    let task_idx = state.task_idx;
    let note_idx = state.note_idx;
    let scroll = state.scroll;

    let Some(task) = app.tasks.get(task_idx) else {
        frame.render_widget(
            Paragraph::new("Task missing").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    let Some(note) = task.notes.get(note_idx) else {
        frame.render_widget(
            Paragraph::new("Note missing").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let created = note.created_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let body = note.body.clone();
    let block = Block::default()
        .title(format!("Note — {created}"))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.max(1) as usize;
    let rows = note_util::layout_note(&body, width);
    let urls = note_util::find_urls(&body);
    let visible = inner.height;
    let max_scroll = rows.len().saturating_sub(visible as usize) as u16;
    let scroll = scroll.min(max_scroll);

    let link_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);

    let mut lines: Vec<Line> = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let visual = row_idx as i32 - scroll as i32;
        if visual < 0 || visual >= visible as i32 {
            continue;
        }
        let mut spans = Vec::new();
        let mut col = 0usize;
        let chars: Vec<char> = row.text.chars().collect();
        while col < chars.len() {
            let src = row.col_to_src.get(col).copied().unwrap_or(col);
            if let Some(url) = urls.iter().find(|u| src >= u.start && src < u.end) {
                let mut url_text = String::new();
                while col < chars.len() {
                    let s = row.col_to_src[col];
                    if s >= url.start && s < url.end {
                        url_text.push(chars[col]);
                        col += 1;
                    } else {
                        break;
                    }
                }
                spans.push(Span::styled(url_text, link_style));
            } else {
                let mut plain = String::new();
                while col < chars.len() {
                    let s = row.col_to_src[col];
                    if urls.iter().any(|u| s >= u.start && s < u.end) {
                        break;
                    }
                    plain.push(chars[col]);
                    col += 1;
                }
                spans.push(Span::raw(plain));
            }
        }
        lines.push(Line::from(spans));
    }

    let hits = note_util::visible_link_hits(&rows, &urls, inner.x, inner.y, scroll, visible);
    if let Some(state) = app.note_view.as_mut() {
        state.scroll = scroll;
        state.link_hits = hits;
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_note_compose(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(state) = app.note_compose.as_mut() else {
        frame.render_widget(
            Paragraph::new("Compose unavailable").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let [hint_area, editor_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);

    let hint = Paragraph::new(vec![
        Line::from("New note"),
        Line::from(Span::styled(
            "Ctrl+S save · Esc cancel · Enter for newline",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(hint, hint_area);

    state.input.set_block(
        Block::default()
            .title("Note body")
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
    );
    state.input.set_cursor_line_style(Style::default());
    state
        .input
        .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(&state.input, editor_area);
}

fn draw_settings(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(state) = app.settings_state.as_mut() else {
        frame.render_widget(
            Paragraph::new("Settings unavailable").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let [hint_area, issue_area, pr_area, help_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);

    let hint = Paragraph::new(vec![
        Line::from("URL templates for opening issues and pull requests in a browser."),
        Line::from(Span::styled(
            "Changes save immediately.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().title("Settings").borders(Borders::ALL));
    frame.render_widget(hint, hint_area);

    let focus = state.focus;
    style_field(
        &mut state.issue_input,
        format!("Issue URL template (use {ISSUE_ID_PLACEHOLDER})"),
        focus == SettingsFocus::IssueUrlTemplate,
    );
    style_field(
        &mut state.pr_input,
        format!(
            "PR URL template (use {NAMESPACE_PLACEHOLDER}, {REPOSITORY_PLACEHOLDER}, {PR_NUMBER_PLACEHOLDER})"
        ),
        focus == SettingsFocus::PrUrlTemplate,
    );
    frame.render_widget(&state.issue_input, issue_area);
    frame.render_widget(&state.pr_input, pr_area);

    let help = Paragraph::new(vec![
        Line::from(format!(
            "Issue example: https://linear.app/your-workspace/issue/{ISSUE_ID_PLACEHOLDER}"
        )),
        Line::from(format!(
            "PR example: https://github.com/{NAMESPACE_PLACEHOLDER}/{REPOSITORY_PLACEHOLDER}/pull/{PR_NUMBER_PLACEHOLDER}"
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Namespace comes from the git origin remote; repository is the module name when opening a PR.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(help, help_area);
}

fn draw_field_prompt(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(prompt) = app.field_prompt.as_mut() else {
        frame.render_widget(
            Paragraph::new("Prompt unavailable").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let (title, hint_line) = match &prompt.kind {
        FieldPromptKind::IssueId { .. } => (
            "Issue ID".to_string(),
            "This task has no issue ID. Enter one to open it in the browser:".to_string(),
        ),
        FieldPromptKind::PrNumber { module, .. } => (
            "PR number".to_string(),
            format!("Enter the pull request number for module `{module}` (empty clears):"),
        ),
    };

    let [hint_top, input_area, hint_bottom] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);

    let top = Paragraph::new(vec![Line::from(hint_line)])
        .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(top, hint_top);

    prompt.input.set_block(
        Block::default()
            .title("Input")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    prompt
        .input
        .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(&prompt.input, input_area);

    let bottom = Paragraph::new(vec![Line::from(Span::styled(
        "←/→ Home/End  Enter confirm  Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(bottom, hint_bottom);
}

fn draw_module_pr_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = app.module_pr_picker.as_ref() else {
        frame.render_widget(
            Paragraph::new("Picker unavailable").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let items: Vec<ListItem> = picker
        .options
        .iter()
        .enumerate()
        .map(|(i, (module, pr))| {
            let cursor = if i == picker.cursor { "> " } else { "  " };
            let label = match pr {
                Some(n) => format!("{cursor}{module}  PR #{n}"),
                None => format!("{cursor}{module}  (enter PR)"),
            };
            let style = if i == picker.cursor {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .title("Open PR — choose module")
            .borders(Borders::ALL),
    );
    frame.render_widget(list, area);
}

fn draw_create_prompt(frame: &mut Frame, area: Rect, app: &mut App) {
    let [hint_top, input_area, hint_bottom] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);

    let top = Paragraph::new(vec![Line::from(
        "Enter a title, branch (prefix/name), or issue ID (ABC-123):",
    )])
    .block(
        Block::default()
            .title("Create new task")
            .borders(Borders::ALL),
    );
    frame.render_widget(top, hint_top);

    app.create_input.set_block(
        Block::default()
            .title("Input")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    app.create_input
        .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(&app.create_input, input_area);

    let bottom = Paragraph::new(vec![Line::from(Span::styled(
        "←/→ Home/End word-jump edit  Enter create  Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(bottom, hint_bottom);
}

fn draw_credential_prompt(frame: &mut Frame, area: Rect, app: &mut App) {
    let fallback_path = credentials::linear_api_key_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.config/tod/credentials/linear_api_key".to_string());
    let (line1, line2, line3) = match app.credential_prompt_kind {
        CredentialPromptKind::Missing => (
            "Linear API key not found in the OS keyring (or encrypted config file).".to_string(),
            "Paste your key below. Prefer OS keyring (service `tod`, account `linear`)."
                .to_string(),
            format!(
                "If the keyring is unavailable, an encrypted copy is stored at {fallback_path}."
            ),
        ),
        CredentialPromptKind::Invalid => (
            "It looks like the API key you entered previously is invalid.".to_string(),
            "Try entering a new one; it will replace the stored key.".to_string(),
            format!(
                "Storage: OS keyring when available, otherwise encrypted file at {fallback_path}."
            ),
        ),
    };

    let [hint_top, input_area, hint_bottom] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);

    let top = Paragraph::new(vec![
        Line::from(line1),
        Line::from(line2),
        Line::from(line3),
    ])
    .block(
        Block::default()
            .title("Linear credentials")
            .borders(Borders::ALL),
    );
    frame.render_widget(top, hint_top);

    app.credential_input.set_block(
        Block::default()
            .title("API key")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    app.credential_input
        .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(&app.credential_input, input_area);

    let bottom = Paragraph::new(vec![Line::from(Span::styled(
        "←/→ Home/End word-jump edit  Enter save  Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(bottom, hint_bottom);
}

fn draw_credential_file_broken(frame: &mut Frame, area: Rect, app: &App) {
    let path = app
        .credential_broken_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/tod/credentials/linear_api_key".to_string());

    let lines = vec![
        Line::from(Span::styled(
            "Credential file cannot be decrypted",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(
            "The encrypted Linear API key file was written on a different machine \
             (or is corrupt). Common after recreating a container.",
        ),
        Line::from(""),
        Line::from(Span::styled(
            path,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Recreate it? You will be asked for a new API key."),
        Line::from(""),
        Line::from(Span::styled(
            "[Y] Recreate    [N] Cancel",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .title("Credential file")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn draw_confirm_delete(frame: &mut Frame, area: Rect, app: &App) {
    let title = app
        .edit
        .as_ref()
        .and_then(|e| app.tasks.get(e.task_idx))
        .map(|t| t.title.as_str())
        .unwrap_or("(unknown task)");

    let lines = vec![
        Line::from(Span::styled(
            "Permanently delete this task?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("This removes the task file from disk. It cannot be undone."),
        Line::from(""),
        Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[Y] Delete    [N] Cancel",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Delete task").borders(Borders::ALL)),
        area,
    );
}

fn draw_switch_modules(frame: &mut Frame, area: Rect, app: &App) {
    let Some(prep) = app.switch_prep.as_ref() else {
        frame.render_widget(
            Paragraph::new("No switch in progress").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };
    let Some(task) = app.tasks.get(prep.task_idx) else {
        frame.render_widget(
            Paragraph::new("Task missing").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let [intro, modules] =
        Layout::vertical([Constraint::Length(4), Constraint::Fill(1)]).areas(area);

    let intro_widget = Paragraph::new(vec![
        Line::from(format!("Task: {}", task.title)),
        Line::from(Span::styled(
            "Select which modules use the task branch (Space toggles).",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(
        Block::default()
            .title("Switch — modules")
            .borders(Borders::ALL),
    );
    frame.render_widget(intro_widget, intro);

    let module_items: Vec<ListItem> = prep
        .available_modules
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let selected = task.modules.iter().any(|m| m == name);
            let mark = if selected { "[x]" } else { "[ ]" };
            let cursor = if i == prep.module_cursor { "> " } else { "  " };
            let style = if i == prep.module_cursor {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{cursor}{mark} {name}"),
                style,
            )))
        })
        .collect();

    let modules_list = List::new(module_items).block(
        Block::default()
            .title("Modules")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(modules_list, modules);
}

fn draw_switch_branch(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = app
        .switch_prep
        .as_ref()
        .and_then(|prep| app.tasks.get(prep.task_idx))
        .map(|t| t.title.as_str())
        .unwrap_or("(none)")
        .to_string();

    let [hint_top, input_area, hint_bottom] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(area);

    let top = Paragraph::new(vec![
        Line::from(format!("Task: {title}")),
        Line::from("Enter the git branch name for this task:"),
    ])
    .block(
        Block::default()
            .title("Switch — branch")
            .borders(Borders::ALL),
    );
    frame.render_widget(top, hint_top);

    if let Some(prep) = app.switch_prep.as_mut() {
        prep.branch_input.set_block(
            Block::default()
                .title("Branch")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );
        prep.branch_input
            .set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_widget(&prep.branch_input, input_area);
    }

    let bottom = Paragraph::new(vec![Line::from(Span::styled(
        "←/→ Home/End word-jump edit  Enter continue  Esc cancel",
        Style::default().fg(Color::DarkGray),
    ))])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(bottom, hint_bottom);
}

fn draw_dirty_warning(frame: &mut Frame, area: Rect, app: &App) {
    let Some(rel) = app.release.as_ref() else {
        frame.render_widget(
            Paragraph::new("No release in progress").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let task_title = app
        .tasks
        .get(rel.task_idx)
        .map(|t| t.title.as_str())
        .unwrap_or("(missing)");
    let context = if rel.then_archive {
        "Releasing worktree before archive"
    } else {
        "Releasing worktree"
    };

    let mut report_lines: Vec<Line> = vec![
        Line::from(format!("Task: {task_title}")),
        Line::from(Span::styled(context, Style::default().fg(Color::DarkGray))),
        Line::from(""),
    ];
    for line in dirty::format_report_lines(&rel.report) {
        let style = if line.starts_with('[') {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if line.contains("blocked") {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        report_lines.push(Line::from(Span::styled(line, style)));
    }

    let action_items: Vec<ListItem> = rel
        .actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let cursor = if i == rel.action_cursor { "> " } else { "  " };
            let shortcut = action.shortcut().to_ascii_uppercase();
            let style = if i == rel.action_cursor {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{cursor}[{shortcut}] {}", action.label()),
                style,
            )))
        })
        .collect();

    let [report_area, actions_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(6)]).areas(area);

    let report_widget = Paragraph::new(report_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title("Dirty worktree warning")
                .borders(Borders::ALL),
        );
    frame.render_widget(report_widget, report_area);

    let actions_list = List::new(action_items).block(
        Block::default()
            .title("Options")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(actions_list, actions_area);
}

fn draw_stale_worktree(frame: &mut Frame, area: Rect, app: &App) {
    let Some(stale) = app.stale_worktree.as_ref() else {
        frame.render_widget(
            Paragraph::new("No worktree path recovery in progress")
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let actions = StaleWorktreeAction::for_kind(stale.kind);
    let selected = actions
        .get(stale.action_cursor)
        .copied()
        .unwrap_or(StaleWorktreeAction::Cancel);

    let (title, headline, explanation) = match stale.kind {
        LeasePathConflictKind::MissingButRegistered => (
            "Stale worktree",
            "Git worktree registration is stale",
            "Treehouse could not create a worktree because this path is missing on disk \
             but still registered with git:",
        ),
        LeasePathConflictKind::AlreadyExists => (
            "Path already exists",
            "Worktree path already exists on disk",
            "Treehouse could not create a worktree because this path already exists \
             (often a leftover from a failed earlier attempt):",
        ),
    };

    let report_lines = vec![
        Line::from(Span::styled(
            headline,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(explanation),
        Line::from(""),
        Line::from(Span::styled(
            stale.problem_path.display().to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "Repo: {} (prune/remove apply to main + all submodules)",
                stale.repo_root.display()
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let mut report_lines = report_lines;
    if let Some(sub) = stale.submodule_rel.as_ref() {
        report_lines.push(Line::from(Span::styled(
            format!("Conflict in submodule: {}", sub.display()),
            Style::default().fg(Color::DarkGray),
        )));
    }
    report_lines.extend([
        Line::from(""),
        Line::from("Choose how to clear it, then tod will retry the lease."),
    ]);

    let action_items: Vec<ListItem> = actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let cursor = if i == stale.action_cursor { "> " } else { "  " };
            let shortcut = action.shortcut().to_ascii_uppercase();
            let style = if i == stale.action_cursor {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{cursor}[{shortcut}] {}", action.label()),
                style,
            )))
        })
        .collect();

    let desc_lines = vec![
        Line::from(Span::styled(
            selected.label(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(selected.description()),
    ];

    let actions_height = (actions.len() as u16 + 2).max(4).min(8);
    let [report_area, actions_area, desc_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(actions_height),
        Constraint::Length(8),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(report_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(title).borders(Borders::ALL)),
        report_area,
    );
    frame.render_widget(
        List::new(action_items).block(
            Block::default()
                .title("Options")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        actions_area,
    );
    frame.render_widget(
        Paragraph::new(desc_lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .title("About this option")
                .borders(Borders::ALL),
        ),
        desc_area,
    );
}

fn draw_branch_locked(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.branch_locked.as_ref() else {
        frame.render_widget(
            Paragraph::new("No branch-lock recovery in progress")
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let actions = BranchLockedAction::available(state.other_worktree.is_some());
    let selected = actions
        .get(state.action_cursor)
        .copied()
        .unwrap_or(BranchLockedAction::Cancel);
    let other_num = state.other_worktree.as_ref().map(|w| w.number);

    let mut report_lines = vec![
        Line::from(Span::styled(
            "Branch locked by another worktree",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "Cannot check out `{}` because another worktree already has it:",
            state.branch
        )),
        Line::from(""),
        Line::from(Span::styled(
            state.conflicting_path.display().to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "Current task worktree: {} ({})",
                state.current_worktree.number,
                state.current_worktree.path.display()
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(other) = state.other_worktree.as_ref() {
        report_lines.push(Line::from(Span::styled(
            format!(
                "Other worktree: {} ({})",
                other.number,
                other.path.display()
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
    report_lines.push(Line::from(""));
    report_lines.push(Line::from(
        "If that other path is a leftover from a crash, prefer using it. \
         Removing it is destructive if it still has real work.",
    ));

    let action_items: Vec<ListItem> = actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let cursor = if i == state.action_cursor { "> " } else { "  " };
            let shortcut = action.shortcut().to_ascii_uppercase();
            let style = if i == state.action_cursor {
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{cursor}[{shortcut}] {}", action.label()),
                style,
            )))
        })
        .collect();

    let desc_lines = vec![
        Line::from(Span::styled(
            selected.label(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(selected.description(other_num)),
    ];

    let actions_height = (actions.len() as u16 + 2).max(4).min(7);
    let [report_area, actions_area, desc_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(actions_height),
        Constraint::Length(6),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(report_lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("Branch in use")
                    .borders(Borders::ALL),
            ),
        report_area,
    );
    frame.render_widget(
        List::new(action_items).block(
            Block::default()
                .title("Options")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        actions_area,
    );
    frame.render_widget(
        Paragraph::new(desc_lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .title("About this option")
                .borders(Borders::ALL),
        ),
        desc_area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(status) = &app.status {
        let style = if status.is_error {
            Style::default()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD)
        } else if status.busy {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let text = if status.busy {
            format!("{} {}", app.spinner_char(), status.text)
        } else {
            status.text.clone()
        };
        for line in text.lines() {
            lines.push(Line::from(Span::styled(line.to_string(), style)));
        }
        if text.is_empty() {
            lines.push(Line::from(Span::styled(String::new(), style)));
        }
    }
    // Context (e.g. selection) never shares a line with controls.
    if let Some(context) = footer_context(app) {
        lines.push(Line::from(context));
    }
    // Controls are always the last footer line(s), alone.
    lines.push(Line::from(footer_controls(app)));

    let footer = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

/// Max wrapped lines reserved for the status message inside the footer.
const MAX_STATUS_LINES: u16 = 10;

/// Non-control footer context (selection label, etc.). Always above controls.
fn footer_context(app: &App) -> Option<String> {
    match app.view {
        View::TaskList => Some(
            app.selected_list_task()
                .map(|t| format!("Selected: {}", t.title))
                .unwrap_or_else(|| "No selection".to_string()),
        ),
        View::Archive => Some(
            app.selected_archive_task()
                .map(|t| format!("Selected: {}", t.title))
                .unwrap_or_else(|| "Archive empty".to_string()),
        ),
        _ => None,
    }
}

/// Keybinding help only — never include status or selection text here.
fn footer_controls(app: &App) -> String {
    match app.view {
        View::TaskList => {
            "↑/↓ move  Enter open  N new  E edit  I issue  P PR  R release  A archive  Shift+A archive  S settings  Q quit"
                .to_string()
        }
        View::Archive => "↑/↓ move  U unarchive  Esc back  Q quit".to_string(),
        View::Edit => {
            "Tab/↑/↓ fields  ←/→ edit text  Space toggle module  Enter/P set PR  N add note  Del/D clear/delete  Ctrl+D delete task  Esc back  Q quit"
                .to_string()
        }
        View::NoteDetail => {
            "↑/↓ scroll  Ctrl+click link  Esc back  Q quit".to_string()
        }
        View::NoteCompose => {
            "Type note  Enter newline  Ctrl+S save  Esc cancel".to_string()
        }
        View::ConfirmDelete => "Y delete  N/Esc cancel  Q quit".to_string(),
        View::Settings => "Tab/↑/↓ fields  ←/→ edit  Esc back  Q quit".to_string(),
        View::CreatePrompt => "Type / move cursor  Enter create  Esc cancel".to_string(),
        View::FieldPrompt => "Type / move cursor  Enter confirm  Esc cancel".to_string(),
        View::ModulePrPicker => "↑/↓ move  Enter open  Esc cancel  Q quit".to_string(),
        View::CredentialPrompt => "Type / move cursor  Enter save  Esc cancel".to_string(),
        View::CredentialFileBroken => "Y recreate  N/Esc cancel  Q quit".to_string(),
        View::SwitchModules => {
            "↑/↓ move  Space toggle  Enter confirm  Esc cancel  Q quit".to_string()
        }
        View::SwitchBranch => "Type / move cursor  Enter continue  Esc cancel".to_string(),
        View::DirtyWarning => {
            "↑/↓ move  Enter choose  C check again  S stash  X/Esc cancel  Q quit".to_string()
        }
        View::StaleWorktree => match app.stale_worktree.as_ref().map(|s| s.kind) {
            Some(LeasePathConflictKind::AlreadyExists) => {
                "↑/↓ move  Enter choose  C clear path  D delete dir  R remove  X/Esc cancel  Q quit"
                    .to_string()
            }
            _ => "↑/↓ move  Enter choose  O override  P prune  R remove  X/Esc cancel  Q quit"
                .to_string(),
        },
        View::BranchLocked => {
            "↑/↓ move  Enter choose  U use that worktree  R remove other  X/Esc cancel  Q quit"
                .to_string()
        }
    }
}

/// Footer height: borders + status + optional context + controls (each on own lines).
fn footer_height(app: &App, term_width: u16) -> u16 {
    let inner_width = term_width.saturating_sub(2).max(1) as usize;
    let context_lines = match footer_context(app) {
        Some(context) => wrapped_line_count(&context, inner_width) as u16,
        None => 0,
    };
    let control_lines = wrapped_line_count(&footer_controls(app), inner_width) as u16;
    let status_lines = match &app.status {
        Some(status) => {
            let text = if status.busy {
                format!("{} {}", app.spinner_char(), status.text)
            } else {
                status.text.clone()
            };
            (wrapped_line_count(&text, inner_width) as u16).clamp(1, MAX_STATUS_LINES)
        }
        None => 0,
    };
    // 2 border rows + content; keep at least the old 3-row footprint when idle.
    (2 + status_lines + context_lines + control_lines)
        .max(3)
        .min(2 + MAX_STATUS_LINES + context_lines + control_lines.max(1))
}

fn wrapped_line_count(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut total = 0;
    for line in text.split('\n') {
        if line.is_empty() {
            total += 1;
            continue;
        }
        let chars = line.chars().count();
        total += chars.div_ceil(width).max(1);
    }
    total.max(1)
}
