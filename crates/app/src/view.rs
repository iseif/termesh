//! Pure render: the frame is a function of `&Model` (ARCHITECTURE.md §7.1).
use ratatui::Frame;
use termesh_ui as ui;

use crate::git_state::GitGroup;
use crate::model::{Model, Overlay};

const PROJECT_EMPTY: &str =
    "No workspace open.\n\nStart with a path:\n  termesh .\n  termesh /path/to/project";
const PROJECT_FIRST_RUN: &str = "No workspace open. Welcome. Three keys are enough to start:\n\n  Ctrl+P  Open a file\n  F10     Actions\n  F11     Help\n\nOr run: termesh /path/to/project";
const EDITOR_BODY: &str = "No file open.\n\nSelect a file in the Project pane and press Enter.\n\nCtrl+P Quick Open · F9 Search · F10 Actions · Ctrl+S Save";
const TERMINAL_EMPTY: &str = "$\n\nNo terminal open.\nUse F6 or the command palette to start one.";
pub fn render(frame: &mut Frame, model: &Model) {
    let area = frame.area();
    let r = ui::regions(area, &model.layout);
    let t = &model.theme;

    render_project(frame, r.project, model, t);
    render_editor(frame, r.editor, model, t);
    render_terminal(frame, r.terminal, model, t);
    // The pane reports how tall its body is; `main` feeds that back so scrolling can be
    // clamped without the model having to know the pane's geometry.
    let agent_lines = ui::widgets::pane_scrolled(
        frame,
        r.agent,
        "Agent",
        model.focus == ui::Pane::Agent,
        &agent_body(model),
        model.agent_scroll,
        t,
    );
    let _ = agent_lines;

    let hints = "Ctrl+P Files   F9 Search   F10 Actions   Tab Focus   F6 Terminal   Ctrl+Q Quit";
    let project = match &model.explorer {
        Some(e) => {
            format!("{} ({})", e.root.display_name(), termesh_workspace::kind_labels(&e.root.kinds))
        }
        None => "no workspace".to_string(),
    };
    let position = match model.active_buffer() {
        Some(b) => {
            let (line, column) = b.cursor_position();
            format!("   {}:{}", line + 1, column + 1)
        }
        None => String::new(),
    };
    let git = git_status_context(model);
    let lsp = lsp_status_context(model);
    let errors = model
        .problem_rows()
        .iter()
        .filter(|problem| problem.severity == termesh_core::DiagnosticSeverity::Error)
        .count();
    // The count was hardcoded until Phase 07 made it live, so it never had to read
    // correctly at one.
    let plural = if errors == 1 { "error" } else { "errors" };
    let context = format!(
        "{project}   focus: {}{position}{git}{lsp}   {errors} {plural}",
        model.focus.title()
    );
    // The status bar sizes the right-hand context first and gives the hints whatever is
    // left, so a task readout written at full length simply eats them. Offer the same
    // fact at three lengths and take the longest that still leaves the hints room.
    //
    // The shortest form is never dropped, even when nothing fits: the hint strip is
    // static text the user learns once, while a running task's state is the live thing
    // they are watching. Squeezing the hints is the correct loss. What survives to the
    // last rung is *which task* and *how it ended* — the run number, origin, and problem
    // count are all recoverable from the Problems overlay; "Test failed" is not.
    let right = model
        .task_runs
        .last()
        .map(|run| {
            let origin = match run.origin {
                termesh_core::TaskOrigin::Human => "human",
                termesh_core::TaskOrigin::Agent { .. } => "agent",
            };
            let status = task_status(run.status);
            let problems = run.problems.len();
            let candidates = [
                format!(
                    "   task #{} {} {status} ({origin}) · {problems} problem(s)",
                    run.id.0, run.spec.label
                ),
                format!("   {} {status} · {problems} problem(s)", run.spec.label),
                format!("   {} {status} · {problems}", run.spec.label),
                format!("   {} {status}", run.spec.label),
            ]
            .map(|suffix| format!("{context}{suffix}"));
            candidates
                .iter()
                .find(|candidate| fits(&r.status, hints, candidate))
                .cloned()
                .unwrap_or_else(|| candidates[candidates.len() - 1].clone())
        })
        .unwrap_or_else(|| context.clone());
    ui::widgets::status_bar(frame, r.status, hints, &right, model.notification.as_deref(), t);

    match model.overlays.last() {
        Some(Overlay::Palette(p)) => {
            let items = p.view_items();
            ui::widgets::command_palette(frame, area, &p.query, &items, p.selected, p.total(), t);
        }
        Some(Overlay::Help(help)) => {
            let rows = help
                .visible_rows()
                .into_iter()
                .map(|row| ui::overlays::HelpViewRow {
                    group: row.group.into(),
                    title: row.title.into(),
                    id: row.id.into(),
                    chord: row.chord.as_deref().map(display_help_chord),
                })
                .collect::<Vec<_>>();
            ui::overlays::help(
                frame,
                area,
                ui::overlays::HelpView { query: &help.query, rows: &rows, scroll: help.scroll },
                t,
            );
        }
        Some(Overlay::Prompt(p)) => {
            ui::widgets::prompt(frame, area, &p.title, &p.input, p.takes_input(), t);
        }
        Some(Overlay::Search(search)) => {
            let items = search.view_items();
            let status = search.status_text();
            let title = match search.mode {
                termesh_core::SearchMode::Files => "Quick Open",
                termesh_core::SearchMode::Text => "Search Workspace",
            };
            ui::overlays::search(
                frame,
                area,
                ui::overlays::SearchView {
                    title,
                    query: &search.query,
                    items: &items,
                    selected: search.selected,
                    status: &status,
                    hints: "Enter Open · Esc Close",
                    preview: search.preview_text().map(|text| (text, search.preview_start_line())),
                },
                t,
            );
        }
        Some(Overlay::Tasks(picker)) => {
            let items: Vec<String> = picker
                .items()
                .iter()
                .map(|task| format!("{}  {} {}", task.label, task.program, task.args.join(" ")))
                .collect();
            ui::overlays::search(
                frame,
                area,
                ui::overlays::SearchView {
                    title: "Run Task",
                    query: "",
                    items: &items,
                    selected: picker.selected,
                    status: "",
                    hints: "Enter Run · Esc Close",
                    preview: None,
                },
                t,
            );
        }
        Some(Overlay::AgentModes(picker)) => {
            // The description is the agent's own account of what the mode permits, and
            // the only reliable one — so it is on the row, not hidden behind a preview.
            let items: Vec<String> = picker
                .modes
                .iter()
                .map(|mode| {
                    let marker =
                        if picker.current.as_deref() == Some(&mode.id) { "\u{25CF}" } else { " " };
                    match &mode.description {
                        Some(description) => {
                            format!("{marker} {}  \u{2014} {description}", mode.name)
                        }
                        None => format!("{marker} {}", mode.name),
                    }
                })
                .collect();
            ui::overlays::search(
                frame,
                area,
                ui::overlays::SearchView {
                    title: "Agent Session Mode",
                    query: "",
                    items: &items,
                    selected: picker.selected,
                    status: "",
                    hints: "Enter Set \u{00B7} Esc Close",
                    preview: None,
                },
                t,
            );
        }
        Some(Overlay::Problems(problems)) => {
            let root = model.explorer.as_ref().map(|explorer| explorer.root.path.as_path());
            let items = problems
                .items()
                .iter()
                .map(|problem| {
                    let severity = match problem.severity {
                        termesh_core::DiagnosticSeverity::Error => "error",
                        termesh_core::DiagnosticSeverity::Warning => "warning",
                        termesh_core::DiagnosticSeverity::Info => "info",
                        termesh_core::DiagnosticSeverity::Hint => "hint",
                    };
                    let inside = root.is_some_and(|root| problem.path.starts_with(root));
                    let path = root
                        .and_then(|root| problem.path.strip_prefix(root).ok())
                        .unwrap_or(&problem.path)
                        .to_string_lossy();
                    let outside = if inside { "" } else { " (outside workspace)" };
                    format!(
                        "[{}] {severity} {path}:{}:{}{outside}  {}",
                        problem.source,
                        problem.line,
                        problem.column,
                        problem.message.lines().next().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>();
            ui::overlays::problems(
                frame,
                area,
                ui::overlays::ProblemsView { items: &items, selected: problems.selected },
                t,
            );
        }
        Some(Overlay::GitStatus(status)) => {
            let rows = status
                .rows()
                .iter()
                .map(|row| ui::overlays::GitStatusViewRow {
                    group: match row.group {
                        GitGroup::Conflicts => "Conflicts",
                        GitGroup::Staged => "Staged",
                        GitGroup::Changes => "Changes",
                    },
                    status: git_row_status(row),
                    path: format!(
                        "{}{}{}",
                        row.path.to_string_lossy(),
                        // A rename is two paths; showing only the new one hides half of
                        // what the developer is about to stage or commit.
                        match &row.kind {
                            termesh_core::GitChangeKind::Renamed { from } =>
                                format!(" ← {}", from.to_string_lossy()),
                            _ => String::new(),
                        },
                        if row.outside_workspace { " (outside workspace)" } else { "" }
                    ),
                })
                .collect::<Vec<_>>();
            ui::overlays::git_status(
                frame,
                area,
                ui::overlays::GitStatusView { rows: &rows, selected: status.selected },
                t,
            );
        }
        Some(Overlay::GitDiff(diff)) => {
            let path = diff.path.to_string_lossy();
            ui::overlays::git_diff(
                frame,
                area,
                ui::overlays::GitDiffView {
                    path: &path,
                    target: match diff.target {
                        termesh_core::GitDiffTarget::Worktree => "worktree",
                        termesh_core::GitDiffTarget::Index => "staged",
                    },
                    text: diff.text.as_deref(),
                    truncated: diff.truncated,
                    error: diff.error.as_deref(),
                    notice: diff.notice.as_deref(),
                    scroll: diff.scroll,
                },
                t,
            );
        }
        Some(Overlay::GitBranches(branches)) => {
            let items = branches
                .branches
                .iter()
                .map(|branch| format!("{} {}", if branch.current { "*" } else { " " }, branch.name))
                .collect::<Vec<_>>();
            ui::overlays::git_branches(
                frame,
                area,
                ui::overlays::GitBranchesView { items: &items, selected: branches.selected },
                t,
            );
        }
        Some(Overlay::Hover(hover)) => {
            ui::overlays::hover(
                frame,
                r.editor,
                editor_cursor_anchor(model, r.editor),
                ui::overlays::HoverView {
                    text: &hover.hover.text,
                    truncated: hover.hover.truncated,
                },
                t,
            );
        }
        Some(Overlay::Completion(completion)) => {
            let items = completion
                .items
                .iter()
                .map(|item| match &item.detail {
                    Some(detail) => format!("{}  {detail}", item.label),
                    None => item.label.clone(),
                })
                .collect::<Vec<_>>();
            ui::overlays::completion(
                frame,
                r.editor,
                editor_cursor_anchor(model, r.editor),
                ui::overlays::SelectionView {
                    title: "Completions",
                    items: &items,
                    selected: completion.selected,
                    footer: "Enter Accept · Esc Close",
                },
                t,
            );
        }
        Some(Overlay::CodeActions(actions)) => {
            let items = actions
                .actions
                .iter()
                .map(|action| match &action.kind {
                    Some(kind) => format!("{}  {kind}", action.title),
                    None => action.title.clone(),
                })
                .collect::<Vec<_>>();
            ui::overlays::selection(
                frame,
                area,
                ui::overlays::SelectionView {
                    title: "Code Actions",
                    items: &items,
                    selected: actions.selected,
                    footer: "Enter Apply · Esc Close",
                },
                t,
            );
        }
        Some(Overlay::References(references)) => {
            let items = references
                .locations
                .iter()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.path.display(),
                        location.range.start.line + 1,
                        location.range.start.character + 1
                    )
                })
                .collect::<Vec<_>>();
            ui::overlays::references(
                frame,
                area,
                ui::overlays::SelectionView {
                    title: "References",
                    items: &items,
                    selected: references.selected,
                    footer: "Enter Open · Esc Close",
                },
                t,
            );
        }
        Some(Overlay::Symbols(symbols)) => {
            let items = symbols
                .rows
                .iter()
                .map(|row| {
                    format!(
                        "{}{}{}",
                        "  ".repeat(row.depth),
                        row.label,
                        row.detail.as_ref().map(|detail| format!("  {detail}")).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>();
            ui::overlays::symbols(
                frame,
                area,
                ui::overlays::SelectionView {
                    title: &symbols.title,
                    items: &items,
                    selected: symbols.selected,
                    footer: "Enter Open · Esc Close",
                },
                t,
            );
        }
        Some(Overlay::DraftRecovery(recovery)) => {
            let items = recovery
                .drafts
                .iter()
                .enumerate()
                .map(|(index, draft)| {
                    format!(
                        "[{}] {}",
                        if recovery.chosen[index] { "x" } else { " " },
                        draft.path.display()
                    )
                })
                .collect::<Vec<_>>();
            ui::overlays::selection(
                frame,
                area,
                ui::overlays::SelectionView {
                    title: "Recover Unsaved Work",
                    items: &items,
                    selected: recovery.selected,
                    footer: "Space Select · Enter Restore Selected · a Restore All · d Discard · Esc Later",
                },
                t,
            );
        }
        None => {}
    }
}

fn editor_cursor_anchor(model: &Model, area: ratatui::layout::Rect) -> (u16, u16) {
    let Some(buffer) = model.active_buffer() else { return (area.x, area.y) };
    let (line, char_column) = buffer.cursor_position();
    let raw = buffer.text().line(line.min(buffer.text().len_lines().saturating_sub(1))).to_string();
    let column = ui::display_column(&raw, char_column, model.settings.tab_width as usize) as u16;
    let strip = u16::from(model.buffers.len() > 1);
    let visible_line = line.saturating_sub(buffer.scroll_top()) as u16;
    (
        area.x.saturating_add(5).saturating_add(column).min(area.right().saturating_sub(1)),
        area.y
            .saturating_add(1 + strip)
            .saturating_add(visible_line)
            .min(area.bottom().saturating_sub(1)),
    )
}

/// Does the right-hand context still leave the whole hint strip visible?
///
/// `status_bar` sizes the right side first and gives the hints whatever is left, so this
/// is the caller's job: the widget has no way to know the hints are the lower priority.
fn fits(status: &ratatui::layout::Rect, hints: &str, right: &str) -> bool {
    let needed = hints.chars().count() + right.chars().count() + 2;
    needed <= usize::from(status.width)
}

fn task_status(status: termesh_core::TaskStatus) -> &'static str {
    match status {
        termesh_core::TaskStatus::Starting => "starting",
        termesh_core::TaskStatus::Running => "running",
        termesh_core::TaskStatus::Succeeded => "succeeded",
        termesh_core::TaskStatus::Failed => "failed",
        termesh_core::TaskStatus::Cancelled => "cancelled",
    }
}

fn git_status_context(model: &Model) -> String {
    let Some(snapshot) = &model.git.snapshot else { return String::new() };
    let branch = if snapshot.branch.detached {
        snapshot.branch.oid.as_deref().map(|oid| &oid[..oid.len().min(7)]).unwrap_or("detached")
    } else {
        snapshot.branch.head.as_deref().unwrap_or("unborn")
    };
    let conflicts = snapshot
        .files
        .iter()
        .filter(|file| {
            matches!(file.index, Some(termesh_core::GitChangeKind::Conflicted))
                || matches!(file.worktree, Some(termesh_core::GitChangeKind::Conflicted))
        })
        .count();
    format!(
        "   branch: {branch} ↑{} ↓{} ~{} !{}",
        snapshot.branch.ahead,
        snapshot.branch.behind,
        snapshot.files.len(),
        conflicts
    )
}

fn lsp_status_context(model: &Model) -> String {
    use crate::lsp_state::LspLoadState;

    fn session_status(language: &str, load: &LspLoadState) -> String {
        match load {
            LspLoadState::Idle | LspLoadState::Ready => String::new(),
            LspLoadState::Starting => "   LSP starting".into(),
            LspLoadState::Indexing { message, percent } => match percent {
                Some(percent) => format!("   LSP {message} {percent}%"),
                None => format!("   LSP {message}"),
            },
            LspLoadState::Unavailable(_) => format!("   LSP {language} unavailable"),
            LspLoadState::Stale(_) => format!("   LSP {language} failed"),
        }
    }

    let active_session = model
        .active_buffer()
        .and_then(termesh_editor::Buffer::path)
        .and_then(|path| model.lsp.server_for(path))
        .and_then(|server| model.lsp.sessions.get(&server));
    if let Some(session) = active_session {
        return session_status(&session.language, &session.load);
    }

    if let Some(session) = model
        .lsp
        .sessions
        .values()
        .find(|session| matches!(session.load, LspLoadState::Indexing { .. }))
    {
        return session_status(&session.language, &session.load);
    }
    if model.lsp.sessions.values().any(|session| matches!(session.load, LspLoadState::Starting)) {
        return "   LSP starting".into();
    }
    if model.lsp.sessions.is_empty() {
        return if model.lsp.configured.is_empty() {
            "   LSP unavailable".into()
        } else {
            String::new()
        };
    }
    if let Some(session) = model.lsp.sessions.values().find(|session| {
        matches!(session.load, LspLoadState::Unavailable(_) | LspLoadState::Stale(_))
    }) {
        return session_status(&session.language, &session.load);
    }
    String::new()
}

fn git_row_status(row: &crate::git_state::GitStatusRow) -> String {
    if row.group == GitGroup::Conflicts {
        return "UU".into();
    }
    let marker = match row.kind {
        termesh_core::GitChangeKind::Modified => 'M',
        termesh_core::GitChangeKind::Added => 'A',
        termesh_core::GitChangeKind::Deleted => 'D',
        termesh_core::GitChangeKind::Renamed { .. } => 'R',
        termesh_core::GitChangeKind::Untracked => '?',
        termesh_core::GitChangeKind::Conflicted => 'U',
    };
    match row.group {
        GitGroup::Staged => format!("{marker} "),
        GitGroup::Changes => format!(" {marker}"),
        GitGroup::Conflicts => "UU".into(),
    }
}

fn render_terminal(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    model: &Model,
    theme: &ui::Theme,
) {
    let Some(active) = model.active_terminal() else {
        ui::widgets::pane(
            frame,
            area,
            "Terminal",
            model.focus == ui::Pane::Terminal,
            TERMINAL_EMPTY,
            theme,
        );
        return;
    };

    let tabs: Vec<ui::widgets::TerminalTab> = model
        .terminals
        .iter()
        .map(|terminal| ui::widgets::TerminalTab {
            label: terminal.title.clone(),
            status: format!(
                "{} \u{00B7} {}",
                terminal_owner(terminal.owner),
                terminal_status(&terminal.status)
            ),
            active: terminal.id == active.id,
        })
        .collect();
    let snapshot = active.screen.snapshot();
    let mut cells = Vec::with_capacity(snapshot.rows() * snapshot.cols());
    for row in 0..snapshot.rows() {
        for col in 0..snapshot.cols() {
            let cell = snapshot.cell(row, col);
            cells.push(ui::widgets::TerminalCell {
                row,
                col,
                symbol: cell.symbol.clone(),
                fg: terminal_color(cell.fg),
                bg: terminal_color(cell.bg),
                attributes: ui::widgets::TerminalAttributes {
                    bold: cell.attributes.bold,
                    dim: cell.attributes.dim,
                    italic: cell.attributes.italic,
                    underline: cell.attributes.underline,
                    inverse: cell.attributes.inverse,
                    hidden: cell.attributes.hidden,
                    strikeout: cell.attributes.strikeout,
                },
                selected: cell.selected,
            });
        }
    }
    let cursor = snapshot.cursor.map(|cursor| ui::widgets::TerminalCursor {
        row: cursor.row,
        col: cursor.col,
        visible: cursor.visible,
    });
    ui::widgets::terminal(
        frame,
        area,
        model.focus == ui::Pane::Terminal,
        &tabs,
        &cells,
        cursor,
        model.terminal_copy_mode(),
        theme,
    );
}

fn terminal_owner(owner: termesh_core::TerminalOwner) -> &'static str {
    match owner {
        termesh_core::TerminalOwner::HumanShell => "shell",
        termesh_core::TerminalOwner::HumanCommand => "command",
        termesh_core::TerminalOwner::Agent { .. } => "agent",
    }
}

fn terminal_status(status: &termesh_core::TerminalStatus) -> String {
    match status {
        termesh_core::TerminalStatus::Starting => "starting".into(),
        termesh_core::TerminalStatus::Running { .. } => "running".into(),
        termesh_core::TerminalStatus::Exited(exit) => match (&exit.code, &exit.signal) {
            (Some(code), _) => format!("exited {code}"),
            (None, Some(signal)) => format!("exited {signal}"),
            (None, None) => "exited".into(),
        },
        termesh_core::TerminalStatus::Failed(_) => "failed".into(),
    }
}

fn terminal_color(color: termesh_terminal::ScreenColor) -> ui::widgets::TerminalColor {
    match color {
        termesh_terminal::ScreenColor::Default => ui::widgets::TerminalColor::Default,
        termesh_terminal::ScreenColor::Indexed(index) => ui::widgets::TerminalColor::Indexed(index),
        termesh_terminal::ScreenColor::Rgb(r, g, b) => ui::widgets::TerminalColor::Rgb(r, g, b),
    }
}

/// The Agent pane: whether an agent is there, what it is saying, and anything waiting on
/// the human.
///
/// The connection state is shown *first* and always. A configured, connected agent with
/// no session yet used to render exactly the same words as no agent at all, which made a
/// working setup look broken — the pane has to distinguish "nothing configured" from
/// "ready when you are".
fn agent_body(model: &Model) -> String {
    let Some(name) = &model.agent_name else {
        // Written for a ~24-column pane: anything longer wraps mid-word and reads as
        // damage rather than instructions.
        return concat!(
            "No agent configured.\n\n",
            "Define one in\n",
            "agents.toml under your\n",
            "config dir. See the\n",
            "README for the shape.\n\n",
            "Or press F6 and run\n",
            "any AI CLI in a\n",
            "terminal.",
        )
        .to_string();
    };

    let restored_history = if model.restored_agent_history.is_empty() {
        String::new()
    } else {
        format!(
            "Prior transcript (read-only).\nThe ACP session did not resume; continue only in a new session.\n\n{}\n\n",
            transcript_text(&model.restored_agent_history)
        )
    };

    let Some(agent) = &model.agent else {
        return format!(
            "\u{25CF} {name}\n  connected\n\n{restored_history}Ask it something:\n  Enter (here)\n  F4  (anywhere)\n  F10 > Prompt\n\nContext:\n  workspace tree\n  open buffers\n  {} catalog task(s)",
            model.task_catalog_len(),
        );
    };

    // Never a bare header: a session with an empty transcript used to render as a blank
    // pane, which looks like the thing broke rather than like it is waiting for you.
    // The mode belongs on screen because it is the answer to "why did nothing happen":
    // an agent held in a read-only mode explains what it would change and then changes
    // nothing, which is indistinguishable from a broken session unless the pane says so
    // (ADR-0015 §2).
    let mode = agent
        .current_mode
        .as_ref()
        .map(|id| {
            let label = agent
                .modes
                .iter()
                .find(|mode| &mode.id == id)
                .map(|mode| mode.name.as_str())
                .unwrap_or(id.as_str());
            format!("  mode: {label}  (F10 > Agent: Session Mode)\n")
        })
        .unwrap_or_default();
    let mut out = format!("\u{25CF} {name}\n{mode}\n{restored_history}");
    if agent.transcript.is_empty() && agent.proposals.is_empty() && !agent.turn_active {
        out.push_str("Session open.\n\nAsk it something:\n  Enter (here)\n  F4  (anywhere)\n\n");
    }

    for terminal in &agent.attached_terminals {
        if let Some(terminal) = model.terminals.iter().find(|candidate| candidate.id == *terminal) {
            let status = match &terminal.status {
                termesh_core::TerminalStatus::Starting => "starting".to_string(),
                termesh_core::TerminalStatus::Running { .. } => "running".to_string(),
                termesh_core::TerminalStatus::Exited(exit) => match exit.code {
                    Some(code) => format!("exited {code}"),
                    None => "exited".to_string(),
                },
                termesh_core::TerminalStatus::Failed(message) => format!("failed: {message}"),
            };
            let released = if terminal.released { ", released" } else { "" };
            out.push_str(&format!(
                "\u{25A3} {} ({status}{released})\n  retained in the Terminal pane\n\n",
                terminal.title
            ));
        }
    }

    out.push_str(&transcript_text(&agent.transcript));

    // Anything waiting on the human goes last, because this pane scrolls from the bottom
    // and snaps back there on new content — so the end is what is on screen. These used to
    // sit above the transcript, on the reasoning that what blocks the agent outranks the
    // conversation. It does; but a long answer then pushed the accept/reject prompt off the
    // top, and the reader never saw the thing they were being asked to decide.
    if let Some(pending) = &agent.pending_permission {
        out.push_str(&format!("\u{26A0} {}\n", pending.summary));

        // An edit permission is answered in the vocabulary of the diff it gates, not of a
        // command: there is no argv to show, and "always" is not offered, because standing
        // permission to edit is the escalation the prompt exists to prevent. The change
        // itself is on screen in the editor, marked in the gutter (ADR-0016 §4).
        if pending.review.is_some() {
            out.push_str("  [a]ccept  [r]eject \u{2014} the agent makes the change if allowed\n\n");
        } else {
            let spec = match &pending.origin {
                crate::model::PermissionOrigin::AgentRequest { terminal_spec, .. } => {
                    terminal_spec.as_ref()
                }
                crate::model::PermissionOrigin::TerminalCreate { spec, .. } => Some(spec),
            };
            if let Some(spec) = spec {
                out.push_str(&format!("  program: {:?}\n", spec.program));
                for (index, argument) in spec.args.iter().enumerate() {
                    out.push_str(&format!("  arg[{index}]: {argument:?}\n"));
                }
                out.push_str(&format!("  cwd: {}\n", spec.cwd.display()));
                for (name, value) in &spec.env {
                    out.push_str(&format!("  env {name:?}={value:?}\n"));
                }
            } else {
                // An edit whose diff could not be placed still needs an answer, and it
                // has no argv — an empty "argv:" invents a command that does not exist.
                if !pending.command.is_empty() {
                    out.push_str("  argv:");
                    for argument in &pending.command {
                        out.push_str(&format!(" {argument:?}"));
                    }
                    out.push('\n');
                }
            }
            out.push_str("  [y] allow once  [A] always  [n] deny\n\n");
        }
    }

    for proposal in &agent.proposals {
        let clean = proposal.applicable().count();
        let conflicts = proposal.hunks.len() - clean;
        let note = if conflicts > 0 { format!(", {conflicts} conflicted") } else { String::new() };
        out.push_str(&format!(
            "\u{258E} Proposed {} edit(s){note}\n\u{258E} in {}\n\u{258E} [a]ccept  [r]eject\n\n",
            proposal.hunks.len(),
            proposal.path.file_name().unwrap_or_default().to_string_lossy(),
        ));
    }

    if agent.turn_active {
        // The ellipsis trails the word, the way it reads aloud: "thinking…", not
        // "…thinking", which looks like a truncated line rather than a state.
        out.push_str("thinking\u{2026}\n\n");
    }

    out
}

/// The whole conversation, oldest first.
///
/// Deliberately *not* trimmed. An earlier version cut this to a character budget, which
/// looked like scrolling but was deletion: the pane could not scroll back to text it had
/// never been given, and a long answer silently ate the question that prompted it. The
/// pane scrolls now, so it gets everything and decides what fits. The transcript itself is
/// bounded by `TRANSCRIPT_LIMIT` turns, which is where the memory limit belongs.
fn transcript_text(lines: &[crate::model::TranscriptLine]) -> String {
    use crate::model::Speaker;

    lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .map(|line| {
            let text = line.text.trim();
            match line.speaker {
                Speaker::You => format!("\u{203A} {text}"),
                Speaker::Agent => text.to_string(),
                // Reasoning is marked so it is not mistaken for the answer.
                Speaker::Thought => format!("\u{2022} {text}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The Editor pane: the active buffer, or a hint before one is open.
fn render_editor(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    model: &Model,
    t: &ui::Theme,
) {
    let focused = model.focus == ui::Pane::Editor;
    let Some(buffer) = model.active_buffer() else {
        ui::widgets::pane(frame, area, "Editor", focused, EDITOR_BODY, t);
        return;
    };

    let tab_width = model.settings.tab_width as usize;
    let text = buffer.text();
    let lines: Vec<ui::widgets::EditorLine> =
        (0..text.len_lines()).map(|i| editor_line(buffer, i, tab_width)).collect();

    // Char offset -> screen cell. Doing it here, once, is what keeps the cursor on top of
    // the character it is actually on when the line contains tabs or wide glyphs.
    let (line, char_column) = buffer.cursor_position();
    let raw_line = text.line(line.min(text.len_lines().saturating_sub(1))).to_string();
    let column = ui::display_column(&raw_line, char_column, tab_width);

    let tabs: Vec<ui::widgets::EditorTab> = model
        .buffers
        .iter()
        .enumerate()
        .map(|(i, b)| ui::widgets::EditorTab {
            name: b.display_name(),
            dirty: b.is_dirty(),
            active: model.active_buffer == Some(i),
        })
        .collect();

    // The buffer decides where the viewport sits — commands scroll it, render only reads
    // it — but clamp here so a resize cannot leave the cursor off screen for a frame.
    // Two borders, plus the tab strip when it is shown.
    let strip = if tabs.len() > 1 { 1 } else { 0 };
    let height = area.height.saturating_sub(2 + strip) as usize;
    let scroll_top = ui::widgets::clamp_viewport(buffer.scroll_top(), line, height);

    let dirty = if buffer.is_dirty() { " \u{2022}" } else { "" };
    let title = format!("{}{dirty}", buffer.display_name());

    ui::widgets::editor(frame, area, &title, focused, &tabs, &lines, (line, column), scroll_top, t);
}

/// Build one rendered line: display text, decoration spans in screen cells, gutter mark.
///
/// This is the boundary the char→cell conversion lives on. `editor` speaks char offsets
/// throughout (ADR-0006 §1) and `ui` speaks display columns; translating in exactly one
/// place is what stops the cursor and the hunks disagreeing about where a tab put them.
fn editor_line(
    buffer: &termesh_editor::Buffer,
    index: usize,
    tab_width: usize,
) -> ui::widgets::EditorLine {
    use termesh_editor::{DecorationClass, HunkSide, Severity, SyntaxKind};
    use ui::widgets::{DecorStyle, SpanStyle};

    let raw = buffer.text().line(index).to_string();
    let (line_start, line_end) = buffer.line_range(index);

    let mut marker = None;
    let spans = buffer
        .decorations()
        .for_line(line_start, line_end)
        .into_iter()
        .map(|d| {
            let style = match d.class {
                DecorationClass::Hunk { side, state, .. } => {
                    let (style, mark) = match (state, side) {
                        // A conflicted hunk must not look like one you can just accept.
                        (termesh_editor::HunkState::Conflicted(_), _) => {
                            (DecorStyle::HunkConflict, '!')
                        }
                        (_, HunkSide::Removed) => (DecorStyle::HunkRemoved, '~'),
                        (_, HunkSide::Added) => (DecorStyle::HunkAdded, '+'),
                    };
                    promote_marker(&mut marker, mark);
                    style
                }
                DecorationClass::Match { current } => {
                    if current {
                        DecorStyle::MatchCurrent
                    } else {
                        DecorStyle::Match
                    }
                }
                DecorationClass::Diagnostic(severity) => {
                    let (style, mark) = match severity {
                        Severity::Error => (DecorStyle::Error, 'E'),
                        Severity::Warning => (DecorStyle::Warning, 'W'),
                        Severity::Info => (DecorStyle::Info, 'I'),
                        Severity::Hint => (DecorStyle::Hint, 'H'),
                    };
                    promote_marker(&mut marker, mark);
                    style
                }
                DecorationClass::Syntax(kind) => match kind {
                    SyntaxKind::Keyword => DecorStyle::Keyword,
                    SyntaxKind::StringLit => DecorStyle::StringLit,
                    SyntaxKind::Comment => DecorStyle::Comment,
                    SyntaxKind::Number => DecorStyle::Number,
                    SyntaxKind::Type => DecorStyle::Type,
                    SyntaxKind::Function => DecorStyle::Function,
                },
            };
            SpanStyle {
                start: ui::display_column(&raw, d.start, tab_width),
                end: ui::display_column(&raw, d.end, tab_width),
                style,
            }
        })
        .collect();

    ui::widgets::EditorLine { text: ui::expand_tabs(&raw, tab_width), spans, marker }
}

fn promote_marker(marker: &mut Option<char>, candidate: char) {
    fn priority(marker: char) -> u8 {
        match marker {
            '!' => 6,
            'E' => 5,
            'W' => 4,
            '~' | '+' => 3,
            'I' => 2,
            'H' => 1,
            _ => 0,
        }
    }
    if marker.is_none_or(|current| priority(candidate) > priority(current)) {
        *marker = Some(candidate);
    }
}

/// The Project pane: the live tree once a workspace is open, a hint before that.
fn render_project(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    model: &Model,
    t: &ui::Theme,
) {
    let focused = model.focus == ui::Pane::Project;
    let Some(explorer) = &model.explorer else {
        let body = if model.is_first_run() { PROJECT_FIRST_RUN } else { PROJECT_EMPTY };
        ui::widgets::pane(frame, area, "Project", focused, body, t);
        return;
    };

    // Flatten the tree into plain render lines; `ui` never sees a filesystem type.
    let rows = explorer.tree.visible_rows();
    let lines: Vec<ui::widgets::TreeLine> = rows
        .iter()
        .map(|r| ui::widgets::TreeLine {
            depth: r.depth,
            name: r.name.clone(),
            is_dir: r.is_expandable,
            expanded: r.expanded,
            loading: r.loading,
            error: r.error.clone(),
            marker: explorer
                .tree
                .path_of(r.id)
                .and_then(|path| git_tree_marker(model, path, r.is_expandable)),
        })
        .collect();

    ui::widgets::file_tree(
        frame,
        area,
        "Project",
        focused,
        &lines,
        explorer.tree.selected_row(),
        t,
    );
}

fn display_help_chord(chord: &str) -> String {
    chord
        .split('+')
        .map(|part| match part {
            "ctrl" => "Ctrl".into(),
            "alt" => "Alt".into(),
            "shift" => "Shift".into(),
            "tab" => "Tab".into(),
            "backspace" => "Backspace".into(),
            "enter" => "Enter".into(),
            "esc" => "Esc".into(),
            "del" => "Del".into(),
            "home" => "Home".into(),
            "end" => "End".into(),
            "pgup" => "PgUp".into(),
            "pgdn" => "PgDn".into(),
            part if part.starts_with('f') || part.chars().count() == 1 => part.to_ascii_uppercase(),
            part => part.into(),
        })
        .collect::<Vec<String>>()
        .join("+")
}

fn git_tree_marker(model: &Model, absolute: &std::path::Path, directory: bool) -> Option<char> {
    let snapshot = model.git.snapshot.as_ref()?;
    let relative = absolute.strip_prefix(&snapshot.repository_root).ok()?;
    let matching = snapshot.files.iter().filter(|file| {
        if directory {
            file.path.starts_with(relative)
        } else {
            file.path == relative
        }
    });
    let mut precedence = 0;
    for file in matching {
        let current = if matches!(file.index, Some(termesh_core::GitChangeKind::Conflicted))
            || matches!(file.worktree, Some(termesh_core::GitChangeKind::Conflicted))
        {
            4
        } else if file.index.is_some() {
            3
        } else if matches!(file.worktree, Some(termesh_core::GitChangeKind::Untracked)) {
            1
        } else if file.worktree.is_some() {
            2
        } else {
            0
        };
        precedence = precedence.max(current);
    }
    match precedence {
        4 => Some('!'),
        3 => Some('+'),
        2 => Some('~'),
        1 => Some('?'),
        _ => None,
    }
}

/// Text rows available in the editor pane at a given terminal size.
///
/// Two borders, plus the tab strip when more than one file is open — otherwise scrolling
/// would be off by a row exactly when a second file is opened.
pub fn editor_rows(width: u16, height: u16, model: &Model) -> usize {
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let strip = if model.buffers.len() > 1 { 1 } else { 0 };
    ui::regions(area, &model.layout).editor.height.saturating_sub(2 + strip) as usize
}

/// Rows and columns inside the terminal pane's border.
pub fn terminal_size(width: u16, height: u16, model: &Model) -> termesh_core::TerminalSize {
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let terminal = ui::regions(area, &model.layout).terminal;
    termesh_core::TerminalSize {
        rows: terminal
            .height
            .saturating_sub(2 + u16::from(model.active_terminal().is_some()))
            .max(1),
        cols: terminal.width.saturating_sub(2).max(1),
    }
}

/// Render one frame to an in-memory backend and return it as text. Powers
/// `termesh --dump-frame` (headless preview / CI snapshot) and the render tests.
/// How far the Agent pane can be scrolled back at a given size.
pub fn agent_scrollback(width: u16, height: u16, model: &Model) -> usize {
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let pane = ui::regions(area, &model.layout).agent;
    let inner_w = pane.width.saturating_sub(2) as usize;
    let inner_h = pane.height.saturating_sub(2) as usize;
    ui::text::wrap(&agent_body(model), inner_w).len().saturating_sub(inner_h)
}

pub fn snapshot(model: &mut Model, width: u16, height: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    // Tell the model how tall the editor is before drawing, so scrolling done by later
    // commands matches what was actually on screen.
    model.set_editor_height(editor_rows(width, height, model));
    model.set_terminal_size(terminal_size(width, height, model));
    model.agent_scroll_max = agent_scrollback(width, height, model);
    let model = &*model;
    let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
    term.draw(|f| render(f, model)).unwrap();
    let buf = term.backend().buffer();
    let mut s = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}
