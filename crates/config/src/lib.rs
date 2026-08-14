//! Global/workspace config + keymaps. Compiled defaults remain the source of truth;
//! `config.toml` and `keymap.toml` layer user choices over them (ADR-0014 §3).
#![forbid(unsafe_code)]

pub mod agents;
pub mod keymap_file;
mod migrate;
pub mod settings;
pub use agents::{AgentConfig, AgentsConfig, ConfigError};
pub use keymap_file::apply_keymap_file;
pub use settings::{Autosave, ConfigDiagnostic, Settings, ThemeChoice};

use std::collections::HashMap;

use termesh_core::input::{Key, KeyChord, KeyContext};
use termesh_core::{Action, Command};

/// Maps key chords to commands, per context. Backend-agnostic (no crossterm dependency).
///
/// Contexts exist because one chord legitimately means different things in different
/// panes: `Down` is "next tree row" in the explorer and "next line" in a buffer. Phase 02
/// resolved that with a focus check inside the command handler, which does not scale past
/// two consumers — so the choice moved here, into resolution.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    map: HashMap<(KeyContext, KeyChord), Command>,
}

impl Keymap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a chord that applies regardless of focus.
    pub fn bind(&mut self, chord: KeyChord, cmd: Command) -> &mut Self {
        self.bind_in(KeyContext::Global, chord, cmd)
    }

    /// Bind a chord that applies only while `context` has focus.
    pub fn bind_in(&mut self, context: KeyContext, chord: KeyChord, cmd: Command) -> &mut Self {
        self.map.insert((context, chord), cmd);
        self
    }

    /// Resolve a chord for the currently focused context.
    ///
    /// The focused context wins over [`KeyContext::Global`], so a pane can shadow a
    /// global binding; anything it does not claim still falls through, which is what
    /// keeps `Ctrl+S` and `Ctrl+P` working everywhere.
    pub fn resolve(&self, chord: &KeyChord, context: KeyContext) -> Option<&Command> {
        self.map.get(&(context, *chord)).or_else(|| self.map.get(&(KeyContext::Global, *chord)))
    }

    /// Reverse lookup: a chord bound to a command, for palette and menu hints.
    ///
    /// Prefers a global binding so the hint shown in the palette is one that works from
    /// wherever the palette was opened.
    pub fn chord_for(&self, cmd: &Command) -> Option<KeyChord> {
        let mut best: Option<(KeyContext, KeyChord)> = None;
        for ((context, chord), bound) in &self.map {
            if bound != cmd {
                continue;
            }
            let better = match best {
                None => true,
                Some((KeyContext::Global, _)) => false,
                Some(_) => *context == KeyContext::Global,
            };
            if better {
                best = Some((*context, *chord));
            }
        }
        best.map(|(_, chord)| chord)
    }

    /// Every binding, for auditing the keymap as a whole.
    pub fn bindings(&self) -> impl Iterator<Item = (KeyContext, KeyChord, &Command)> {
        self.map.iter().map(|((context, chord), cmd)| (*context, *chord, cmd))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[derive(Default)]
struct DefaultBindings(Vec<(KeyContext, KeyChord, Command)>);

impl DefaultBindings {
    fn bind(&mut self, chord: KeyChord, command: Command) -> &mut Self {
        self.bind_in(KeyContext::Global, chord, command)
    }

    fn bind_in(&mut self, context: KeyContext, chord: KeyChord, command: Command) -> &mut Self {
        self.0.push((context, chord, command));
        self
    }
}

/// The default, mainstream-IDE-flavored keymap (ARCHITECTURE.md §6.3).
pub fn default_keymap() -> Keymap {
    let mut keymap = Keymap::new();
    for (context, chord, command) in default_bindings() {
        keymap.bind_in(context, chord, command);
    }
    keymap
}

/// Every default binding before it enters the lookup map.
///
/// Keeping this flat source is important: [`Keymap`] intentionally overwrites on insert,
/// so inspecting the finished map cannot reveal that two defaults claimed one chord.
fn default_bindings() -> Vec<(KeyContext, KeyChord, Command)> {
    // `Command::Action` shadows the `Action` enum under a glob, so commands are imported
    // by name and registry actions stay qualified.
    use Command::{
        CloseOverlay, EditorBackspace, EditorCursorDown, EditorCursorLeft, EditorCursorRight,
        EditorCursorUp, EditorDeleteForward, EditorInsertNewline, EditorLineEnd, EditorLineStart,
        EditorRedo, EditorUndo, ExplorerCollapseOrParent, ExplorerNext, ExplorerPrev,
        ExplorerToggle, FocusNext, FocusPrev, GrowBottom, GrowSidebar, OpenPalette, Quit,
        ShrinkBottom, ShrinkSidebar,
    };
    use KeyContext::{Agent, Editor, Project, Terminal};

    let mut k = DefaultBindings::default();

    // --- shell controls, everywhere ---
    k.bind(KeyChord::ctrl(Key::Char('p')), Command::Action(Action::FileOpen))
        .bind(KeyChord::plain(Key::F(10)), OpenPalette)
        // Alias, same reasoning as `Alt+I` for the agent prompt below: the function key
        // carries the portability promise, but on macOS `F9`–`F12` are media keys unless
        // the user turns on "Use F1, F2, etc. as standard function keys", so the two
        // most-reached-for shortcuts each get a route that needs no `fn` and no setting.
        .bind(KeyChord::alt(Key::Char('p')), OpenPalette)
        .bind(KeyChord::plain(Key::F(11)), Command::Action(Action::HelpShow))
        .bind(KeyChord::alt(Key::Char('h')), Command::Action(Action::HelpShow))
        .bind(KeyChord::ctrl(Key::Char('q')), Quit)
        .bind(KeyChord::plain(Key::Esc), CloseOverlay)
        .bind(KeyChord::plain(Key::Tab), FocusNext)
        .bind(KeyChord::plain(Key::BackTab), FocusPrev)
        .bind(KeyChord::alt(Key::Left), ShrinkSidebar)
        .bind(KeyChord::alt(Key::Right), GrowSidebar)
        .bind(KeyChord::alt(Key::Up), ShrinkBottom)
        .bind(KeyChord::alt(Key::Down), GrowBottom);

    // --- pane navigation ---
    //
    // Every pane gets a direct binding, and these are the only chords a focused shell
    // still honours (ADR-0008 §3). Cycling alone cannot do the job: the Terminal captures
    // Tab to give the shell a real keyboard, so a Terminal inside the Tab ring would be a
    // one-way door — you could Tab in and never Tab out. `FocusNext` therefore skips it
    // and F6 toggles it, the way an IDE panel works.
    //
    // Function keys, not Ctrl chords, and deliberately so. The chord has to survive a
    // shell *and* whatever we are hosting under Tier 0, and between them they claim
    // nearly every Ctrl+letter — Claude Code alone binds A-E, G, J-L, N-P, R-X. Ctrl+`
    // is not an option either: it would have to arrive as NUL (0x60 & 0x1f), most
    // emulators send nothing for it, and NUL is indistinguishable from Ctrl+Space, which
    // macOS claims for input-source switching.
    k.bind(KeyChord::plain(Key::F(1)), Command::Action(Action::FocusProject))
        .bind(KeyChord::plain(Key::F(2)), Command::Action(Action::FocusEditor))
        .bind(KeyChord::plain(Key::F(6)), Command::Action(Action::TerminalFocus))
        .bind(KeyChord::plain(Key::F(7)), Command::Action(Action::FocusAgent));

    // --- file explorer ---
    k.bind_in(Project, KeyChord::plain(Key::Down), ExplorerNext)
        .bind_in(Project, KeyChord::plain(Key::Up), ExplorerPrev)
        .bind_in(Project, KeyChord::plain(Key::Enter), ExplorerToggle)
        .bind_in(Project, KeyChord::plain(Key::Right), ExplorerToggle)
        .bind_in(Project, KeyChord::plain(Key::Left), ExplorerCollapseOrParent)
        .bind_in(Project, KeyChord::plain(Key::Delete), Command::Action(Action::FileDelete));

    // --- editor ---
    //
    // These shadow the explorer's arrow bindings rather than competing with them, which
    // is the whole reason contexts exist. Plain character entry is deliberately absent:
    // it is not a command, it is text, and it is handled as the fallthrough.
    k.bind_in(Editor, KeyChord::plain(Key::Left), EditorCursorLeft)
        .bind_in(Editor, KeyChord::plain(Key::Right), EditorCursorRight)
        .bind_in(Editor, KeyChord::plain(Key::Up), EditorCursorUp)
        .bind_in(Editor, KeyChord::plain(Key::Down), EditorCursorDown)
        .bind_in(Editor, KeyChord::plain(Key::Home), EditorLineStart)
        .bind_in(Editor, KeyChord::plain(Key::End), EditorLineEnd)
        .bind_in(Editor, KeyChord::plain(Key::Enter), EditorInsertNewline)
        .bind_in(Editor, KeyChord::plain(Key::Backspace), EditorBackspace)
        .bind_in(Editor, KeyChord::plain(Key::Delete), EditorDeleteForward);

    // --- agent review (ARCHITECTURE.md §6.3) ---
    //
    // Single letters, which only works because contexts keep them off the editor's text
    // input: `a` types an 'a' in a buffer and accepts a proposal in the agent pane.
    k.bind_in(Agent, KeyChord::plain(Key::Enter), Command::Action(Action::AgentPrompt))
        .bind_in(
            Agent,
            KeyChord::plain(Key::Char('a')),
            Command::Action(Action::AgentProposalAccept),
        )
        .bind_in(
            Agent,
            KeyChord::plain(Key::Char('r')),
            Command::Action(Action::AgentProposalReject),
        )
        .bind_in(Agent, KeyChord::plain(Key::Up), Command::AgentScrollUp)
        .bind_in(Agent, KeyChord::plain(Key::Down), Command::AgentScrollDown)
        .bind_in(Agent, KeyChord::plain(Key::PageUp), Command::AgentScrollUp)
        .bind_in(Agent, KeyChord::plain(Key::PageDown), Command::AgentScrollDown)
        .bind_in(Agent, KeyChord::plain(Key::Char('y')), Command::AgentAllowOnce)
        .bind_in(Agent, KeyChord::plain(Key::Char('A')), Command::AgentAllowAlways)
        .bind_in(Agent, KeyChord::plain(Key::Char('n')), Command::AgentDeny);

    // A focused terminal normally owns its keyboard. Alt+C is the one action reserved
    // specifically to enter client-owned copy mode; app input routing protects it from
    // reaching the PTY.
    k.bind_in(Terminal, KeyChord::alt(Key::Char('c')), Command::Action(Action::TerminalCopyMode));

    // --- terminal copy mode ---
    // Normal terminal focus bypasses the keymap. These bindings are resolved only
    // after the user explicitly enters copy mode.
    k.bind_in(Terminal, KeyChord::plain(Key::Left), Command::TerminalCopyLeft)
        .bind_in(Terminal, KeyChord::plain(Key::Right), Command::TerminalCopyRight)
        .bind_in(Terminal, KeyChord::plain(Key::Up), Command::TerminalCopyUp)
        .bind_in(Terminal, KeyChord::plain(Key::Down), Command::TerminalCopyDown)
        .bind_in(Terminal, KeyChord::shift(Key::Left), Command::TerminalCopyExtendLeft)
        .bind_in(Terminal, KeyChord::shift(Key::Right), Command::TerminalCopyExtendRight)
        .bind_in(Terminal, KeyChord::shift(Key::Up), Command::TerminalCopyExtendUp)
        .bind_in(Terminal, KeyChord::shift(Key::Down), Command::TerminalCopyExtendDown)
        .bind_in(Terminal, KeyChord::plain(Key::PageUp), Command::TerminalCopyPageUp)
        .bind_in(Terminal, KeyChord::plain(Key::PageDown), Command::TerminalCopyPageDown)
        .bind_in(Terminal, KeyChord::plain(Key::Enter), Command::TerminalCopyConfirm)
        .bind_in(Terminal, KeyChord::plain(Key::Esc), Command::TerminalCopyCancel);

    // Scrollback, outside copy mode. Shift+PageUp/PageDown is what every terminal
    // emulator uses for this — tmux, iTerm2, GNOME Terminal, Windows Terminal — and
    // shells do not expect it, so it is safe to claim from a running process. Plain
    // PageUp still belongs to the process, and to copy mode when that is active.
    k.bind_in(Terminal, KeyChord::shift(Key::PageUp), Command::TerminalScrollUp).bind_in(
        Terminal,
        KeyChord::shift(Key::PageDown),
        Command::TerminalScrollDown,
    );

    // Find is buffer-scoped: Ctrl+F searches the *open file*, and only in the Editor.
    // Workspace search is F9 (bound below) rather than a Ctrl chord — Ctrl+Shift+F,
    // which would have been the natural pair, is undeliverable (ADR-0009).
    // Replace is Ctrl+R, not Ctrl+H: Ctrl+H is Backspace on the wire.
    k.bind_in(Editor, KeyChord::ctrl(Key::Char('f')), Command::EditorFind).bind_in(
        Editor,
        KeyChord::ctrl(Key::Char('r')),
        Command::EditorReplace,
    );
    // F3 rather than n/N, which would collide with typing.
    k.bind(KeyChord::plain(Key::F(3)), Command::EditorFindNext)
        .bind(KeyChord::shift(Key::F(3)), Command::EditorFindPrev);

    // Tabs are global: switching files is about the editor wherever you are looking.
    k.bind(KeyChord::ctrl(Key::Tab), Command::EditorNextTab)
        .bind(KeyChord::ctrl(Key::BackTab), Command::EditorPrevTab)
        .bind(KeyChord::ctrl(Key::Char('w')), Command::EditorCloseTab);

    // Undo/redo are global: they are about the active buffer wherever you are looking.
    k.bind(KeyChord::ctrl(Key::Char('z')), EditorUndo)
        .bind(KeyChord::ctrl(Key::Char('y')), EditorRedo);

    // --- feature actions ---
    k.bind(KeyChord::ctrl(Key::Char('s')), Command::Action(Action::FileSave))
        .bind(KeyChord::plain(Key::F(9)), Command::Action(Action::WorkspaceSearch))
        .bind(KeyChord::alt(Key::Char('f')), Command::Action(Action::WorkspaceSearch))
        .bind(KeyChord::plain(Key::F(5)), Command::Action(Action::TaskRun))
        .bind(KeyChord::shift(Key::F(5)), Command::Action(Action::TaskCancel))
        .bind(KeyChord::plain(Key::F(8)), Command::Action(Action::ProblemsNext))
        .bind(KeyChord::shift(Key::F(8)), Command::Action(Action::ProblemsPrevious))
        .bind(KeyChord::plain(Key::F(12)), Command::Action(Action::EditorGotoDefinition))
        .bind(KeyChord::shift(Key::F(12)), Command::Action(Action::LspReferences))
        .bind(KeyChord::alt(Key::Char('k')), Command::Action(Action::LspHover))
        .bind(KeyChord::alt(Key::Char('/')), Command::Action(Action::LspCompletion))
        .bind(KeyChord::alt(Key::Char('o')), Command::Action(Action::LspDocumentSymbols))
        .bind(KeyChord::alt(Key::Enter), Command::Action(Action::LspCodeAction))
        // Three routes on purpose. Alt is not a modifier every terminal delivers —
        // macOS Terminal.app sends Option as a compose key unless told otherwise — so a
        // function key carries the guarantee and Alt+I is the ergonomic alias.
        .bind(KeyChord::plain(Key::F(4)), Command::Action(Action::AgentPrompt))
        .bind(KeyChord::alt(Key::Char('i')), Command::Action(Action::AgentPrompt))
        .bind(KeyChord::ctrl(Key::Char('g')), Command::Action(Action::GitStage));

    k.0
}

#[cfg(test)]
const DELIBERATE_SHADOWS: &[(KeyContext, KeyChord)] =
    &[(KeyContext::Terminal, KeyChord::plain(Key::Esc))];

#[cfg(test)]
const PALETTE_ONLY: &[&str] = &[
    "file.new",
    "file.new_folder",
    "file.rename",
    "pane.split_right",
    "terminal.new",
    "terminal.run",
    "terminal.next",
    "terminal.previous",
    "terminal.restart",
    "terminal.close",
    "git.show",
    "git.unstage",
    "git.commit",
    "git.branch.checkout",
    "git.fetch",
    "git.pull",
    "git.push",
    "problems.show",
    "lsp.symbols.workspace",
    "lsp.rename",
    "lsp.format",
    "lsp.restart",
    "workspace.restore_drafts",
    // Protocol/tool vocabulary, not standalone user operations. They remain listed so
    // registry growth cannot make them disappear from the reachability audit.
    "editor.apply_transaction",
    "agent.session.new",
    "config.reload",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use termesh_core::input::{Key, KeyChord, KeyContext};
    use termesh_core::{ActionRegistry, Command};

    #[test]
    fn no_two_default_bindings_claim_the_same_chord_in_a_context() {
        // The map is a HashMap and `insert` overwrites, so a collision is silent:
        // the second binding wins and the first vanishes with no error anywhere.
        let mut seen: HashMap<(KeyContext, KeyChord), Command> = HashMap::new();
        let mut collisions = Vec::new();
        for (context, chord, command) in default_bindings() {
            if let Some(existing) = seen.insert((context, chord), command.clone()) {
                collisions.push(format!("{context:?} {chord:?}: {existing:?} vs {command:?}"));
            }
        }
        assert!(collisions.is_empty(), "colliding defaults: {collisions:#?}");
    }

    #[test]
    fn a_pane_binding_that_shadows_a_global_one_is_deliberate() {
        // Shadowing is legal and useful (Down means different things per pane), but an
        // accidental shadow of Ctrl+S or F10 is a bug. The allow-list makes each one a choice.
        for (context, chord, _) in default_bindings() {
            if context == KeyContext::Global {
                continue;
            }
            if default_keymap().resolve(&chord, KeyContext::Global).is_some() {
                assert!(
                    DELIBERATE_SHADOWS.contains(&(context, chord)),
                    "{context:?} {chord:?} shadows a global binding without being listed"
                );
            }
        }
    }

    #[test]
    fn every_registered_action_is_reachable() {
        // Palette-only is a legitimate choice; unreachable is not. terminal.copy_mode was
        // neither bound nor listed, and the palette cannot open from a focused terminal.
        for action in ActionRegistry::with_defaults().actions() {
            let bound = default_keymap().chord_for(&Command::Action(action.clone())).is_some();
            assert!(
                bound || PALETTE_ONLY.contains(&action.id()),
                "{} has no binding and is not listed as palette-only",
                action.id()
            );
        }
    }

    /// A chord the terminal cannot deliver does not fail loudly — it silently triggers
    /// whatever the colliding key is bound to, which looks exactly like a broken feature.
    /// `Ctrl+I` bound to the agent prompt cycled focus instead, because it *is* `Tab`.
    #[test]
    fn no_default_binding_is_one_the_terminal_cannot_deliver() {
        let k = default_keymap();
        let bad: Vec<String> = k
            .bindings()
            .filter(|(_, chord, _)| chord.is_terminal_ambiguous())
            .map(|(context, chord, cmd)| format!("{chord} in {context:?} -> {cmd:?}"))
            .collect();
        assert!(bad.is_empty(), "these cannot be distinguished from another key: {bad:?}");
    }

    #[test]
    fn the_ambiguous_chords_are_the_ones_that_share_a_control_byte() {
        for c in ['i', 'm', 'j', 'h', '['] {
            assert!(KeyChord::ctrl(Key::Char(c)).is_terminal_ambiguous(), "Ctrl+{c}");
        }
        for c in ['p', 'q', 's', 'f', 'r', 'z'] {
            assert!(!KeyChord::ctrl(Key::Char(c)).is_terminal_ambiguous(), "Ctrl+{c}");
        }
        // Alt+I is a different escape sequence entirely, so it is fine.
        assert!(!KeyChord::alt(Key::Char('i')).is_terminal_ambiguous());
    }

    #[test]
    fn asking_the_agent_is_reachable_without_ctrl_i() {
        let k = default_keymap();
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('i')), KeyContext::Global),
            Some(&Command::Action(Action::AgentPrompt))
        );
        // And from the pane itself, where you are already looking.
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::Enter), KeyContext::Agent),
            Some(&Command::Action(Action::AgentPrompt))
        );
    }

    #[test]
    fn phase_05_shortcuts_match_the_design() {
        let k = default_keymap();
        assert_eq!(
            k.resolve(&KeyChord::ctrl(Key::Char('p')), KeyContext::Global),
            Some(&Command::Action(Action::FileOpen))
        );
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::F(10)), KeyContext::Global),
            Some(&Command::OpenPalette)
        );
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::F(9)), KeyContext::Global),
            Some(&Command::Action(Action::WorkspaceSearch))
        );
        assert_eq!(k.resolve(&KeyChord::ctrl_shift(Key::Char('p')), KeyContext::Global), None);
        assert_eq!(k.resolve(&KeyChord::ctrl_shift(Key::Char('f')), KeyContext::Global), None);
    }

    #[test]
    fn phase_06_keeps_only_the_existing_git_shortcut() {
        let keymap = default_keymap();
        assert_eq!(
            keymap.resolve(&KeyChord::ctrl(Key::Char('g')), KeyContext::Global),
            Some(&Command::Action(Action::GitStage))
        );
        let git_bindings = keymap
            .bindings()
            .filter(|(_, _, command)| {
                matches!(
                    command,
                    Command::Action(
                        Action::GitShow
                            | Action::GitStage
                            | Action::GitUnstage
                            | Action::GitCommit
                            | Action::GitBranchCheckout
                            | Action::GitFetch
                            | Action::GitPull
                            | Action::GitPush
                    )
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(git_bindings.len(), 1);
    }

    #[test]
    fn language_actions_have_reachable_default_chords() {
        let k = default_keymap();
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('k')), KeyContext::Global),
            Some(&Command::Action(Action::LspHover))
        );
        assert_eq!(
            k.resolve(&KeyChord::shift(Key::F(12)), KeyContext::Global),
            Some(&Command::Action(Action::LspReferences))
        );
    }

    /// `F9`/`F10` are media keys on a stock Mac. The function key stays the portable
    /// guarantee, but neither shortcut may be reachable *only* through it.
    #[test]
    fn workspace_search_and_the_palette_each_have_a_route_that_needs_no_function_key() {
        let k = default_keymap();
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('f')), KeyContext::Global),
            Some(&Command::Action(Action::WorkspaceSearch))
        );
        assert_eq!(
            k.resolve(&KeyChord::alt(Key::Char('p')), KeyContext::Global),
            Some(&Command::OpenPalette)
        );
    }

    #[test]
    fn hints_are_discoverable_via_reverse_lookup() {
        let k = default_keymap();
        let chord = k.chord_for(&Command::Quit).unwrap();
        assert_eq!(chord.to_string(), "Ctrl+Q");
    }

    /// The reason contexts exist: one chord, two meanings, decided by focus.
    #[test]
    fn the_same_arrow_means_different_things_in_different_panes() {
        let k = default_keymap();
        let down = KeyChord::plain(Key::Down);
        assert_eq!(k.resolve(&down, KeyContext::Project), Some(&Command::ExplorerNext));
        assert_eq!(k.resolve(&down, KeyContext::Editor), Some(&Command::EditorCursorDown));
    }

    #[test]
    fn enter_and_delete_are_context_dependent_too() {
        let k = default_keymap();
        for (chord, project, editor) in [
            (KeyChord::plain(Key::Enter), Command::ExplorerToggle, Command::EditorInsertNewline),
            (
                KeyChord::plain(Key::Delete),
                Command::Action(Action::FileDelete),
                Command::EditorDeleteForward,
            ),
        ] {
            assert_eq!(k.resolve(&chord, KeyContext::Project), Some(&project));
            assert_eq!(k.resolve(&chord, KeyContext::Editor), Some(&editor));
        }
    }

    #[test]
    fn global_bindings_reach_every_context() {
        let k = default_keymap();
        let save = KeyChord::ctrl(Key::Char('s'));
        for context in [KeyContext::Global, KeyContext::Project, KeyContext::Editor] {
            assert_eq!(
                k.resolve(&save, context),
                Some(&Command::Action(Action::FileSave)),
                "Ctrl+S must work in {context:?}"
            );
        }
    }

    #[test]
    fn a_context_binding_does_not_leak_into_other_contexts() {
        let k = default_keymap();
        // Backspace is text editing; in the explorer it must do nothing at all rather
        // than falling through to something surprising.
        assert_eq!(k.resolve(&KeyChord::plain(Key::Backspace), KeyContext::Project), None);
    }

    #[test]
    fn a_pane_binding_shadows_a_global_one() {
        let mut k = Keymap::new();
        k.bind(KeyChord::plain(Key::Enter), Command::FocusNext);
        k.bind_in(KeyContext::Editor, KeyChord::plain(Key::Enter), Command::EditorInsertNewline);

        let enter = KeyChord::plain(Key::Enter);
        assert_eq!(k.resolve(&enter, KeyContext::Editor), Some(&Command::EditorInsertNewline));
        assert_eq!(k.resolve(&enter, KeyContext::Project), Some(&Command::FocusNext));
    }

    #[test]
    fn palette_hints_prefer_a_binding_that_works_anywhere() {
        let mut k = Keymap::new();
        k.bind_in(KeyContext::Editor, KeyChord::plain(Key::F(5)), Command::Quit);
        k.bind(KeyChord::ctrl(Key::Char('q')), Command::Quit);
        assert_eq!(
            k.chord_for(&Command::Quit),
            Some(KeyChord::ctrl(Key::Char('q'))),
            "a hint shown in the palette should work from wherever it was opened"
        );
    }

    #[test]
    fn terminal_focus_reaches_the_action_globally() {
        let k = default_keymap();
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::F(6)), KeyContext::Editor),
            Some(&Command::Action(Action::TerminalFocus)),
            "F6 should reach the terminal from any pane"
        );
    }

    /// Ctrl+T was tried as an ergonomic alias and withdrawn: a focused shell swallows
    /// everything but the reserved chords, and Ctrl+T belongs to the programs we exist
    /// to host — Claude Code binds it to its task list. Nothing may reclaim it.
    #[test]
    fn no_ctrl_letter_is_reserved_for_pane_navigation() {
        let k = default_keymap();
        for (_, chord, command) in k.bindings() {
            if command.is_pane_focus() {
                assert!(
                    !chord.mods.ctrl,
                    "{chord} -> {command:?}: pane navigation must not steal a shell chord"
                );
            }
        }
    }

    /// Every pane must be reachable directly, from anywhere — including from inside a
    /// terminal, which is the one pane that swallows the Tab cycle.
    #[test]
    fn every_pane_has_a_direct_focus_binding() {
        let k = default_keymap();
        for action in
            [Action::FocusProject, Action::FocusEditor, Action::FocusAgent, Action::TerminalFocus]
        {
            let chord = k
                .chord_for(&Command::Action(action.clone()))
                .unwrap_or_else(|| panic!("{action:?} needs a binding"));
            assert_eq!(
                k.resolve(&chord, KeyContext::Terminal),
                Some(&Command::Action(action.clone())),
                "{action:?} must be reachable from inside the terminal"
            );
        }
    }

    /// Regression: Ctrl+` shipped as the terminal chord and did nothing at all, because
    /// no emulator delivers it (see `KeyChord::is_terminal_ambiguous`). The guard test
    /// above only missed it because the NUL family was absent from the ambiguity table.
    #[test]
    fn the_nul_family_is_ambiguous_so_it_can_never_be_bound_again() {
        for c in ['`', '@', ' '] {
            assert!(KeyChord::ctrl(Key::Char(c)).is_terminal_ambiguous(), "Ctrl+{c} is NUL");
        }
    }

    #[test]
    fn terminal_copy_navigation_has_its_own_context() {
        let k = default_keymap();
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::Left), KeyContext::Terminal),
            Some(&Command::TerminalCopyLeft)
        );
        assert_eq!(
            k.resolve(&KeyChord::shift(Key::Right), KeyContext::Terminal),
            Some(&Command::TerminalCopyExtendRight)
        );
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::Enter), KeyContext::Terminal),
            Some(&Command::TerminalCopyConfirm)
        );
        assert_eq!(
            k.resolve(&KeyChord::plain(Key::Esc), KeyContext::Terminal),
            Some(&Command::TerminalCopyCancel)
        );
    }
}
