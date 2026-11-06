use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::highlight::SyntaxCache;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// One open buffer (a tab).
pub struct Document {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub text: String,
    /// Highlighting state, shared by every group showing this document.
    pub syntax: SyntaxCache,
    /// Text as it was last loaded/saved, used for the dirty marker.
    saved: String,
    untitled_index: usize,
}

impl Document {
    pub fn new_untitled(untitled_index: usize) -> Self {
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            path: None,
            text: String::new(),
            syntax: SyntaxCache::default(),
            saved: String::new(),
            untitled_index,
        }
    }

    pub fn open(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "file is not valid UTF-8")
        })?;
        // Normalise CRLF so the editor deals with a single line ending.
        let text = text.replace("\r\n", "\n");
        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            path: Some(path.to_path_buf()),
            saved: text.clone(),
            text,
            syntax: SyntaxCache::default(),
            untitled_index: 0,
        })
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| std::io::Error::other("no path"))?;
        std::fs::write(path, &self.text)?;
        self.saved = self.text.clone();
        Ok(())
    }

    pub fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.path = Some(path);
        self.save()
    }

    pub fn is_dirty(&self) -> bool {
        self.text != self.saved
    }

    pub fn title(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => format!("Untitled-{}", self.untitled_index),
        }
    }

    /// Extension used by syntect to pick a grammar.
    pub fn syntax_token(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| {
                p.extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            })
            .unwrap_or_else(|| "txt".to_owned())
    }

    pub fn language_name(&self) -> &'static str {
        match self.syntax_token().as_str() {
            "rs" => "Rust",
            "py" => "Python",
            "js" | "mjs" | "cjs" => "JavaScript",
            "ts" | "tsx" => "TypeScript",
            "json" => "JSON",
            "toml" => "TOML",
            "yaml" | "yml" => "YAML",
            "md" => "Markdown",
            "html" | "htm" => "HTML",
            "css" => "CSS",
            "c" | "h" => "C",
            "cpp" | "cc" | "hpp" => "C++",
            "go" => "Go",
            "java" => "Java",
            "sh" | "bash" | "zsh" => "Shell",
            "lua" => "Lua",
            "rb" => "Ruby",
            "xml" => "XML",
            "sql" => "SQL",
            _ => "Plain Text",
        }
    }
}

/// Convert a char index into (line, column), both 1-based.
pub fn line_col(text: &str, char_index: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for c in text.chars().take(char_index) {
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}
