//! Remembers the open folder, editor groups, tabs and cursor positions between
//! runs, in a small tab-separated file under the user's config directory.

use std::path::PathBuf;

#[derive(Debug, Default, PartialEq)]
pub struct Session {
    pub workspace: Option<PathBuf>,
    pub groups: Vec<Group>,
    pub focused: usize,
    pub sidebar: bool,
    /// "explorer", "search" or "git".
    pub sidebar_view: String,
    pub dark: bool,
    /// Line numbers count from the caret line instead of the file's start.
    pub relative_numbers: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Group {
    pub width: f32,
    pub active: usize,
    /// Files with their caret position (char index).
    pub tabs: Vec<(PathBuf, usize)>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rsedit").join("session.tsv"))
}

impl Session {
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "v1\nsidebar\t{}\nview\t{}\ndark\t{}\nrelnum\t{}\nfocused\t{}\n",
            self.sidebar, self.sidebar_view, self.dark, self.relative_numbers, self.focused
        );
        // One entry per line: a path with a line break (legal on Linux) would
        // add lines of its own, so such paths aren't remembered.
        let line_safe = |p: &PathBuf| {
            let s = p.display().to_string();
            (!s.contains(['\n', '\r'])).then_some(s)
        };
        if let Some(w) = self.workspace.as_ref().and_then(line_safe) {
            out += &format!("workspace\t{w}\n");
        }
        for g in &self.groups {
            out += &format!("group\t{}\t{}\n", g.width, g.active);
            for (p, caret) in &g.tabs {
                if let Some(p) = line_safe(p) {
                    out += &format!("tab\t{caret}\t{p}\n");
                }
            }
        }
        out
    }

    pub fn from_text(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != "v1" {
            return None;
        }
        let mut s = Session::default();
        for line in lines {
            let mut f = line.splitn(3, '\t');
            match (f.next(), f.next(), f.next()) {
                (Some("sidebar"), Some(v), _) => s.sidebar = v == "true",
                (Some("dark"), Some(v), _) => s.dark = v == "true",
                (Some("relnum"), Some(v), _) => s.relative_numbers = v == "true",
                (Some("view"), Some(v), _) => s.sidebar_view = v.to_owned(),
                (Some("focused"), Some(v), _) => s.focused = v.parse().unwrap_or(0),
                (Some("workspace"), Some(p), None) => s.workspace = Some(PathBuf::from(p)),
                (Some("workspace"), Some(a), Some(b)) => {
                    s.workspace = Some(PathBuf::from(format!("{a}\t{b}")))
                }
                (Some("group"), Some(w), Some(a)) => s.groups.push(Group {
                    width: w
                        .parse()
                        .ok()
                        .filter(|w: &f32| w.is_finite() && *w > 0.0)
                        .unwrap_or(1.0),
                    active: a.parse().unwrap_or(0),
                    tabs: Vec::new(),
                }),
                (Some("tab"), Some(c), Some(p)) => {
                    if let Some(g) = s.groups.last_mut() {
                        g.tabs.push((PathBuf::from(p), c.parse().unwrap_or(0)));
                    }
                }
                _ => {}
            }
        }
        Some(s)
    }

    pub fn load() -> Option<Self> {
        Self::from_text(&std::fs::read_to_string(path()?).ok()?)
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, self.to_text());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = Session {
            workspace: Some("/home/me/project".into()),
            groups: vec![
                Group {
                    width: 0.5,
                    active: 1,
                    tabs: vec![("/a b/c.rs".into(), 10), ("/x.txt".into(), 0)],
                },
                Group {
                    width: 0.5,
                    active: 0,
                    tabs: vec![("/y.rs".into(), 3)],
                },
            ],
            focused: 1,
            sidebar: true,
            sidebar_view: "git".into(),
            dark: false,
            relative_numbers: true,
        };
        assert_eq!(Session::from_text(&s.to_text()), Some(s));
        assert_eq!(Session::from_text("garbage"), None);
    }

    #[test]
    fn paths_with_line_breaks_cannot_add_entries() {
        let s = Session {
            workspace: Some("/w".into()),
            groups: vec![Group {
                width: 1.0,
                active: 0,
                tabs: vec![
                    ("/ok.rs".into(), 1),
                    ("/x\nworkspace\t/elsewhere".into(), 0),
                ],
            }],
            ..Session::default()
        };
        let back = Session::from_text(&s.to_text()).unwrap();
        assert_eq!(back.workspace, Some("/w".into()));
        assert_eq!(back.groups[0].tabs, vec![("/ok.rs".into(), 1)]);
        let odd = Session::from_text("v1\ngroup\tNaN\t0\ngroup\t-inf\t0\n").unwrap();
        assert!(odd.groups.iter().all(|g| g.width == 1.0));
    }
}
