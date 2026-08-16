//! Command-line parsing. Hand-rolled: the surface is a small set of flags and a path, and a
//! dependency on `clap` would be a load-bearing addition for no benefit yet.

use std::path::PathBuf;

/// What the binary was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// The directory to open. `None` means start with no workspace.
    pub path: Option<PathBuf>,
    pub mode: Mode,
    pub color: ColorChoice,
    /// Local trace destination. `None` means instrumentation has no subscriber and
    /// therefore collects nothing.
    pub trace: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
    Ansi16,
}

impl ColorChoice {
    pub fn resolve(self, detected: termesh_platform::ColorDepth) -> termesh_platform::ColorDepth {
        match self {
            ColorChoice::Auto => detected,
            ColorChoice::Always => termesh_platform::ColorDepth::Indexed256,
            ColorChoice::Never => termesh_platform::ColorDepth::None,
            ColorChoice::Ansi16 => termesh_platform::ColorDepth::Ansi16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Run the interactive TUI.
    Run,
    /// Render one frame to stdout and exit. No TTY required.
    DumpFrame {
        palette: Option<String>,
        /// A file to open in the editor before rendering, so the headless smoke can
        /// show a real buffer and not just the empty-state hint.
        open: Option<PathBuf>,
        /// Run a canned agent turn against the opened file first, so the frame shows
        /// review hunks. The CI-visible proof that the phase's whole point renders.
        agent_demo: bool,
        /// Inject a recorded ANSI command run into a managed terminal without spawning
        /// a process. This keeps the Phase-04 smoke deterministic and headless.
        terminal_demo: bool,
        /// Exercise the Phase-05 Cargo task and Problems UI with scripted events only.
        search_task_demo: bool,
        /// Show deterministic Phase-06 Git state and a unified diff without a repository.
        git_demo: bool,
        /// Show deterministic Phase-07 diagnostics and hover without a language server.
        lsp_demo: bool,
        /// Show a synthetic Rust + TypeScript workspace without spawning toolchains.
        polyglot_demo: bool,
        /// Show synthetic Java diagnostics, JDT import progress, and Maven tasks.
        java_demo: bool,
    },
    /// Start nothing — the CI smoke test that the binary links and runs.
    ProbeOnly,
    Help,
    Version,
}

impl Cli {
    /// Parse arguments, *excluding* argv[0].
    pub fn parse<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|a| a.as_ref().to_string()).collect();
        let mut path = None;
        let mut mode = Mode::Run;
        let mut open_file: Option<PathBuf> = None;
        let mut agent_demo = false;
        let mut terminal_demo = false;
        let mut search_task_demo = false;
        let mut git_demo = false;
        let mut lsp_demo = false;
        let mut polyglot_demo = false;
        let mut java_demo = false;
        let mut color = ColorChoice::Auto;
        let mut trace = None;
        let mut i = 0;

        while i < args.len() {
            match args[i].as_str() {
                "--probe-only" => mode = Mode::ProbeOnly,
                "--help" | "-h" => mode = Mode::Help,
                "--version" | "-V" => mode = Mode::Version,
                "--dump-frame" => {
                    // `--dump-frame palette [query]` previews the command palette.
                    let palette = if args.get(i + 1).map(String::as_str) == Some("palette") {
                        i += 1;
                        // A following non-flag token is the filter query.
                        match args.get(i + 1) {
                            Some(q) if !q.starts_with('-') => {
                                i += 1;
                                Some(q.clone())
                            }
                            _ => Some(String::new()),
                        }
                    } else {
                        None
                    };
                    mode = Mode::DumpFrame {
                        palette,
                        open: None,
                        agent_demo: false,
                        terminal_demo: false,
                        search_task_demo: false,
                        git_demo: false,
                        lsp_demo: false,
                        polyglot_demo: false,
                        java_demo: false,
                    };
                }
                "--agent-demo" => agent_demo = true,
                "--terminal-demo" => terminal_demo = true,
                "--search-task-demo" => search_task_demo = true,
                "--git-demo" => git_demo = true,
                "--lsp-demo" => lsp_demo = true,
                "--polyglot-demo" => polyglot_demo = true,
                "--java-demo" => java_demo = true,
                "--color=auto" => color = ColorChoice::Auto,
                "--color=always" => color = ColorChoice::Always,
                "--color=never" => color = ColorChoice::Never,
                "--color=16" => color = ColorChoice::Ansi16,
                "--trace" => {
                    if let Some(file) = args.get(i + 1).filter(|file| !file.starts_with('-')) {
                        trace = Some(PathBuf::from(file));
                        i += 1;
                    }
                }
                // `--open FILE` fills the editor pane in a headless frame.
                "--open" => {
                    if let Some(file) = args.get(i + 1).filter(|f| !f.starts_with('-')) {
                        open_file = Some(PathBuf::from(file));
                        i += 1;
                    }
                }
                // The first bare argument is the project path.
                other if !other.starts_with('-') && path.is_none() => {
                    path = Some(PathBuf::from(other));
                }
                _ => {}
            }
            i += 1;
        }

        // These may appear either side of `--dump-frame`, so they are folded in last.
        if let Mode::DumpFrame { palette, open, .. } = &mode {
            mode = Mode::DumpFrame {
                palette: palette.clone(),
                open: open_file.or_else(|| open.clone()),
                agent_demo,
                terminal_demo,
                search_task_demo,
                git_demo,
                lsp_demo,
                polyglot_demo,
                java_demo,
            };
        }

        Self { path, mode, color, trace }
    }
}

pub const VERSION_LINE: &str = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"));

pub fn version_line() -> String {
    let commit = option_env!("TERMESH_GIT_COMMIT")
        .or(option_env!("GITHUB_SHA"))
        .unwrap_or("unknown")
        .chars()
        .take(12)
        .collect::<String>();
    format!("{VERSION_LINE} ({commit})")
}

pub const HELP: &str = concat!(
    "termesh ",
    env!("CARGO_PKG_VERSION"),
    " — a terminal-native, agent-first IDE (beta)\n

USAGE:
    termesh [PATH]

ARGS:
    PATH    Directory to open. The workspace root is detected by walking up
            to the nearest Cargo.toml / go.mod / pyproject.toml / package.json / .git.

OPTIONS:
    --dump-frame [palette [QUERY]]   Render one frame to stdout and exit (no TTY needed)
    --open FILE                      With --dump-frame: open FILE in the editor first
    --agent-demo                     With --dump-frame: run a scripted agent turn first
    --terminal-demo                  With --dump-frame: show recorded ANSI terminal output
    --search-task-demo               With --dump-frame: show a failed Cargo task and Problems
    --git-demo                       With --dump-frame: show deterministic Git status and diff
    --lsp-demo                       With --dump-frame: show diagnostics and hover without a server
    --polyglot-demo                  With --dump-frame: show Rust + TypeScript and discovered tasks
    --java-demo                      With --dump-frame: show Java diagnostics and Maven tasks
    --color=<auto|always|never|16>   Override terminal colour capability detection
    --trace FILE                     Write local opt-in performance traces to FILE
    --probe-only                     Start nothing; used as a CI smoke test
    -V, --version                    Show version and build commit
    -h, --help                       Show this help
"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse(args)
    }

    #[test]
    fn no_arguments_runs_with_no_workspace() {
        let cli = parse(&[]);
        assert_eq!(cli.mode, Mode::Run);
        assert_eq!(cli.path, None);
    }

    #[test]
    fn color_override_accepts_the_documented_values() {
        assert_eq!(parse(&["--color=auto"]).color, ColorChoice::Auto);
        assert_eq!(parse(&["--color=always"]).color, ColorChoice::Always);
        assert_eq!(parse(&["--color=never"]).color, ColorChoice::Never);
        assert_eq!(parse(&["--color=16"]).color, ColorChoice::Ansi16);
    }

    #[test]
    fn explicit_color_choices_override_detection() {
        use termesh_platform::ColorDepth;

        assert_eq!(ColorChoice::Auto.resolve(ColorDepth::TrueColor), ColorDepth::TrueColor);
        assert_eq!(ColorChoice::Always.resolve(ColorDepth::None), ColorDepth::Indexed256);
        assert_eq!(ColorChoice::Never.resolve(ColorDepth::TrueColor), ColorDepth::None);
        assert_eq!(ColorChoice::Ansi16.resolve(ColorDepth::TrueColor), ColorDepth::Ansi16);
    }

    #[test]
    fn a_bare_path_is_the_project_to_open() {
        assert_eq!(parse(&["/tmp/proj"]).path, Some(PathBuf::from("/tmp/proj")));
        assert_eq!(parse(&["."]).path, Some(PathBuf::from(".")));
    }

    #[test]
    fn flags_and_a_path_coexist() {
        let cli = parse(&["--dump-frame", "/tmp/proj"]);
        assert_eq!(
            cli.mode,
            Mode::DumpFrame {
                palette: None,
                open: None,
                agent_demo: false,
                terminal_demo: false,
                search_task_demo: false,
                git_demo: false,
                lsp_demo: false,
                polyglot_demo: false,
                java_demo: false,
            }
        );
        assert_eq!(cli.path, Some(PathBuf::from("/tmp/proj")));
    }

    #[test]
    fn dump_frame_takes_an_optional_palette_query() {
        assert_eq!(
            parse(&["--dump-frame"]).mode,
            Mode::DumpFrame {
                palette: None,
                open: None,
                agent_demo: false,
                terminal_demo: false,
                search_task_demo: false,
                git_demo: false,
                lsp_demo: false,
                polyglot_demo: false,
                java_demo: false,
            }
        );
        assert_eq!(
            parse(&["--dump-frame", "palette"]).mode,
            Mode::DumpFrame {
                palette: Some(String::new()),
                open: None,
                agent_demo: false,
                terminal_demo: false,
                search_task_demo: false,
                git_demo: false,
                lsp_demo: false,
                polyglot_demo: false,
                java_demo: false,
            }
        );
        assert_eq!(
            parse(&["--dump-frame", "palette", "git"]).mode,
            Mode::DumpFrame {
                palette: Some("git".into()),
                open: None,
                agent_demo: false,
                terminal_demo: false,
                search_task_demo: false,
                git_demo: false,
                lsp_demo: false,
                polyglot_demo: false,
                java_demo: false,
            }
        );
    }

    #[test]
    fn a_palette_query_is_not_mistaken_for_the_project_path() {
        let cli = parse(&["--dump-frame", "palette", "git"]);
        assert_eq!(cli.path, None, "'git' is the query, not a directory");
    }

    #[test]
    fn only_the_first_bare_argument_is_taken_as_the_path() {
        assert_eq!(parse(&["/a", "/b"]).path, Some(PathBuf::from("/a")));
    }

    #[test]
    fn dump_frame_can_open_a_file_from_either_side_of_the_flag() {
        for args in
            [["--dump-frame", "--open", "src/main.rs"], ["--open", "src/main.rs", "--dump-frame"]]
        {
            assert_eq!(
                parse(&args).mode,
                Mode::DumpFrame {
                    palette: None,
                    open: Some(PathBuf::from("src/main.rs")),
                    agent_demo: false,
                    terminal_demo: false,
                    search_task_demo: false,
                    git_demo: false,
                    lsp_demo: false,
                    polyglot_demo: false,
                    java_demo: false,
                },
                "failed for {args:?}"
            );
        }
    }

    #[test]
    fn dump_frame_accepts_terminal_demo_on_either_side() {
        for args in [["--dump-frame", "--terminal-demo"], ["--terminal-demo", "--dump-frame"]] {
            assert!(matches!(parse(&args).mode, Mode::DumpFrame { terminal_demo: true, .. }));
        }
    }

    #[test]
    fn dump_frame_accepts_git_demo() {
        let cli = parse(&["--dump-frame", "--git-demo"]);
        assert!(matches!(cli.mode, Mode::DumpFrame { git_demo: true, .. }));
    }

    #[test]
    fn dump_frame_accepts_lsp_demo() {
        let cli = parse(&["--dump-frame", "--lsp-demo"]);
        assert!(matches!(cli.mode, Mode::DumpFrame { lsp_demo: true, .. }));
    }

    #[test]
    fn dump_frame_accepts_polyglot_demo() {
        let cli = parse(&["--dump-frame", "--polyglot-demo"]);
        assert!(matches!(cli.mode, Mode::DumpFrame { polyglot_demo: true, .. }));
    }

    #[test]
    fn dump_frame_accepts_java_demo() {
        let cli = parse(&["--dump-frame", "--java-demo"]);
        assert!(matches!(cli.mode, Mode::DumpFrame { java_demo: true, .. }));
    }

    #[test]
    fn dump_frame_accepts_search_task_demo_on_either_side() {
        for args in [["--dump-frame", "--search-task-demo"], ["--search-task-demo", "--dump-frame"]]
        {
            assert!(matches!(parse(&args).mode, Mode::DumpFrame { search_task_demo: true, .. }));
        }
    }

    #[test]
    fn the_file_after_open_is_not_mistaken_for_the_project_path() {
        let cli = parse(&["--dump-frame", "--open", "src/main.rs", "/tmp/proj"]);
        assert_eq!(cli.path, Some(PathBuf::from("/tmp/proj")));
    }

    #[test]
    fn open_is_ignored_without_dump_frame() {
        // Nothing to render into, so the interactive path is unaffected.
        assert_eq!(parse(&["--open", "src/main.rs"]).mode, Mode::Run);
    }

    #[test]
    fn help_and_probe_are_recognised() {
        assert_eq!(parse(&["--help"]).mode, Mode::Help);
        assert_eq!(parse(&["-h"]).mode, Mode::Help);
        assert_eq!(parse(&["--probe-only"]).mode, Mode::ProbeOnly);
    }

    #[test]
    fn the_version_flag_prints_the_crate_version() {
        assert_eq!(parse(&["--version"]).mode, Mode::Version);
        assert_eq!(VERSION_LINE, "termesh 0.1.0");
        let line = version_line();
        assert!(line.starts_with(VERSION_LINE));
        assert!(line.ends_with(')'), "name version (commit): {line}");
    }

    #[test]
    fn instrumentation_is_off_unless_asked_for() {
        assert_eq!(Cli::parse(["."]).trace, None);
        assert_eq!(Cli::parse(["--trace", "/tmp/t.log", "."]).trace, Some("/tmp/t.log".into()));
    }
}
