use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use termesh_filesystem::FsError;
use termesh_workspace::{PermissionPolicy, PermissionStore};

#[derive(Debug, Default)]
pub struct FakePermissionStore {
    policies: Mutex<BTreeMap<PathBuf, PermissionPolicy>>,
    load_error: Mutex<Option<FsError>>,
    save_error: Mutex<Option<FsError>>,
}

impl FakePermissionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_policy(&self, root: impl Into<PathBuf>, mut policy: PermissionPolicy) -> &Self {
        policy.mark_saved();
        self.policies.lock().unwrap().insert(root.into(), policy);
        self
    }

    pub fn fail_load(&self, error: FsError) -> &Self {
        *self.load_error.lock().unwrap() = Some(error);
        self
    }

    pub fn fail_save(&self, error: FsError) -> &Self {
        *self.save_error.lock().unwrap() = Some(error);
        self
    }

    pub fn policy(&self, root: &Path) -> Option<PermissionPolicy> {
        self.policies.lock().unwrap().get(root).cloned()
    }
}

impl PermissionStore for FakePermissionStore {
    fn load(&self, root: &Path) -> Result<PermissionPolicy, FsError> {
        if let Some(error) = self.load_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(self.policies.lock().unwrap().get(root).cloned().unwrap_or_default())
    }

    fn save(&self, root: &Path, policy: &PermissionPolicy) -> Result<(), FsError> {
        if let Some(error) = self.save_error.lock().unwrap().clone() {
            return Err(error);
        }
        let mut policy = policy.clone();
        policy.mark_saved();
        self.policies.lock().unwrap().insert(root.to_path_buf(), policy);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use termesh_core::TerminalSpec;
    use termesh_workspace::PermissionStore;

    /// `/proj` is rooted but not absolute on Windows, and grant checks rightly refuse a
    /// non-absolute cwd, so this fixture needs a root the host agrees is absolute.
    const ROOT: &str = if cfg!(windows) { r"C:\proj" } else { "/proj" };

    #[test]
    fn fake_round_trips_policies_by_workspace() {
        let store = FakePermissionStore::new();
        let mut policy = termesh_workspace::PermissionPolicy::default();
        policy.remember(
            Path::new(ROOT),
            &TerminalSpec {
                program: "cargo".into(),
                args: vec!["test".into()],
                cwd: ROOT.into(),
                env: Vec::new(),
            },
        );
        store.save(Path::new(ROOT), &policy).unwrap();
        assert!(store.load(Path::new(ROOT)).unwrap().permits(
            Path::new(ROOT),
            &TerminalSpec {
                program: "cargo".into(),
                args: vec!["test".into()],
                cwd: ROOT.into(),
                env: Vec::new(),
            }
        ));
    }
}
