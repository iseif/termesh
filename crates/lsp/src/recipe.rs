//! Built-in language-server recipes and command resolution.

use std::path::Path;
use std::process::Command;

use termesh_core::{LspFailure, LspFailureKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub language_id: String,
    pub command: Vec<String>,
    /// File extensions this server claims. Routing is per document so a polyglot
    /// workspace can run several servers at once in a later phase.
    pub extensions: Vec<String>,
    /// Raw JSON for `initializationOptions`. `None` for rust-analyzer.
    pub initialization_options: Option<String>,
}

impl Recipe {
    pub fn claims(&self, path: &Path) -> bool {
        path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
            self.extensions.iter().any(|claimed| claimed.eq_ignore_ascii_case(extension))
        })
    }
}

pub fn recipe_for(project_label: &str) -> Option<Recipe> {
    match project_label {
        "rust" => Some(Recipe {
            language_id: "rust".into(),
            command: vec!["rust-analyzer".into()],
            extensions: vec!["rs".into()],
            initialization_options: None,
        }),
        "node" => Some(Recipe {
            language_id: "typescript".into(),
            command: vec!["typescript-language-server".into(), "--stdio".into()],
            extensions: ["ts", "tsx", "js", "jsx", "mjs", "cjs"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            initialization_options: None,
        }),
        "python" => Some(Recipe {
            language_id: "python".into(),
            command: vec!["pyright-langserver".into(), "--stdio".into()],
            extensions: vec!["py".into(), "pyi".into()],
            initialization_options: None,
        }),
        "java" => Some(Recipe {
            language_id: "java".into(),
            // The launcher wrapper owns equinox discovery, per-OS configuration,
            // and workspace data; Termesh deliberately passes no launcher flags
            // (ADR-0013 §1).
            command: vec!["jdtls".into()],
            extensions: vec!["java".into()],
            initialization_options: None,
        }),
        _ => None,
    }
}

pub fn resolve_recipe(
    project_label: &str,
    command_override: Option<Vec<String>>,
) -> Option<Recipe> {
    let mut recipe = recipe_for(project_label)?;
    if let Some(command) = command_override {
        recipe.command = command;
    }
    Some(recipe)
}

pub fn server_available(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok_and(|output| output.status.success())
}

pub fn missing_server(program: &str) -> LspFailure {
    let install = match program {
        "rust-analyzer" => "Install it with `rustup component add rust-analyzer`.",
        "typescript-language-server" => {
            "Install it with `npm i -g typescript-language-server typescript`."
        }
        "pyright-langserver" => "Install it with `npm i -g pyright`.",
        "jdtls" => {
            "Install it (for example, `brew install jdtls`) or set \
             `[lsp.java].command` in .termesh/workspace.toml for a custom launcher."
        }
        _ => "Install it or configure [lsp.<language>].command in .termesh/workspace.toml.",
    };
    LspFailure {
        kind: LspFailureKind::NotInstalled,
        message: format!("Language server `{program}` was not found on PATH. {install}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use termesh_core::LspFailureKind;

    #[test]
    fn a_rust_project_gets_the_rust_analyzer_recipe() {
        let r = recipe_for("rust").expect("rust has a recipe");
        assert_eq!(r.command, vec!["rust-analyzer".to_string()]);
        assert_eq!(r.language_id, "rust");
        assert_eq!(r.extensions, vec!["rs".to_string()]);
        assert!(r.initialization_options.is_none(), "rust-analyzer needs none");
    }

    #[test]
    fn a_recipe_can_carry_initialization_options() {
        let r = Recipe {
            language_id: "java".into(),
            command: vec!["jdtls".into()],
            extensions: vec!["java".into()],
            initialization_options: Some(r#"{"settings":{}}"#.into()),
        };
        assert!(r.initialization_options.is_some());
    }

    #[test]
    fn a_recipe_claims_documents_by_extension() {
        let r = recipe_for("rust").unwrap();
        assert!(r.claims(Path::new("/proj/src/main.rs")));
        assert!(!r.claims(Path::new("/proj/web/app.ts")));
    }

    #[test]
    fn a_node_project_gets_typescript_language_server() {
        let r = recipe_for("node").expect("node has a recipe");
        assert_eq!(
            r.command,
            vec!["typescript-language-server".to_string(), "--stdio".to_string()]
        );
        assert_eq!(r.language_id, "typescript");
        // One server owns the whole JS/TS family; routing is by extension, not dialect.
        for extension in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            assert!(r.claims(Path::new(&format!("/p/a.{extension}"))), "{extension}");
        }
        assert!(!r.claims(Path::new("/p/a.json")));
    }

    #[test]
    fn a_python_project_gets_pyright() {
        let r = recipe_for("python").expect("python has a recipe");
        assert_eq!(r.command, vec!["pyright-langserver".to_string(), "--stdio".to_string()]);
        assert_eq!(r.language_id, "python");
        assert!(r.claims(Path::new("/p/a.py")));
        assert!(r.claims(Path::new("/p/a.pyi")));
    }

    #[test]
    fn a_java_project_gets_the_jdtls_launcher() {
        let r = recipe_for("java").expect("java has a recipe");
        // The wrapper owns the equinox launcher, the per-OS configuration directory,
        // and the workspace data directory, so the command is one word (ADR-0013 §1).
        assert_eq!(r.command, vec!["jdtls".to_string()]);
        assert_eq!(r.language_id, "java");
        assert!(r.claims(Path::new("/p/src/App.java")));
        assert!(!r.claims(Path::new("/p/pom.xml")));
        assert!(r.initialization_options.is_none());
    }

    #[test]
    fn a_missing_jdtls_names_an_install_route_and_the_override() {
        let failure = missing_server("jdtls");
        assert_eq!(failure.kind, LspFailureKind::NotInstalled);
        assert!(failure.message.contains("jdtls"), "{}", failure.message);
        assert!(
            failure.message.contains("brew") || failure.message.contains("install"),
            "an actionable route: {}",
            failure.message
        );
        assert!(failure.message.contains("[lsp.java].command"), "{}", failure.message);
    }

    #[test]
    fn a_workspace_override_replaces_the_java_command() {
        // Workspace settings parse the raw argv independently; this boundary proves
        // the documented tarball escape hatch replaces the built-in Java launcher.
        let command = vec![
            "java".to_string(),
            "-jar".to_string(),
            "/opt/jdtls/plugins/launcher.jar".to_string(),
        ];
        let r = resolve_recipe("java", Some(command.clone())).expect("java has a recipe");
        assert_eq!(r.command, command);
    }

    #[test]
    fn a_kind_without_a_recipe_is_a_supported_state_not_an_error() {
        // Go is detected but deliberately unimplemented this phase (ADR-0012 Context).
        assert!(recipe_for("go").is_none());
        assert!(recipe_for("unknown").is_none());
    }

    #[test]
    fn a_missing_binary_produces_an_actionable_message() {
        let failure = missing_server("rust-analyzer");
        assert_eq!(failure.kind, LspFailureKind::NotInstalled);
        assert!(failure.message.contains("rust-analyzer"), "{}", failure.message);
        assert!(failure.message.contains("rustup"), "say how to install it");
    }

    #[test]
    fn every_recipe_names_how_to_install_its_server() {
        for program in
            ["rust-analyzer", "typescript-language-server", "pyright-langserver", "jdtls"]
        {
            let failure = missing_server(program);
            assert_eq!(failure.kind, LspFailureKind::NotInstalled);
            assert!(failure.message.contains(program), "{}", failure.message);
            assert!(
                failure.message.contains("npm")
                    || failure.message.contains("rustup")
                    || failure.message.contains("brew"),
                "an actionable install command, not just a complaint: {}",
                failure.message
            );
        }
    }

    #[test]
    fn resolve_recipe_applies_a_raw_command_override() {
        let r =
            resolve_recipe("rust", Some(vec!["my-analyzer".to_string(), "--stdio".to_string()]))
                .expect("rust has a recipe");
        assert_eq!(r.command, vec!["my-analyzer".to_string(), "--stdio".to_string()]);
    }
}
