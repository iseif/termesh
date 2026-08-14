//! Workspace-local language-server command overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use termesh_core::TaskSpec;
use termesh_filesystem::{FileSystemService, FsError};
use toml_edit::DocumentMut;

const SETTINGS_PATH: &str = ".termesh/workspace.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LanguageSettings {
    commands: BTreeMap<String, Vec<String>>,
    tasks: BTreeMap<String, DeclaredTask>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredTask {
    label: String,
    program: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageSettingsError {
    message: String,
}

impl std::fmt::Display for LanguageSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LanguageSettingsError {}

impl LanguageSettings {
    pub fn parse(text: &str) -> Result<Self, LanguageSettingsError> {
        let document = text.parse::<DocumentMut>().map_err(|error| LanguageSettingsError {
            message: format!("invalid workspace settings: {error}"),
        })?;
        Self::from_document(&document)
    }

    pub fn load(fs: &dyn FileSystemService, root: &Path) -> Result<Self, FsError> {
        let path = root.join(SETTINGS_PATH);
        let bytes = match fs.read_file(&path) {
            Ok(bytes) => bytes,
            Err(FsError::NotFound(_)) => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| settings_error(&path, "workspace settings are not valid UTF-8"))?;
        Self::parse(&text).map_err(|error| settings_error(&path, error.to_string()))
    }

    pub fn command(&self, language: &str) -> Option<&Vec<String>> {
        self.commands.get(language)
    }

    pub fn tasks(&self, root: &Path) -> Vec<TaskSpec> {
        self.tasks
            .iter()
            .map(|(id, task)| TaskSpec {
                id: format!("workspace.{id}"),
                label: task.label.clone(),
                program: task.program.clone(),
                args: task.args.clone(),
                cwd: root.into(),
            })
            .collect()
    }

    fn from_document(document: &DocumentMut) -> Result<Self, LanguageSettingsError> {
        let mut commands = BTreeMap::new();
        if let Some(lsp) = document.get("lsp") {
            let lsp = lsp
                .as_table()
                .ok_or_else(|| LanguageSettingsError { message: "lsp must be a table".into() })?;
            for (language, item) in lsp {
                let table = item.as_table().ok_or_else(|| LanguageSettingsError {
                    message: format!("lsp.{language} must be a table"),
                })?;
                let Some(command) = table.get("command") else {
                    continue;
                };
                let array = command.as_array().ok_or_else(|| LanguageSettingsError {
                    message: format!("lsp.{language}.command must be an argv array"),
                })?;
                let command: Vec<String> = array
                    .iter()
                    .map(|argument| {
                        argument.as_str().map(str::to_string).ok_or_else(|| LanguageSettingsError {
                            message: format!(
                                "lsp.{language}.command arguments must all be strings"
                            ),
                        })
                    })
                    .collect::<Result<_, _>>()?;
                if command.is_empty() || command[0].is_empty() {
                    return Err(LanguageSettingsError {
                        message: format!("lsp.{language}.command needs a program"),
                    });
                }
                commands.insert(language.to_string(), command);
            }
        }

        let mut tasks = BTreeMap::new();
        if let Some(declared) = document.get("tasks") {
            let declared = declared
                .as_table()
                .ok_or_else(|| LanguageSettingsError { message: "tasks must be a table".into() })?;
            for (id, item) in declared {
                if id.is_empty() {
                    return Err(LanguageSettingsError {
                        message: "task id cannot be empty".into(),
                    });
                }
                let table = item.as_table().ok_or_else(|| LanguageSettingsError {
                    message: format!("tasks.{id} must be a table"),
                })?;
                let label = table.get("label").and_then(|item| item.as_str()).ok_or_else(|| {
                    LanguageSettingsError { message: format!("tasks.{id}.label must be a string") }
                })?;
                let program =
                    table.get("program").and_then(|item| item.as_str()).ok_or_else(|| {
                        LanguageSettingsError {
                            message: format!("tasks.{id}.program must be a string"),
                        }
                    })?;
                if program.is_empty() {
                    return Err(LanguageSettingsError {
                        message: format!("tasks.{id}.program cannot be empty"),
                    });
                }
                let args = table.get("args").and_then(|item| item.as_array()).ok_or_else(|| {
                    LanguageSettingsError {
                        message: format!("tasks.{id}.args must be an argv array"),
                    }
                })?;
                let args = args
                    .iter()
                    .map(|argument| {
                        argument.as_str().map(str::to_string).ok_or_else(|| LanguageSettingsError {
                            message: format!("tasks.{id}.args must contain only strings"),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                tasks.insert(
                    id.to_string(),
                    DeclaredTask { label: label.to_string(), program: program.to_string(), args },
                );
            }
        }
        Ok(Self { commands, tasks })
    }
}

fn settings_error(path: &Path, message: impl Into<String>) -> FsError {
    FsError::Other { path: PathBuf::from(path), message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use termesh_test_support::FakeFileSystem;

    #[test]
    fn a_workspace_override_yields_the_raw_optional_command() {
        let settings = LanguageSettings::parse(
            r#"
                [lsp.rust]
                command = ["my-analyzer", "--stdio"]
            "#,
        )
        .unwrap();
        assert_eq!(
            settings.command("rust"),
            Some(&vec!["my-analyzer".to_string(), "--stdio".to_string()])
        );
    }

    #[test]
    fn a_malformed_override_reports_rather_than_replacing() {
        let err = LanguageSettings::parse("[lsp.rust]\ncommand = \"one string\"\n")
            .expect_err("a command must be an argv array");
        assert!(err.to_string().contains("command"), "{err}");
    }

    #[test]
    fn an_absent_settings_file_falls_back_to_the_default_recipe() {
        let fs = FakeFileSystem::with_paths(&["/proj/Cargo.toml"]);
        let settings = LanguageSettings::load(&fs, Path::new("/proj")).unwrap();
        assert!(settings.command("rust").is_none());
    }

    #[test]
    fn malformed_settings_name_the_file_and_reason() {
        let fs = FakeFileSystem::new();
        fs.add_file("/proj/.termesh/workspace.toml", b"[lsp.rust\n");
        let error = LanguageSettings::load(&fs, Path::new("/proj")).unwrap_err();
        assert!(error.to_string().contains("workspace.toml"), "{error}");
    }

    #[test]
    fn workspace_declared_tasks_are_added_to_the_detected_ones() {
        let settings = LanguageSettings::parse(
            r#"
                [tasks.smoke]
                label = "Smoke"
                program = "make"
                args = ["smoke"]
            "#,
        )
        .unwrap();

        let declared = settings.tasks(Path::new("/p"));

        assert_eq!(declared[0].id, "workspace.smoke");
        assert_eq!(declared[0].program, "make");
        assert_eq!(declared[0].args, vec!["smoke"]);
    }

    #[test]
    fn a_declared_task_must_use_an_argv_array() {
        let error = LanguageSettings::parse(
            "[tasks.a]\nlabel = \"A\"\nprogram = \"make\"\nargs = \"smoke\"\n",
        )
        .expect_err("args is an argv array, never a shell string");
        assert!(error.to_string().contains("args"), "{error}");
    }
}
