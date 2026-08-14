//! Exact, workspace-scoped grants for agent-managed terminal commands (ADR-0008 §5).

use std::path::{Component, Path, PathBuf};

use termesh_core::TerminalSpec;
use termesh_filesystem::{FileSystemService, FsError};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

const SETTINGS_DIR: &str = ".termesh";
const SETTINGS_FILE: &str = "workspace.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandGrant {
    pub program: String,
    pub args: Vec<String>,
    /// Lexically normalized path relative to the workspace, or `.` for its root.
    pub cwd: PathBuf,
}

impl CommandGrant {
    pub fn from_spec(root: &Path, spec: &TerminalSpec) -> Option<Self> {
        if spec.program.is_empty() || !spec.env.is_empty() {
            return None;
        }
        let root = clean_absolute(root)?;
        let cwd = clean_absolute(&spec.cwd)?;
        let relative = cwd.strip_prefix(&root).ok()?;
        let relative = if relative.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            relative.to_path_buf()
        };
        Some(Self { program: spec.program.clone(), args: spec.args.clone(), cwd: relative })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionPolicy {
    grants: Vec<CommandGrant>,
    dirty: bool,
}

impl PermissionPolicy {
    pub fn permits(&self, root: &Path, spec: &TerminalSpec) -> bool {
        CommandGrant::from_spec(root, spec).is_some_and(|grant| self.grants.contains(&grant))
    }

    /// Remember one exact safe command. Returns false when the command is unsafe.
    pub fn remember(&mut self, root: &Path, spec: &TerminalSpec) -> bool {
        let Some(grant) = CommandGrant::from_spec(root, spec) else {
            return false;
        };
        if !self.grants.contains(&grant) {
            self.grants.push(grant);
            self.dirty = true;
        }
        true
    }

    pub fn grants(&self) -> &[CommandGrant] {
        &self.grants
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }
}

pub trait PermissionStore: Send + Sync {
    fn load(&self, root: &Path) -> Result<PermissionPolicy, FsError>;
    fn save(&self, root: &Path, policy: &PermissionPolicy) -> Result<(), FsError>;
}

pub struct FilePermissionStore<'a> {
    fs: &'a dyn FileSystemService,
}

impl<'a> FilePermissionStore<'a> {
    pub fn new(fs: &'a dyn FileSystemService) -> Self {
        Self { fs }
    }
}

impl PermissionStore for FilePermissionStore<'_> {
    fn load(&self, root: &Path) -> Result<PermissionPolicy, FsError> {
        let path = settings_path(root);
        let bytes = match self.fs.read_file(&path) {
            Ok(bytes) => bytes,
            Err(FsError::NotFound(_)) => return Ok(PermissionPolicy::default()),
            Err(error) => return Err(error),
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| config_error(&path, "workspace settings are not valid UTF-8"))?;
        let document = parse_document(&path, &text)?;
        policy_from_document(root, &path, &document)
    }

    fn save(&self, root: &Path, policy: &PermissionPolicy) -> Result<(), FsError> {
        let path = settings_path(root);
        let mut document = match self.fs.read_file(&path) {
            Ok(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| config_error(&path, "workspace settings are not valid UTF-8"))?;
                parse_document(&path, &text)?
            }
            Err(FsError::NotFound(_)) => DocumentMut::new(),
            Err(error) => return Err(error),
        };
        replace_commands(&path, &mut document, &policy.grants)?;
        self.fs.create_dir(&root.join(SETTINGS_DIR))?;
        self.fs.write_file(&path, document.to_string().as_bytes())
    }
}

fn settings_path(root: &Path) -> PathBuf {
    root.join(SETTINGS_DIR).join(SETTINGS_FILE)
}

fn parse_document(path: &Path, text: &str) -> Result<DocumentMut, FsError> {
    text.parse::<DocumentMut>().map_err(|error| config_error(path, error.to_string()))
}

fn policy_from_document(
    root: &Path,
    path: &Path,
    document: &DocumentMut,
) -> Result<PermissionPolicy, FsError> {
    let Some(agent) = document.get("agent") else {
        return Ok(PermissionPolicy::default());
    };
    let agent = agent.as_table().ok_or_else(|| config_error(path, "agent must be a table"))?;
    let Some(permissions) = agent.get("permissions") else {
        return Ok(PermissionPolicy::default());
    };
    let permissions = permissions
        .as_table()
        .ok_or_else(|| config_error(path, "agent.permissions must be a table"))?;
    let Some(commands) = permissions.get("commands") else {
        return Ok(PermissionPolicy::default());
    };
    let commands = commands.as_array_of_tables().ok_or_else(|| {
        config_error(path, "agent.permissions.commands must be an array of tables")
    })?;

    let mut policy = PermissionPolicy::default();
    for (index, table) in commands.iter().enumerate() {
        if table.iter().any(|(key, _)| !matches!(key, "program" | "args" | "cwd")) {
            return Err(config_error(path, format!("command {index} has an unknown field")));
        }
        let program = table
            .get("program")
            .and_then(Item::as_str)
            .ok_or_else(|| config_error(path, format!("command {index} needs a string program")))?;
        let args = table
            .get("args")
            .and_then(Item::as_array)
            .ok_or_else(|| config_error(path, format!("command {index} needs an args array")))?;
        let args: Vec<String> = args
            .iter()
            .map(|argument| {
                argument.as_str().map(str::to_owned).ok_or_else(|| {
                    config_error(path, format!("command {index} args must be strings"))
                })
            })
            .collect::<Result<_, _>>()?;
        let stored_cwd = table
            .get("cwd")
            .and_then(Item::as_str)
            .ok_or_else(|| config_error(path, format!("command {index} needs a string cwd")))?;
        let relative = clean_relative(Path::new(stored_cwd)).ok_or_else(|| {
            config_error(path, format!("command {index} cwd must stay inside the workspace"))
        })?;
        let spec = TerminalSpec {
            program: program.into(),
            args,
            cwd: root.join(&relative),
            env: Vec::new(),
        };
        let grant = CommandGrant::from_spec(root, &spec)
            .ok_or_else(|| config_error(path, format!("command {index} is unsafe")))?;
        if !policy.grants.contains(&grant) {
            policy.grants.push(grant);
        }
    }
    Ok(policy)
}

fn replace_commands(
    path: &Path,
    document: &mut DocumentMut,
    grants: &[CommandGrant],
) -> Result<(), FsError> {
    if document.get("agent").is_some_and(|item| !item.is_table()) {
        return Err(config_error(path, "agent must be a table"));
    }
    if document.get("agent").is_none() {
        document["agent"] = Item::Table(Table::new());
    }
    let agent = document["agent"].as_table_mut().expect("agent table created above");
    if agent.get("permissions").is_some_and(|item| !item.is_table()) {
        return Err(config_error(path, "agent.permissions must be a table"));
    }
    if agent.get("permissions").is_none() {
        agent["permissions"] = Item::Table(Table::new());
    }
    let permissions = agent["permissions"].as_table_mut().expect("permissions table created above");

    let mut commands = ArrayOfTables::new();
    for grant in grants {
        let mut table = Table::new();
        table["program"] = value(grant.program.clone());
        let mut args = Array::new();
        for argument in &grant.args {
            args.push(argument.as_str());
        }
        table["args"] = value(args);
        table["cwd"] = value(grant.cwd.to_string_lossy().into_owned());
        commands.push(table);
    }
    permissions["commands"] = Item::ArrayOfTables(commands);
    Ok(())
}

fn clean_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return None,
            Component::CurDir => {}
            Component::Prefix(prefix) => clean.push(prefix.as_os_str()),
            Component::RootDir => clean.push(component.as_os_str()),
            Component::Normal(part) => clean.push(part),
        }
    }
    Some(clean)
}

fn clean_relative(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(if clean.as_os_str().is_empty() { PathBuf::from(".") } else { clean })
}

fn config_error(path: &Path, message: impl Into<String>) -> FsError {
    FsError::Other { path: path.to_path_buf(), message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use termesh_filesystem::FileSystemService;
    use termesh_test_support::FakeFileSystem;

    /// A workspace root the host platform agrees is absolute.
    ///
    /// `/proj` is *rooted* on Windows but not *absolute* — that needs a drive prefix — and
    /// [`clean_absolute`] rightly refuses a non-absolute cwd, since a relative one cannot be
    /// checked for containment. Hardcoding `/proj` therefore made every grant assertion here
    /// fail on Windows while passing everywhere else.
    const ROOT: &str = if cfg!(windows) { r"C:\proj" } else { "/proj" };

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    /// `ROOT` joined with a workspace-relative path, in the host's own separator.
    fn under_root(relative: &str) -> std::path::PathBuf {
        relative.split('/').fold(root().to_path_buf(), |acc, part| acc.join(part))
    }

    fn spec(
        program: &str,
        args: &[&str],
        cwd: &str,
        env: &[(&str, &str)],
    ) -> termesh_core::TerminalSpec {
        termesh_core::TerminalSpec {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: cwd.into(),
            env: env.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect(),
        }
    }

    fn cargo_test_policy() -> PermissionPolicy {
        let mut policy = PermissionPolicy::default();
        assert!(policy.remember(root(), &spec("cargo", &["test"], ROOT, &[])));
        policy
    }

    #[test]
    fn only_exact_safe_commands_can_be_remembered() {
        let root = root();
        let safe = spec("cargo", &["test"], ROOT, &[]);
        let env = spec("cargo", &["test"], ROOT, &[("TOKEN", "secret")]);
        let outside = spec("cargo", &["test"], "/tmp", &[]);
        let traversal = spec("cargo", &["test"], under_root("src/../src").to_str().unwrap(), &[]);

        assert!(CommandGrant::from_spec(root, &safe).is_some());
        assert!(CommandGrant::from_spec(root, &env).is_none());
        assert!(CommandGrant::from_spec(root, &outside).is_none());
        assert!(CommandGrant::from_spec(root, &traversal).is_none());
    }

    #[test]
    fn policy_matches_program_arguments_and_workspace_relative_cwd_exactly() {
        let root = root();
        let mut policy = PermissionPolicy::default();
        let allowed = spec("cargo", &["test"], ROOT, &[]);
        assert!(policy.remember(root, &allowed));
        assert!(policy.permits(root, &allowed));
        assert!(!policy.permits(root, &spec("cargo", &["test", "--all"], ROOT, &[])));
        assert!(!policy
            .permits(root, &spec("cargo", &["test"], under_root("sub").to_str().unwrap(), &[])));
        assert!(policy.is_dirty());
        policy.mark_saved();
        assert!(!policy.is_dirty());
    }

    #[test]
    fn saving_preserves_unrelated_keys_and_comments() {
        let fs = FakeFileSystem::new();
        fs.add_file(
            under_root(".termesh/workspace.toml"),
            b"# mine\n[tasks]\ndefault = \"test\"\n",
        );
        let store = FilePermissionStore::new(&fs);

        store.save(root(), &cargo_test_policy()).unwrap();

        let text = String::from_utf8(
            fs.read_file(under_root(".termesh/workspace.toml").as_path()).unwrap(),
        )
        .unwrap();
        assert!(text.contains("# mine"));
        assert!(text.contains("[tasks]"));
        assert!(text.contains("[[agent.permissions.commands]]"));
    }

    #[test]
    fn saved_policy_round_trips_without_becoming_dirty() {
        let fs = FakeFileSystem::new();
        fs.add_dir(root());
        let store = FilePermissionStore::new(&fs);
        store.save(root(), &cargo_test_policy()).unwrap();

        let loaded = store.load(root()).unwrap();

        assert!(loaded.permits(root(), &spec("cargo", &["test"], ROOT, &[])));
        assert!(!loaded.is_dirty());
    }

    #[test]
    fn malformed_or_unsafe_config_is_rejected_and_never_overwritten() {
        let fs = FakeFileSystem::new();
        let path = under_root(".termesh/workspace.toml");
        fs.add_file(
            &path,
            b"[[agent.permissions.commands]]\nprogram = \"cargo\"\ncwd = \"../tmp\"\nargs = []\n",
        );
        let before = fs.read_file(&path).unwrap();
        let store = FilePermissionStore::new(&fs);

        assert!(store.load(root()).is_err());
        assert_eq!(fs.read_file(&path).unwrap(), before);
    }
}
