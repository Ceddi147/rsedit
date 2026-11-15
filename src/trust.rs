//! Which folders may run language servers.
//!
//! A language server executes code from the project it analyses: rust-analyzer
//! runs Cargo, which builds and runs build scripts and proc macros and follows
//! the project's `.cargo/config.toml`. Opening a file must not be enough to run
//! a stranger's code, so servers only start in folders the user has trusted.
//! Trusted folders are kept in `trusted-folders` in rsedit's config directory,
//! one absolute path per line; subfolders of a trusted folder are trusted too.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Trust {
    trusted: Vec<PathBuf>,
    /// Folders the user said "not now" to in this run: don't ask again.
    declined: HashSet<PathBuf>,
    /// Trust everything (tests).
    everything: bool,
}

fn file() -> Option<PathBuf> {
    Some(crate::settings::config_dir()?.join("trusted-folders"))
}

/// The canonical form of `p`, so symlinks and `..` can't sidestep a check.
fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

impl Trust {
    pub fn load() -> Self {
        let text = file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .unwrap_or_default();
        Self::from_text(&text)
    }

    fn from_text(text: &str) -> Self {
        let trusted = text
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('/'))
            .map(PathBuf::from)
            .collect();
        Self {
            trusted,
            ..Self::default()
        }
    }

    /// Trusts every folder; for tests, which start real servers.
    pub fn everything() -> Self {
        Self {
            everything: true,
            ..Self::default()
        }
    }

    pub fn is_trusted(&self, dir: &Path) -> bool {
        if self.everything {
            return true;
        }
        let dir = canon(dir);
        self.trusted.iter().any(|t| dir.starts_with(t))
    }

    pub fn is_declined(&self, dir: &Path) -> bool {
        self.declined.contains(&canon(dir))
    }

    pub fn decline(&mut self, dir: &Path) {
        self.declined.insert(canon(dir));
    }

    /// Forgets "not now" answers, so the next server start asks again.
    pub fn ask_again(&mut self) {
        self.declined.clear();
    }

    /// Trusts `dir` (and everything below it) from now on, also in later runs.
    pub fn trust(&mut self, dir: &Path) -> std::io::Result<()> {
        let dir = canon(dir);
        self.declined.remove(&dir);
        if self.trusted.iter().any(|t| dir.starts_with(t)) {
            return Ok(());
        }
        self.trusted.push(dir);
        let Some(path) = file() else { return Ok(()) };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for t in &self.trusted {
            // A newline in a path would split it into two entries.
            if let Some(s) = t.to_str().filter(|s| !s.contains(['\n', '\r'])) {
                text.push_str(s);
                text.push('\n');
            }
        }
        std::fs::write(path, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subfolders_of_trusted_folders_are_trusted() {
        let t = Trust::from_text("/home/me/code\n\nnot-absolute\n");
        assert!(t.is_trusted(Path::new("/home/me/code")));
        assert!(t.is_trusted(Path::new("/home/me/code/rsedit/src")));
        assert!(!t.is_trusted(Path::new("/home/me/codex")));
        assert!(!t.is_trusted(Path::new("/home/me")));
        assert!(!t.is_trusted(Path::new("not-absolute")));
        assert!(Trust::everything().is_trusted(Path::new("/anywhere")));
        assert!(!Trust::default().is_trusted(Path::new("/anywhere")));
    }

    #[test]
    fn declined_folders_are_remembered_until_asked_again() {
        let mut t = Trust::default();
        let dir = Path::new("/nonexistent/project");
        assert!(!t.is_declined(dir));
        t.decline(dir);
        assert!(t.is_declined(dir));
        t.ask_again();
        assert!(!t.is_declined(dir));
    }
}
