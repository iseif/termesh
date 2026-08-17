//! Translate crossterm key events into backend-agnostic [`KeyChord`]s and route them:
//! an open overlay captures input; otherwise the keymap resolves a [`Command`].
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use termesh_core::input::{Key, KeyChord, Mods};
use termesh_core::Command;

use crate::model::{Model, Overlay};

/// Translate a crossterm key event into a backend-agnostic chord, or `None` if it
/// carries no meaning for us. Runs on the input pump thread, off the render loop.
pub fn translate_key(ev: KeyEvent) -> Option<KeyChord> {
    if ev.kind == KeyEventKind::Release {
        return None; // some terminals emit press+release; act on press/repeat only
    }
    translate(ev)
}

/// Backend-independent entry point (also used by tests).
pub fn on_chord(model: &mut Model, chord: KeyChord) {
    // An open overlay captures input; otherwise the keymap resolves a command *for the
    // focused pane*, and anything it does not claim falls through to text entry.
    if !model.overlay_active() {
        if model.focus == termesh_ui::Pane::Terminal {
            // A focused shell swallows every chord except pane focus and the explicit
            // entry into client-owned copy mode. Ask the keymap rather than hardcoding
            // chords: rebinding either capability must move the reservation with it
            // (CONTRIBUTING.md, "one command surface").
            if let Some(command) = model
                .keymap
                .resolve(&chord, termesh_core::input::KeyContext::Terminal)
                .filter(is_reserved_terminal_command)
                .cloned()
            {
                model.dispatch(command);
                return;
            }
            // Scrollback before anything that could swallow it. A running process
            // otherwise takes every remaining chord, which is what left the output of
            // a failing `mvn test` unreachable while it was still running.
            if let Some(command @ (Command::TerminalScrollUp | Command::TerminalScrollDown)) =
                model.keymap.resolve(&chord, termesh_core::input::KeyContext::Terminal).cloned()
            {
                model.dispatch(command);
                return;
            }
            if model.active_terminal_has_running_task() {
                if let Some(command @ Command::Action(termesh_core::Action::TaskCancel)) =
                    model.keymap.resolve(&chord, termesh_core::input::KeyContext::Terminal).cloned()
                {
                    model.dispatch(command);
                    return;
                }
            }
            if model.terminal_copy_mode() {
                if let Some(command) = model
                    .keymap
                    .resolve(&chord, termesh_core::input::KeyContext::Terminal)
                    .filter(is_terminal_copy_command)
                    .cloned()
                {
                    model.dispatch(command);
                }
                return;
            }
            if model.terminal_accepts_input() {
                model.type_terminal_chord(chord);
                return;
            }
        }
        if let Some(cmd) = model.keymap.resolve(&chord, model.key_context()).cloned() {
            model.dispatch(cmd);
        } else if let Some(c) = text_input(model, &chord) {
            model.type_char(c);
        }
        return;
    }
    match model.overlays.last() {
        Some(Overlay::Palette(_)) => palette_chord(model, chord),
        Some(Overlay::Help(_)) => help_chord(model, chord),
        Some(Overlay::Prompt(_)) => prompt_chord(model, chord),
        Some(Overlay::Search(_)) => search_chord(model, chord),
        Some(Overlay::Tasks(_)) => task_chord(model, chord),
        Some(Overlay::AgentModes(_)) => agent_mode_chord(model, chord),
        Some(Overlay::Problems(_)) => problems_chord(model, chord),
        Some(Overlay::GitStatus(_)) => git_status_chord(model, chord),
        Some(Overlay::GitDiff(_)) => git_diff_chord(model, chord),
        Some(Overlay::GitBranches(_)) => git_branches_chord(model, chord),
        Some(Overlay::Hover(_)) => hover_chord(model, chord),
        Some(Overlay::Completion(_)) => completion_chord(model, chord),
        Some(Overlay::CodeActions(_)) => code_actions_chord(model, chord),
        Some(Overlay::References(_)) => references_chord(model, chord),
        Some(Overlay::Symbols(_)) => symbols_chord(model, chord),
        Some(Overlay::DraftRecovery(_)) => draft_recovery_chord(model, chord),
        None => {}
    }
}

fn draft_recovery_chord(model: &mut Model, chord: KeyChord) {
    let Some(Overlay::DraftRecovery(recovery)) = model.overlays.last_mut() else { return };
    match chord.key {
        Key::Esc => {
            let previous_focus = recovery.previous_focus;
            model.overlays.pop();
            model.focus = previous_focus;
        }
        Key::Up => recovery.selected = recovery.selected.saturating_sub(1),
        Key::Down => {
            recovery.selected =
                (recovery.selected + 1).min(recovery.drafts.len().saturating_sub(1));
        }
        Key::Char(' ') if chord.mods == Mods::default() => {
            if let Some(chosen) = recovery.chosen.get_mut(recovery.selected) {
                *chosen = !*chosen;
            }
        }
        Key::Enter => model.accept_selected_recovery_drafts(),
        Key::Char('a') if chord.mods == Mods::default() => {
            model.dispatch(Command::Action(termesh_core::Action::WorkspaceRestoreDrafts));
        }
        Key::Char('d') if chord.mods == Mods::default() => model.discard_recovery_drafts(),
        _ => {}
    }
}

fn is_reserved_terminal_command(command: &&Command) -> bool {
    command.is_pane_focus()
        || matches!(
            command,
            Command::Action(
                termesh_core::Action::TerminalCopyMode | termesh_core::Action::HelpShow
            )
        )
}

fn is_terminal_copy_command(command: &&Command) -> bool {
    matches!(
        command,
        Command::TerminalCopyLeft
            | Command::TerminalCopyRight
            | Command::TerminalCopyUp
            | Command::TerminalCopyDown
            | Command::TerminalCopyExtendLeft
            | Command::TerminalCopyExtendRight
            | Command::TerminalCopyExtendUp
            | Command::TerminalCopyExtendDown
            | Command::TerminalCopyPageUp
            | Command::TerminalCopyPageDown
            | Command::TerminalCopyConfirm
            | Command::TerminalCopyCancel
    )
}

/// The character this chord types into a buffer, if it types one at all.
///
/// Literal text is the only input that is not a [`Command`]: there is no finite set of
/// "insert an x" actions to put in the registry. Everything else — motion, newline,
/// delete — stays a command so it remains remappable and reachable from one dispatch
/// path (CONTRIBUTING.md, "one command surface").
///
/// Modified chords are excluded so an unbound `Ctrl+K` types nothing rather than a stray
/// `k`, which is the sort of thing that silently corrupts a file.
fn text_input(model: &Model, chord: &KeyChord) -> Option<char> {
    if model.key_context() != termesh_core::input::KeyContext::Editor {
        return None;
    }
    match chord.key {
        Key::Char(c) if !chord.mods.ctrl && !chord.mods.alt => Some(c),
        _ => None,
    }
}

/// Input for the create/rename/delete overlay. Esc always cancels without touching disk.
fn prompt_chord(model: &mut Model, chord: KeyChord) {
    let Some(Overlay::Prompt(prompt)) = model.overlays.last_mut() else { return };

    match chord.key {
        Key::Esc => {
            model.overlays.pop();
        }
        Key::Enter => {
            // Pop first so the model never confirms against a live overlay.
            let Some(Overlay::Prompt(prompt)) = model.overlays.pop() else { return };
            model.confirm_prompt(prompt);
        }
        Key::Backspace if prompt.takes_input() => {
            prompt.input.pop();
        }
        Key::Char(c) if prompt.takes_input() && !chord.mods.ctrl && !chord.mods.alt => {
            prompt.input.push(c);
        }
        _ => {}
    }
}

enum Outcome {
    Consumed,
    Close,
    Invoke(Option<termesh_core::Action>),
}

fn palette_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::Palette(p)) = model.overlays.last_mut() else {
            return;
        };
        match chord.key {
            Key::Esc => Outcome::Close,
            Key::Enter => Outcome::Invoke(p.selected_action()),
            Key::Up => {
                p.move_up();
                Outcome::Consumed
            }
            Key::Down => {
                p.move_down();
                Outcome::Consumed
            }
            Key::Backspace => {
                p.pop_char();
                Outcome::Consumed
            }
            Key::Char(c) if !chord.mods.ctrl && !chord.mods.alt => {
                p.push_char(c);
                Outcome::Consumed
            }
            _ => Outcome::Consumed,
        }
    };
    match outcome {
        Outcome::Consumed => {}
        Outcome::Close => {
            model.overlays.pop();
        }
        Outcome::Invoke(action) => {
            model.overlays.pop();
            if let Some(a) = action {
                model.dispatch(Command::Action(a));
            }
        }
    }
}

fn help_chord(model: &mut Model, chord: KeyChord) {
    let close = {
        let Some(Overlay::Help(help)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => Some(help.previous_focus),
            Key::Up => {
                help.scroll_by(-1);
                None
            }
            Key::Down => {
                help.scroll_by(1);
                None
            }
            Key::PageUp => {
                help.scroll_by(-10);
                None
            }
            Key::PageDown => {
                help.scroll_by(10);
                None
            }
            Key::Backspace => {
                help.pop_char();
                None
            }
            Key::Char(character) if !chord.mods.ctrl && !chord.mods.alt => {
                help.push_char(character);
                None
            }
            _ => None,
        }
    };
    if let Some(previous_focus) = close {
        model.overlays.pop();
        model.focus = previous_focus;
    }
}

enum SearchOutcome {
    Consumed,
    QueryChanged,
    SelectionChanged,
    Close(termesh_ui::Pane),
    Open {
        path: std::path::PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        previous_focus: termesh_ui::Pane,
    },
}

fn search_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::Search(search)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => SearchOutcome::Close(search.previous_focus),
            Key::Enter => match search.selected().cloned() {
                Some(found) => SearchOutcome::Open {
                    path: found.path,
                    line: found.line,
                    column: found.column,
                    previous_focus: search.previous_focus,
                },
                None => SearchOutcome::Consumed,
            },
            Key::Up => {
                search.move_up();
                SearchOutcome::SelectionChanged
            }
            Key::Down => {
                search.move_down();
                SearchOutcome::SelectionChanged
            }
            Key::Backspace => {
                search.pop_char();
                SearchOutcome::QueryChanged
            }
            Key::Char(value) if !chord.mods.ctrl && !chord.mods.alt => {
                search.push_char(value);
                SearchOutcome::QueryChanged
            }
            _ => SearchOutcome::Consumed,
        }
    };
    match outcome {
        SearchOutcome::Consumed => {}
        SearchOutcome::QueryChanged => model.search_query_changed(),
        SearchOutcome::SelectionChanged => {
            model.request_selected_preview();
        }
        SearchOutcome::Close(previous_focus) => {
            model.overlays.pop();
            model.focus = previous_focus;
            model.cancel_search();
        }
        SearchOutcome::Open { path, line, column, previous_focus } => {
            model.overlays.pop();
            model.focus = previous_focus;
            model.cancel_search();
            match (line, column) {
                (Some(line), Some(column)) => model.open_file_at(path, line, column),
                _ => model.open_file(path),
            }
        }
    }
}

fn agent_mode_chord(model: &mut Model, chord: KeyChord) {
    let chosen = {
        let Some(Overlay::AgentModes(picker)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => {
                model.overlays.pop();
                return;
            }
            Key::Up => {
                picker.move_up();
                None
            }
            Key::Down => {
                picker.move_down();
                None
            }
            Key::Enter => picker.selected().map(|mode| mode.id.clone()),
            _ => None,
        }
    };
    if let Some(mode) = chosen {
        model.overlays.pop();
        model.set_agent_mode(mode);
    }
}

fn task_chord(model: &mut Model, chord: KeyChord) {
    let selected = {
        let Some(Overlay::Tasks(picker)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => {
                model.overlays.pop();
                return;
            }
            Key::Up => {
                picker.move_up();
                None
            }
            Key::Down => {
                picker.move_down();
                None
            }
            Key::Enter => picker.selected().cloned(),
            _ => None,
        }
    };
    if let Some(task) = selected {
        model.overlays.pop();
        model.run_task(task);
    }
}

fn problems_chord(model: &mut Model, chord: KeyChord) {
    let selected = {
        let Some(Overlay::Problems(problems)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => {
                model.overlays.pop();
                return;
            }
            Key::Up => {
                problems.move_up();
                None
            }
            Key::Down => {
                problems.move_down();
                None
            }
            Key::Enter => problems.selected().cloned(),
            _ => None,
        }
    };
    if let Some(problem) = selected {
        model.overlays.pop();
        model.navigate_problem(problem.navigation_problem());
    }
}

fn hover_chord(model: &mut Model, chord: KeyChord) {
    if chord.key != Key::Esc {
        return;
    }
    let Some(Overlay::Hover(hover)) = model.overlays.pop() else { return };
    model.focus = hover.previous_focus;
}

fn completion_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::Completion(completion)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => Some((completion.previous_focus, None)),
            Key::Up => {
                completion.move_up();
                None
            }
            Key::Down => {
                completion.move_down();
                None
            }
            Key::Enter => Some((
                completion.previous_focus,
                completion.items.get(completion.selected).cloned(),
            )),
            _ => None,
        }
    };
    if let Some((previous_focus, completion)) = outcome {
        model.overlays.pop();
        model.focus = previous_focus;
        if let Some(completion) = completion {
            model.accept_completion(completion);
        }
    }
}

fn code_actions_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::CodeActions(actions)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => Some((actions.previous_focus, None)),
            Key::Up => {
                actions.move_up();
                None
            }
            Key::Down => {
                actions.move_down();
                None
            }
            Key::Enter => {
                Some((actions.previous_focus, actions.actions.get(actions.selected).cloned()))
            }
            _ => None,
        }
    };
    if let Some((previous_focus, action)) = outcome {
        model.overlays.pop();
        model.focus = previous_focus;
        if let Some(action) = action {
            model.accept_code_action(action);
        }
    }
}

fn references_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::References(references)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => Some((references.previous_focus, None)),
            Key::Up => {
                references.move_up();
                None
            }
            Key::Down => {
                references.move_down();
                None
            }
            Key::Enter => Some((
                references.previous_focus,
                references.locations.get(references.selected).cloned(),
            )),
            _ => None,
        }
    };
    if let Some((previous_focus, location)) = outcome {
        model.overlays.pop();
        model.focus = previous_focus;
        if let Some(location) = location {
            model.open_lsp_location(location);
        }
    }
}

fn symbols_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::Symbols(symbols)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => Some((symbols.previous_focus, None)),
            Key::Up => {
                symbols.move_up();
                None
            }
            Key::Down => {
                symbols.move_down();
                None
            }
            Key::Enter => Some((
                symbols.previous_focus,
                symbols.rows.get(symbols.selected).map(|row| row.location.clone()),
            )),
            _ => None,
        }
    };
    if let Some((previous_focus, location)) = outcome {
        model.overlays.pop();
        model.focus = previous_focus;
        if let Some(location) = location {
            model.open_lsp_location(location);
        }
    }
}

fn git_status_chord(model: &mut Model, chord: KeyChord) {
    if chord.mods.ctrl && !chord.mods.alt && matches!(chord.key, Key::Char('g')) {
        model.dispatch(Command::Action(termesh_core::Action::GitStage));
        return;
    }
    let action = {
        let Some(Overlay::GitStatus(status)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => {
                let previous_focus = status.previous_focus;
                model.overlays.pop();
                model.focus = previous_focus;
                return;
            }
            Key::Up => {
                status.move_up();
                None
            }
            Key::Down => {
                status.move_down();
                None
            }
            Key::Enter => {
                model.open_selected_git_diff();
                return;
            }
            Key::Char('s') if chord.mods == Mods::default() => Some(termesh_core::Action::GitStage),
            Key::Char('u') if chord.mods == Mods::default() => {
                Some(termesh_core::Action::GitUnstage)
            }
            Key::Char('c') if chord.mods == Mods::default() => {
                Some(termesh_core::Action::GitCommit)
            }
            Key::Char('b') if chord.mods == Mods::default() => {
                Some(termesh_core::Action::GitBranchCheckout)
            }
            _ => None,
        }
    };
    if let Some(action) = action {
        model.dispatch(Command::Action(action));
    }
}

fn git_diff_chord(model: &mut Model, chord: KeyChord) {
    let Some(Overlay::GitDiff(diff)) = model.overlays.last_mut() else { return };
    match chord.key {
        Key::Esc => {
            model.overlays.pop();
        }
        Key::Up => diff.scroll_up(1),
        Key::Down => diff.scroll_down(1),
        Key::PageUp => diff.scroll_up(10),
        Key::PageDown => diff.scroll_down(10),
        _ => {}
    }
}

fn git_branches_chord(model: &mut Model, chord: KeyChord) {
    let outcome = {
        let Some(Overlay::GitBranches(branches)) = model.overlays.last_mut() else { return };
        match chord.key {
            Key::Esc => {
                let previous_focus = branches.previous_focus;
                model.overlays.pop();
                model.focus = previous_focus;
                return;
            }
            Key::Up => {
                branches.move_up();
                None
            }
            Key::Down => {
                branches.move_down();
                None
            }
            Key::Enter => branches.selected().map(|branch| branch.name.clone()),
            _ => None,
        }
    };
    if let Some(branch) = outcome {
        model.overlays.pop();
        model.queue_git_operation(termesh_core::GitOperation::Checkout { branch });
    }
}

fn translate(ev: KeyEvent) -> Option<KeyChord> {
    let mut mods = Mods {
        ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
        alt: ev.modifiers.contains(KeyModifiers::ALT),
        shift: ev.modifiers.contains(KeyModifiers::SHIFT),
    };
    let key = match ev.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => {
            mods.shift = false; // BackTab already implies Shift+Tab
            Key::BackTab
        }
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n) => Key::F(n),
        // Drop keys we don't model rather than folding them into a real key: mapping
        // them to Esc made any unmapped keypress dismiss the active overlay.
        _ => return None,
    };
    Some(KeyChord { key, mods })
}

#[cfg(test)]
mod tests {
    use super::*;
    use termesh_core::Command;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn release_events_are_dropped() {
        let mut ev = press(KeyCode::Char('a'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(translate_key(ev), None);
        assert!(translate_key(press(KeyCode::Char('a'))).is_some(), "press still translates");
    }

    #[test]
    fn unmodelled_keys_are_dropped_not_folded_into_esc() {
        // Regression: these used to translate to Key::Esc, so any unmapped keypress
        // dismissed the open overlay.
        for code in [KeyCode::Insert, KeyCode::Null, KeyCode::Menu] {
            assert_eq!(translate_key(press(code)), None, "{code:?} should be dropped");
        }
    }

    #[test]
    fn unmodelled_keypress_leaves_an_open_overlay_alone() {
        let mut m = Model::new();
        m.dispatch(Command::OpenPalette);
        if let Some(chord) = translate_key(press(KeyCode::Insert)) {
            on_chord(&mut m, chord);
        }
        assert!(m.overlay_active(), "an unmapped key must not close the palette");
    }

    #[test]
    fn modifiers_survive_translation() {
        let ev = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(translate_key(ev), Some(KeyChord::ctrl(Key::Char('p'))));
    }

    #[test]
    fn the_reserved_terminal_chord_reaches_the_terminal() {
        let mut model = Model::new();
        let ev = KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE);

        on_chord(&mut model, translate_key(ev).expect("F6 translates"));

        assert_eq!(model.focus, termesh_ui::Pane::Terminal, "F6 should open a shell");
    }

    /// Regression: Ctrl+Space was rewritten to Ctrl+` at this boundary to chase a chord
    /// no emulator sends. It only cost us Ctrl+Space, which macOS intercepts anyway.
    #[test]
    fn ctrl_space_is_not_rewritten_into_another_chord() {
        let ev = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(translate_key(ev), Some(KeyChord::ctrl(Key::Char(' '))));
    }

    /// The way out of a focused shell must follow the keymap, not a hardcoded chord.
    #[test]
    fn rebinding_terminal_focus_also_rebinds_the_escape_from_the_terminal() {
        let mut model = Model::new();
        model.keymap.bind(
            KeyChord::ctrl(Key::Char('e')),
            Command::Action(termesh_core::Action::TerminalFocus),
        );
        on_chord(&mut model, KeyChord::plain(Key::F(6)));
        assert_eq!(model.focus, termesh_ui::Pane::Terminal, "precondition: in the terminal");

        on_chord(&mut model, KeyChord::ctrl(Key::Char('e')));

        assert_ne!(model.focus, termesh_ui::Pane::Terminal, "the rebound chord must escape");
    }

    #[test]
    fn backtab_does_not_double_count_shift() {
        let ev = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        let chord = translate_key(ev).unwrap();
        assert_eq!(chord.key, Key::BackTab);
        assert!(!chord.mods.shift, "BackTab already implies Shift+Tab");
    }
}
