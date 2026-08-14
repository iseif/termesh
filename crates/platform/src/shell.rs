//! Construct shell launch specs without ever parsing an agent command (ADR-0008 §2).

use std::path::Path;

use termesh_core::TerminalSpec;

pub fn unix_shell(shell: Option<&str>, cwd: &Path) -> TerminalSpec {
    terminal_spec(non_empty(shell).unwrap_or("/bin/sh"), vec!["-i".into()], cwd)
}

pub fn unix_command(shell: Option<&str>, line: &str, cwd: &Path) -> TerminalSpec {
    terminal_spec(non_empty(shell).unwrap_or("/bin/sh"), vec!["-lc".into(), line.into()], cwd)
}

pub fn windows_shell(comspec: Option<&str>, cwd: &Path) -> TerminalSpec {
    terminal_spec(non_empty(comspec).unwrap_or("cmd.exe"), Vec::new(), cwd)
}

pub fn windows_command(comspec: Option<&str>, line: &str, cwd: &Path) -> TerminalSpec {
    terminal_spec(
        non_empty(comspec).unwrap_or("cmd.exe"),
        vec!["/D".into(), "/S".into(), "/C".into(), line.into()],
        cwd,
    )
}

#[cfg(unix)]
pub fn default_shell(cwd: &Path) -> TerminalSpec {
    let shell = std::env::var("SHELL").ok();
    unix_shell(shell.as_deref(), cwd)
}

#[cfg(windows)]
pub fn default_shell(cwd: &Path) -> TerminalSpec {
    let comspec = std::env::var("COMSPEC").ok();
    windows_shell(comspec.as_deref(), cwd)
}

/// As [`default_shell`], but `configured` (config.toml's `shell` key, ADR-0014 Task 3)
/// wins over the environment when present.
#[cfg(unix)]
pub fn shell(configured: Option<&str>, cwd: &Path) -> TerminalSpec {
    let shell = configured.map(str::to_string).or_else(|| std::env::var("SHELL").ok());
    unix_shell(shell.as_deref(), cwd)
}

/// As [`default_shell`], but `configured` (config.toml's `shell` key, ADR-0014 Task 3)
/// wins over the environment when present.
#[cfg(windows)]
pub fn shell(configured: Option<&str>, cwd: &Path) -> TerminalSpec {
    let comspec = configured.map(str::to_string).or_else(|| std::env::var("COMSPEC").ok());
    windows_shell(comspec.as_deref(), cwd)
}

#[cfg(unix)]
pub fn human_command(line: String, cwd: &Path) -> TerminalSpec {
    let shell = std::env::var("SHELL").ok();
    unix_command(shell.as_deref(), &line, cwd)
}

#[cfg(windows)]
pub fn human_command(line: String, cwd: &Path) -> TerminalSpec {
    let comspec = std::env::var("COMSPEC").ok();
    windows_command(comspec.as_deref(), &line, cwd)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn terminal_spec(program: &str, args: Vec<String>, cwd: &Path) -> TerminalSpec {
    TerminalSpec { program: program.into(), args, cwd: cwd.to_path_buf(), env: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn unix_falls_back_to_bin_sh() {
        let spec = unix_shell(None, Path::new("/proj"));
        assert_eq!(spec.program, "/bin/sh");
        assert_eq!(spec.args, ["-i"]);
    }

    #[test]
    fn unix_ignores_an_empty_shell_value() {
        assert_eq!(unix_shell(Some(""), Path::new("/proj")).program, "/bin/sh");
    }

    #[test]
    fn windows_human_command_uses_cmd_switches() {
        let spec = windows_command(
            Some("C:\\Windows\\System32\\cmd.exe"),
            "cargo test",
            Path::new("C:\\p"),
        );
        assert_eq!(spec.program, "C:\\Windows\\System32\\cmd.exe");
        assert_eq!(spec.args, ["/D", "/S", "/C", "cargo test"]);
    }

    #[test]
    fn agent_text_remains_one_human_shell_argument() {
        let spec = unix_command(Some("/bin/zsh"), "printf '%s' hello", Path::new("/proj"));
        assert_eq!(spec.args, ["-lc", "printf '%s' hello"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_shell_wins_over_the_environment() {
        assert_eq!(shell(Some("/bin/zsh"), Path::new("/proj")).program, "/bin/zsh");
    }

    #[cfg(unix)]
    #[test]
    fn no_configured_shell_falls_back_to_the_platform_default() {
        // Cannot control $SHELL deterministically in a shared test process, but we can
        // assert the fallback is the same value default_shell() would use.
        assert_eq!(
            shell(None, Path::new("/proj")).program,
            default_shell(Path::new("/proj")).program
        );
    }
}
