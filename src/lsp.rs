//! A minimal Language Server Protocol client: enough to keep documents in sync
//! and ask for completions. Servers run as child processes; reading and writing
//! happen on background threads so the UI never blocks on them.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::buffer::{LspChange, LspPos};

/// Which server handles a file, and where its project root is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub command: &'static str,
    pub root: PathBuf,
}

/// The language server for a file, if we know one: (server command, LSP language id).
pub fn server_for(path: &Path) -> Option<(&'static str, &'static str)> {
    match path.extension()?.to_str()? {
        "rs" => Some(("rust-analyzer", "rust")),
        "c" => Some(("clangd", "c")),
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h" | "ipp" | "inl" => {
            Some(("clangd", "cpp"))
        }
        _ => None,
    }
}

/// Project root for a file: the nearest folder with a project marker, else the
/// workspace folder, else the file's own folder.
pub fn root_for(path: &Path, command: &str, workspace: Option<&Path>) -> PathBuf {
    let markers: &[&str] = match command {
        "rust-analyzer" => &["Cargo.toml"],
        _ => &[
            "compile_commands.json",
            "compile_flags.txt",
            ".clangd",
            "CMakeLists.txt",
        ],
    };
    let mut dir = path.parent();
    while let Some(d) = dir {
        if markers.iter().any(|m| d.join(m).exists()) {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    workspace
        .filter(|w| path.starts_with(w))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf())
}

pub fn uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A completion suggestion from the server.
#[derive(Clone, Debug, Default)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    /// LSP `CompletionItemKind`.
    pub kind: u8,
    pub insert: String,
    pub filter: String,
    pub sort: String,
    /// Range to replace, in server positions (line, column).
    pub replace: Option<((usize, usize), (usize, usize))>,
    /// Extra edits (e.g. adding an import): (start, end, text) in server positions.
    pub additional: Vec<((usize, usize), (usize, usize), String)>,
}

/// A problem reported by the server. Positions are server positions (line, column).
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
    pub source: String,
}

/// A place in a file, in server positions.
#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

pub enum Event {
    Completion {
        id: i64,
        items: Vec<CompletionItem>,
        incomplete: bool,
    },
    /// The full current set of problems of one file (replaces earlier ones).
    Diagnostics {
        path: PathBuf,
        items: Vec<Diagnostic>,
    },
    Definition {
        id: i64,
        locations: Vec<Location>,
    },
    References {
        id: i64,
        locations: Vec<Location>,
    },
    /// The edits `textDocument/formatting` asks for, or why it failed.
    Formatting {
        id: i64,
        result: Result<Vec<TextEdit>, String>,
    },
    /// Hover documentation as Markdown (empty: nothing to show).
    Hover {
        id: i64,
        text: String,
    },
    /// The edits a rename needs, or why the server refused.
    Rename {
        id: i64,
        result: Result<Vec<FileEdit>, String>,
    },
}

/// A replacement in server positions: (start, end, new text).
pub type TextEdit = ((usize, usize), (usize, usize), String);

/// All edits of a workspace edit that fall into one file.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEdit {
    pub path: PathBuf,
    /// The document version the edits were computed for, if the server says.
    pub version: Option<i32>,
    pub edits: Vec<TextEdit>,
}

#[derive(Clone, Copy, PartialEq)]
enum RequestKind {
    Completion,
    Definition,
    References,
    Rename,
    Hover,
    Formatting,
}

enum Incoming {
    Message(Value),
    Closed,
}

pub struct Server {
    pub key: ServerKey,
    child: Child,
    out: Sender<Value>,
    incoming: Receiver<Incoming>,
    next_id: i64,
    initialize_id: i64,
    pub ready: bool,
    /// Messages waiting for the server to finish initializing.
    queued: Vec<Value>,
    /// Whether positions are in UTF-16 code units (the LSP default) instead of chars.
    pub utf16: bool,
    /// The server can format whole documents.
    pub formatting: bool,
    /// Open documents and their version numbers.
    pub opened: HashMap<u64, (String, i32)>,
    pub alive: bool,
    /// Last line the server wrote to stderr (for error messages).
    pub stderr: Arc<Mutex<String>>,
    /// What each outstanding request was, to parse its answer.
    pending: HashMap<i64, RequestKind>,
    ctx: egui::Context,
    workspace: Option<PathBuf>,
    /// Guessed C/C++ flags (when the project has no compilation database): the
    /// scan runs in the background and arrives here.
    flags_rx: Option<Receiver<Arc<crate::cppflags::Flags>>>,
    guess: Option<Arc<crate::cppflags::Flags>>,
    /// C/C++ files opened before the flags were ready: (path, language id).
    needs_flags: Vec<(PathBuf, String)>,
    /// clangd was pointed at a database generated from the build directory.
    has_compdb: bool,
}

impl Server {
    pub fn start(
        key: ServerKey,
        ctx: egui::Context,
        workspace: Option<PathBuf>,
        background_index: bool,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(key.command);
        if key.command == "clangd" {
            // Keep clangd's memory down: by default no background index of the whole project.
            cmd.arg(format!("--background-index={background_index}"));
            cmd.arg("--header-insertion=never");
        }
        // No compile_commands.json: a CMake/Ninja build directory still knows the real
        // flags. clangd only takes the database's location at initialization.
        let compdb = (key.command == "clangd"
            && !crate::cppflags::has_compile_db(&key.root.join("_")))
        .then(|| {
            let project = crate::cppflags::project_dir(&key.root.join("_"), workspace.as_deref());
            crate::cppflags::ninja_compdb(&project)
        })
        .flatten();
        let mut child = cmd
            .current_dir(&key.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");

        let (out, out_rx) = channel::<Value>();
        std::thread::spawn(move || {
            for msg in out_rx {
                let body = msg.to_string();
                if write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len())
                    .and_then(|_| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        });

        let (in_tx, incoming) = channel();
        let ctx2 = ctx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_message(&mut reader) {
                    Some(v) => {
                        if in_tx.send(Incoming::Message(v)).is_err() {
                            break;
                        }
                        ctx2.request_repaint();
                    }
                    None => {
                        let _ = in_tx.send(Incoming::Closed);
                        ctx2.request_repaint();
                        break;
                    }
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let last = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    *last.lock().expect("stderr lock") = line;
                }
            }
        });

        let mut server = Self {
            key,
            child,
            out,
            incoming,
            next_id: 1,
            initialize_id: 0,
            ready: false,
            queued: Vec::new(),
            utf16: true,
            formatting: false,
            opened: HashMap::new(),
            alive: true,
            stderr,
            pending: HashMap::new(),
            ctx: ctx.clone(),
            workspace,
            flags_rx: None,
            guess: None,
            needs_flags: Vec::new(),
            has_compdb: compdb.is_some(),
        };
        let root = uri(&server.key.root);
        server.initialize_id = server.next_id;
        server.next_id += 1;
        let _ = server.out.send(json!({
            "jsonrpc": "2.0",
            "id": server.initialize_id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root,
                "workspaceFolders": [{"uri": root, "name": "root"}],
                "clientInfo": {"name": "rsedit"},
                "initializationOptions": match &compdb {
                    Some(dir) => json!({"compilationDatabasePath": dir.to_string_lossy()}),
                    None => Value::Null,
                },
                "capabilities": {
                    "general": {"positionEncodings": ["utf-32", "utf-16"]},
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "publishDiagnostics": {"relatedInformation": false},
                        "definition": {"linkSupport": true},
                        "references": {},
                        "hover": {"contentFormat": ["markdown", "plaintext"]},
                        "formatting": {},
                        "rename": {"prepareSupport": false},
                        "completion": {
                            "completionItem": {
                                "snippetSupport": false,
                                "insertReplaceSupport": false,
                                "labelDetailsSupport": true,
                            },
                            "contextSupport": true,
                        },
                    },
                    "workspace": {"workspaceEdit": {"documentChanges": true}},
                    "window": {"workDoneProgress": false},
                },
            },
        }));
        Ok(server)
    }

    fn send(&mut self, msg: Value) {
        if self.ready {
            let _ = self.out.send(msg);
        } else {
            self.queued.push(msg);
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn pos(&self, p: LspPos) -> Value {
        json!({"line": p.line, "character": if self.utf16 { p.col16 } else { p.col }})
    }

    /// Without a compilation database clangd has no include paths. Guess them
    /// (like VSCode's C/C++ extension) and hand them over per file, in memory.
    fn guess_flags(&mut self, path: &Path, language: &str) {
        if self.key.command != "clangd" || self.has_compdb || crate::cppflags::has_compile_db(path)
        {
            return;
        }
        if self.guess.is_some() {
            // A database (if any) was already announced; guessed flags are per file.
            self.set_compile_commands(&[(path.to_path_buf(), language.to_string())]);
            return;
        }
        self.needs_flags
            .push((path.to_path_buf(), language.to_string()));
        if self.flags_rx.is_none() {
            let project = crate::cppflags::project_dir(path, self.workspace.as_deref());
            let (tx, rx) = channel();
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(crate::cppflags::compute_cached(&project));
                ctx.request_repaint();
            });
            self.flags_rx = Some(rx);
        }
    }

    fn set_compile_commands(&mut self, files: &[(PathBuf, String)]) {
        let Some(flags) = self.guess.clone() else {
            return;
        };
        let mut changes = serde_json::Map::new();
        for (path, lang) in files {
            let dir = path.parent().unwrap_or(path);
            changes.insert(
                path.to_string_lossy().into_owned(),
                json!({
                    "workingDirectory": dir.to_string_lossy(),
                    "compilationCommand": crate::cppflags::command(&flags, lang, path),
                }),
            );
        }
        self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": {"compilationDatabaseChanges": changes}}),
        );
    }

    pub fn did_open(&mut self, doc: u64, path: &Path, language: &str, text: String) {
        self.guess_flags(path, language);
        let u = uri(path);
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {"uri": u, "languageId": language, "version": 1, "text": text}}),
        );
        self.opened.insert(doc, (u, 1));
    }

    /// Sends edits; `None` means "here's the whole text again".
    pub fn did_change(
        &mut self,
        doc: u64,
        changes: Option<Vec<LspChange>>,
        full_text: impl FnOnce() -> String,
    ) {
        let Some((u, version)) = self.opened.get_mut(&doc) else {
            return;
        };
        *version += 1;
        let (u, version) = (u.clone(), *version);
        let content: Vec<Value> = match changes {
            Some(changes) => changes
                .into_iter()
                .map(|c| json!({"range": {"start": self.pos(c.start), "end": self.pos(c.end)}, "text": c.text}))
                .collect(),
            None => vec![json!({"text": full_text()})],
        };
        if !content.is_empty() {
            self.notify(
                "textDocument/didChange",
                json!({"textDocument": {"uri": u, "version": version}, "contentChanges": content}),
            );
        }
    }

    pub fn did_close(&mut self, doc: u64) {
        if let Some((u, _)) = self.opened.remove(&doc) {
            self.notify("textDocument/didClose", json!({"textDocument": {"uri": u}}));
        }
    }

    /// Asks for completions at a position; the answer arrives as an [`Event`].
    pub fn completion(&mut self, doc: u64, at: LspPos, trigger: Option<char>) -> Option<i64> {
        let (u, _) = self.opened.get(&doc)?.clone();
        let context = match trigger {
            Some(c) => json!({"triggerKind": 2, "triggerCharacter": c.to_string()}),
            None => json!({"triggerKind": 1}),
        };
        let pos = self.pos(at);
        let id = self.request(
            "textDocument/completion",
            json!({"textDocument": {"uri": u}, "position": pos, "context": context}),
        );
        self.pending.insert(id, RequestKind::Completion);
        Some(id)
    }

    /// Asks where the symbol at a position is defined; answered by [`Event::Definition`].
    pub fn definition(&mut self, doc: u64, at: LspPos) -> Option<i64> {
        let (u, _) = self.opened.get(&doc)?.clone();
        let pos = self.pos(at);
        let id = self.request(
            "textDocument/definition",
            json!({"textDocument": {"uri": u}, "position": pos}),
        );
        self.pending.insert(id, RequestKind::Definition);
        Some(id)
    }

    /// Asks for every use of the symbol at a position; answered by [`Event::References`].
    pub fn references(&mut self, doc: u64, at: LspPos) -> Option<i64> {
        let (u, _) = self.opened.get(&doc)?.clone();
        let pos = self.pos(at);
        let id = self.request(
            "textDocument/references",
            json!({"textDocument": {"uri": u}, "position": pos, "context": {"includeDeclaration": true}}),
        );
        self.pending.insert(id, RequestKind::References);
        Some(id)
    }

    /// Asks for the edits that format a whole document; answered by [`Event::Formatting`].
    pub fn formatting(&mut self, doc: u64, tab_size: usize, insert_spaces: bool) -> Option<i64> {
        let (u, _) = self.opened.get(&doc)?.clone();
        let id = self.request(
            "textDocument/formatting",
            json!({
                "textDocument": {"uri": u},
                "options": {"tabSize": tab_size, "insertSpaces": insert_spaces},
            }),
        );
        self.pending.insert(id, RequestKind::Formatting);
        Some(id)
    }

    /// Asks for the documentation of the symbol at a position; answered by [`Event::Hover`].
    pub fn hover(&mut self, doc: u64, at: LspPos) -> Option<i64> {
        let (u, _) = self.opened.get(&doc)?.clone();
        let pos = self.pos(at);
        let id = self.request(
            "textDocument/hover",
            json!({"textDocument": {"uri": u}, "position": pos}),
        );
        self.pending.insert(id, RequestKind::Hover);
        Some(id)
    }

    /// Asks for the edits that rename the symbol at a position; answered by [`Event::Rename`].
    pub fn rename(&mut self, doc: u64, at: LspPos, new_name: &str) -> Option<i64> {
        let (u, _) = self.opened.get(&doc)?.clone();
        let pos = self.pos(at);
        let id = self.request(
            "textDocument/rename",
            json!({"textDocument": {"uri": u}, "position": pos, "newName": new_name}),
        );
        self.pending.insert(id, RequestKind::Rename);
        Some(id)
    }

    /// Tells the server a file was saved (rust-analyzer runs `cargo check` then).
    pub fn did_save(&mut self, doc: u64) {
        if let Some((u, _)) = self.opened.get(&doc).cloned() {
            self.notify("textDocument/didSave", json!({"textDocument": {"uri": u}}));
        }
    }

    /// Handles everything the server sent since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(rx) = &self.flags_rx
            && let Ok(guess) = rx.try_recv()
        {
            self.guess = Some(guess);
            self.flags_rx = None;
            let files = std::mem::take(&mut self.needs_flags);
            self.set_compile_commands(&files);
        }
        while let Ok(msg) = self.incoming.try_recv() {
            let msg = match msg {
                Incoming::Message(m) => m,
                Incoming::Closed => {
                    self.alive = false;
                    continue;
                }
            };
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(Value::as_str);
            match (id, method) {
                // A request from the server: answer so it doesn't wait on us.
                (Some(id), Some(method)) => {
                    let result = match method {
                        "workspace/configuration" => {
                            let n = msg["params"]["items"].as_array().map_or(0, Vec::len);
                            Value::Array(vec![Value::Null; n])
                        }
                        _ => Value::Null,
                    };
                    let _ = self
                        .out
                        .send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                }
                (Some(id), None) => {
                    let id = id.as_i64().unwrap_or(-1);
                    if id == self.initialize_id {
                        let enc = msg["result"]["capabilities"]["positionEncoding"]
                            .as_str()
                            .unwrap_or("utf-16");
                        self.utf16 = enc != "utf-32";
                        let fp = &msg["result"]["capabilities"]["documentFormattingProvider"];
                        self.formatting = fp.as_bool().unwrap_or(fp.is_object());
                        self.ready = true;
                        let _ = self
                            .out
                            .send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
                        for m in std::mem::take(&mut self.queued) {
                            let _ = self.out.send(m);
                        }
                    } else {
                        // An error answer (e.g. a cancelled request) counts as "nothing found".
                        let result = msg.get("result").unwrap_or(&Value::Null);
                        match self.pending.remove(&id) {
                            Some(RequestKind::Completion) => {
                                let (items, incomplete) = parse_completion(result);
                                events.push(Event::Completion {
                                    id,
                                    items,
                                    incomplete,
                                });
                            }
                            Some(RequestKind::Definition) => {
                                events.push(Event::Definition {
                                    id,
                                    locations: parse_locations(result),
                                });
                            }
                            Some(RequestKind::References) => {
                                events.push(Event::References {
                                    id,
                                    locations: parse_locations(result),
                                });
                            }
                            Some(RequestKind::Formatting) => {
                                let result = match msg["error"]["message"].as_str() {
                                    Some(m) => Err(m.to_owned()),
                                    None => Ok(parse_text_edits(result)),
                                };
                                events.push(Event::Formatting { id, result });
                            }
                            Some(RequestKind::Hover) => {
                                events.push(Event::Hover {
                                    id,
                                    text: parse_hover(result),
                                });
                            }
                            Some(RequestKind::Rename) => {
                                // The server's reason matters here ("can't rename a keyword").
                                let result = match msg["error"]["message"].as_str() {
                                    Some(m) => Err(m.to_owned()),
                                    None => parse_workspace_edit(result),
                                };
                                events.push(Event::Rename { id, result });
                            }
                            None => {}
                        }
                    }
                }
                (None, Some("textDocument/publishDiagnostics")) => {
                    let params = &msg["params"];
                    if let Some(path) = params["uri"].as_str().and_then(path_of_uri) {
                        events.push(Event::Diagnostics {
                            path,
                            items: parse_diagnostics(&params["diagnostics"]),
                        });
                    }
                }
                _ => {} // other notifications (progress, logs) aren't used
            }
        }
        if self.alive
            && let Ok(Some(_)) = self.child.try_wait()
        {
            self.alive = false;
        }
        events
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self
            .out
            .send(json!({"jsonrpc": "2.0", "id": self.next_id, "method": "shutdown"}));
        let _ = self.out.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        // Give it a moment to exit cleanly, then make sure it's gone.
        for _ in 0..20 {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads one `Content-Length`-framed JSON message.
fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// The file path of a `file://` URI.
pub fn path_of_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

fn parse_diagnostics(v: &Value) -> Vec<Diagnostic> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| {
                    let (start, end) = range_of(&d["range"])?;
                    Some(Diagnostic {
                        start,
                        end,
                        severity: d["severity"].as_u64().unwrap_or(1) as u8,
                        message: d["message"].as_str().unwrap_or_default().to_owned(),
                        source: d["source"].as_str().unwrap_or_default().to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses `Location`, `Location[]` or `LocationLink[]`.
fn parse_locations(v: &Value) -> Vec<Location> {
    let one = |l: &Value| -> Option<Location> {
        // LocationLink: targetUri + targetSelectionRange; Location: uri + range.
        let (uri, range) = match l.get("targetUri") {
            Some(u) => (
                u,
                l.get("targetSelectionRange").unwrap_or(&l["targetRange"]),
            ),
            None => (&l["uri"], &l["range"]),
        };
        let (start, end) = range_of(range)?;
        Some(Location {
            path: path_of_uri(uri.as_str()?)?,
            start,
            end,
        })
    };
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn range_of(v: &Value) -> Option<((usize, usize), (usize, usize))> {
    let p = |v: &Value| {
        Some((
            v["line"].as_u64()? as usize,
            v["character"].as_u64()? as usize,
        ))
    };
    Some((p(&v["start"])?, p(&v["end"])?))
}

/// Parses a completion result (`CompletionItem[]` or `CompletionList`).
/// Longest hover text kept (servers send whole doc pages; tooltips can't scroll).
const HOVER_MAX_LINES: usize = 30;
const HOVER_MAX_BYTES: usize = 4096;

/// A hover answer as compact Markdown; empty when there's nothing to show.
pub fn parse_hover(v: &Value) -> String {
    let fence = |lang: &str, code: &str| format!("```{lang}\n{}\n```", code.trim_end());
    let part = |p: &Value| -> String {
        match p {
            Value::String(s) => s.clone(),
            Value::Object(o) => {
                let value = o.get("value").and_then(Value::as_str).unwrap_or_default();
                match (
                    o.get("language").and_then(Value::as_str),
                    o.get("kind").and_then(Value::as_str),
                ) {
                    (Some(lang), _) => fence(lang, value),
                    (None, Some("plaintext")) => fence("", value),
                    _ => value.to_owned(),
                }
            }
            _ => String::new(),
        }
    };
    let contents = &v["contents"];
    let raw = match contents {
        Value::Array(a) => a.iter().map(part).collect::<Vec<_>>().join("\n\n"),
        other => part(other),
    };
    compact_markdown(&raw)
}

/// Makes server Markdown fit a tooltip: headings become bold, stray rules go, and
/// the text is cut to a few lines (closing a code fence the cut left open).
fn compact_markdown(raw: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    for line in raw.lines() {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence {
            if t.trim() == "---" {
                if lines.last().is_none_or(|l| l.is_empty() || l == "---") {
                    continue;
                }
                lines.push("---".into());
                continue;
            }
            let hashes = t.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
                lines.push(format!("**{}**", t[hashes..].trim()));
                continue;
            }
        }
        lines.push(t.to_owned());
    }
    while lines.last().is_some_and(|l| l.is_empty() || l == "---") {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty() || l == "---") {
        lines.remove(0);
    }
    let mut out = String::new();
    let mut open = false;
    let mut cut = false;
    for (i, l) in lines.iter().enumerate() {
        if i >= HOVER_MAX_LINES || out.len() + l.len() > HOVER_MAX_BYTES {
            cut = true;
            break;
        }
        if l.trim_start().starts_with("```") {
            open = !open;
        }
        out.push_str(l);
        out.push('\n');
    }
    if open {
        out.push_str("```\n");
    }
    if cut {
        out.push_str("…\n");
    }
    out.trim().to_owned()
}

fn parse_text_edits(v: &Value) -> Vec<TextEdit> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let (s, e2) = range_of(&e["range"])?;
                    Some((s, e2, e["newText"].as_str()?.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Reads a `WorkspaceEdit`. File creation, renaming and deletion aren't supported.
pub fn parse_workspace_edit(v: &Value) -> Result<Vec<FileEdit>, String> {
    let mut files = Vec::new();
    if let Some(changes) = v["documentChanges"].as_array() {
        for c in changes {
            if c.get("kind").is_some() {
                return Err("Rename needs file operations, which aren't supported".into());
            }
            let Some(path) = c["textDocument"]["uri"].as_str().and_then(path_of_uri) else {
                continue;
            };
            files.push(FileEdit {
                path,
                version: c["textDocument"]["version"].as_i64().map(|v| v as i32),
                edits: parse_text_edits(&c["edits"]),
            });
        }
    } else if let Some(changes) = v["changes"].as_object() {
        for (uri, edits) in changes {
            if let Some(path) = path_of_uri(uri) {
                files.push(FileEdit {
                    path,
                    version: None,
                    edits: parse_text_edits(edits),
                });
            }
        }
    }
    files.retain(|f| !f.edits.is_empty());
    Ok(files)
}

pub fn parse_completion(result: &Value) -> (Vec<CompletionItem>, bool) {
    let (list, incomplete) = match result {
        Value::Array(a) => (a.as_slice(), false),
        Value::Object(_) => (
            result["items"].as_array().map_or(&[][..], Vec::as_slice),
            result["isIncomplete"].as_bool().unwrap_or(false),
        ),
        _ => (&[][..], false),
    };
    let items = list
        .iter()
        .map(|it| {
            let s = |k: &str| it[k].as_str().unwrap_or_default().to_owned();
            let label = s("label");
            let edit = &it["textEdit"];
            let (replace, edit_text) = if edit.is_object() {
                // TextEdit has `range`; InsertReplaceEdit has `insert`/`replace`.
                let r = range_of(&edit["range"]).or_else(|| range_of(&edit["replace"]));
                (r, edit["newText"].as_str().map(str::to_owned))
            } else {
                (None, None)
            };
            let insert = edit_text
                .or_else(|| it["insertText"].as_str().map(str::to_owned))
                .unwrap_or_else(|| label.clone());
            let mut detail = s("detail");
            if detail.is_empty() {
                detail = it["labelDetails"]["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
            }
            let additional = parse_text_edits(&it["additionalTextEdits"]);
            CompletionItem {
                filter: it["filterText"]
                    .as_str()
                    .map_or_else(|| label.trim().to_owned(), str::to_owned),
                sort: it["sortText"]
                    .as_str()
                    .map_or_else(|| label.clone(), str::to_owned),
                kind: it["kind"].as_u64().unwrap_or(0) as u8,
                detail,
                insert,
                replace,
                additional,
                label: label.trim().to_owned(),
            }
        })
        .collect();
    (items, incomplete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_parses_completions() {
        let body = r#"{"jsonrpc":"2.0","id":3,"result":{"isIncomplete":true,"items":[
            {"label":" push","kind":2,"detail":"fn(&mut self, T)","textEdit":{"range":{"start":{"line":4,"character":6},"end":{"line":4,"character":8}},"newText":"push"},"sortText":"a"},
            {"label":"HashMap","kind":22,"insertText":"HashMap","additionalTextEdits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use std::collections::HashMap;\n"}]}
        ]}}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let msg = read_message(&mut BufReader::new(framed.as_bytes())).unwrap();
        let (items, incomplete) = parse_completion(&msg["result"]);
        assert!(incomplete);
        assert_eq!(items[0].label, "push");
        assert_eq!(items[0].replace, Some(((4, 6), (4, 8))));
        assert_eq!(items[1].additional.len(), 1);
    }

    #[test]
    fn parses_hover_shapes() {
        let md: Value = json!({"contents": {"kind": "markdown", "value": "### function `f`\n\n---\n```cpp\nint f()\n```\n---"}});
        assert_eq!(parse_hover(&md), "**function `f`**\n\n```cpp\nint f()\n```");
        let plain: Value = json!({"contents": {"kind": "plaintext", "value": "int x"}});
        assert_eq!(parse_hover(&plain), "```\nint x\n```");
        let marked: Value = json!({"contents": ["docs", {"language": "rust", "value": "fn f()"}]});
        assert_eq!(parse_hover(&marked), "docs\n\n```rust\nfn f()\n```");
        assert_eq!(parse_hover(&json!({"contents": "just text"})), "just text");
        assert_eq!(parse_hover(&Value::Null), "");
        // Long answers are cut, and an open code fence is closed.
        let long = format!("```\n{}", "line\n".repeat(100));
        let cut = parse_hover(&json!({"contents": {"kind": "markdown", "value": long}}));
        assert!(cut.lines().count() <= HOVER_MAX_LINES + 3);
        assert!(cut.ends_with("```\n…"), "{cut}");
        // A `#` inside a code fence stays.
        let code = parse_hover(
            &json!({"contents": {"kind": "markdown", "value": "```c\n# define X\n```"}}),
        );
        assert!(code.contains("# define X"));
    }

    #[test]
    fn parses_workspace_edits() {
        let changes: Value = serde_json::from_str(
            r#"{"changes":{"file:///a/x.rs":[{"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":5}},"newText":"foo"}],"file:///a/y.rs":[]}}"#,
        )
        .unwrap();
        let files = parse_workspace_edit(&changes).unwrap();
        assert_eq!(files.len(), 1, "files without edits are dropped");
        assert_eq!(files[0].path, PathBuf::from("/a/x.rs"));
        assert_eq!(files[0].edits, vec![((1, 2), (1, 5), "foo".to_owned())]);

        let versioned: Value = serde_json::from_str(
            r#"{"documentChanges":[{"textDocument":{"uri":"file:///a/x.rs","version":7},"edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"b","annotationId":"x"}]}]}"#,
        )
        .unwrap();
        let files = parse_workspace_edit(&versioned).unwrap();
        assert_eq!(files[0].version, Some(7));

        let rename_file: Value = serde_json::from_str(
            r#"{"documentChanges":[{"kind":"rename","oldUri":"file:///a","newUri":"file:///b"}]}"#,
        )
        .unwrap();
        assert!(parse_workspace_edit(&rename_file).is_err());
        assert_eq!(parse_workspace_edit(&Value::Null), Ok(vec![]));
    }

    #[test]
    fn parses_diagnostics_and_locations() {
        let d: Value = serde_json::from_str(
            r#"[{"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":9}},"severity":2,"message":"unused","source":"rustc"}]"#,
        )
        .unwrap();
        let ds = parse_diagnostics(&d);
        assert_eq!(ds[0].start, (2, 4));
        assert_eq!((ds[0].severity, ds[0].message.as_str()), (2, "unused"));
        let link: Value = serde_json::from_str(
            r#"[{"targetUri":"file:///p/a.rs","targetRange":{"start":{"line":1,"character":0},"end":{"line":5,"character":1}},"targetSelectionRange":{"start":{"line":1,"character":3},"end":{"line":1,"character":6}}}]"#,
        )
        .unwrap();
        assert_eq!(
            parse_locations(&link),
            vec![Location {
                path: "/p/a.rs".into(),
                start: (1, 3),
                end: (1, 6)
            }]
        );
        let loc: Value = serde_json::from_str(r#"{"uri":"file:///q.cpp","range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}}}"#).unwrap();
        assert_eq!(parse_locations(&loc)[0].path, PathBuf::from("/q.cpp"));
    }

    #[test]
    fn roots_and_uris() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(
            root_for(&here.join("src/lsp.rs"), "rust-analyzer", None),
            here
        );
        assert_eq!(uri(Path::new("/a b/c#.rs")), "file:///a%20b/c%23.rs");
        assert_eq!(
            path_of_uri("file:///a%20b/c%23.rs"),
            Some(PathBuf::from("/a b/c#.rs"))
        );
        assert_eq!(
            path_of_uri("file:///x/%C3%A4.rs"),
            Some(PathBuf::from("/x/ä.rs"))
        );
        assert_eq!(server_for(Path::new("x.hpp")), Some(("clangd", "cpp")));
    }

    /// End to end against the real clangd and the real vampir checkout.
    #[test]
    #[ignore]
    fn clangd_resolves_includes_without_compile_db() {
        let file = Path::new(
            "/home/cedric/IAK/vampir/server/components/protocol/include/protocol/CallTree.hpp",
        );
        let key = ServerKey {
            command: "clangd",
            root: root_for(file, "clangd", None),
        };
        let mut s = Server::start(key, egui::Context::default(), None, false).unwrap();
        s.did_open(1, file, "cpp", std::fs::read_to_string(file).unwrap());
        let start = std::time::Instant::now();
        let mut last = None;
        while start.elapsed().as_secs() < 40 {
            for e in s.poll() {
                if let Event::Diagnostics { path, items } = e
                    && path == file
                {
                    println!("{} diagnostics", items.len());
                    for d in &items {
                        println!("  {}:{} {}", d.start.0 + 1, d.start.1, d.message);
                    }
                    last = Some(items);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let items = last.expect("diagnostics arrive");
        assert!(
            !items.iter().any(|d| d.message.contains("not found")),
            "{items:?}"
        );
    }

    /// References and rename against the real clangd on a two-file project.
    #[test]
    #[ignore]
    fn clangd_references_and_rename() {
        let dir = std::env::temp_dir().join(format!("rsedit-clangd-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        // The background index only knows files listed in a compilation database.
        std::fs::write(
            dir.join("compile_commands.json"),
            json!([{"directory": dir, "command": "clang++ -std=c++20 -c main.cpp", "file": "main.cpp"}])
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("util.hpp"),
            "inline int helper(int x) { return x + 1; }\n",
        )
        .unwrap();
        let main = dir.join("main.cpp");
        let text = "#include \"util.hpp\"\nint main() { return helper(1) + helper(2); }\n";
        std::fs::write(&main, text).unwrap();
        let key = ServerKey {
            command: "clangd",
            root: dir.clone(),
        };
        let mut s = Server::start(key, egui::Context::default(), None, true).unwrap();
        s.did_open(1, &main, "cpp", text.to_owned());
        let at = LspPos {
            line: 1,
            col: 22,
            col16: 22,
        };
        let (mut refs, mut rename) = (Vec::new(), None);
        let start = std::time::Instant::now();
        let mut last_ask = std::time::Instant::now();
        while start.elapsed().as_secs() < 40 && rename.is_none() {
            // The background index needs a moment: ask again until all uses are known.
            if s.ready && s.pending.is_empty() && last_ask.elapsed().as_secs() >= 2 {
                last_ask = std::time::Instant::now();
                if refs.len() < 3 {
                    s.references(1, at);
                } else {
                    s.rename(1, at, "twice");
                }
            }
            for e in s.poll() {
                match e {
                    Event::References { locations, .. } => refs = locations,
                    Event::Rename { result, .. } => rename = Some(result),
                    _ => {}
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(refs.len(), 3, "{refs:?}"); // declaration + two calls
        let files = rename.expect("rename answered").unwrap();
        let total: usize = files.iter().map(|f| f.edits.len()).sum();
        assert_eq!(total, 3, "{files:?}");
        assert_eq!(files.len(), 2);
        assert!(files.iter().flat_map(|f| &f.edits).all(|e| e.2 == "twice"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hover against the real clangd.
    #[test]
    #[ignore]
    fn clangd_hover_end_to_end() {
        let dir = std::env::temp_dir().join(format!("rsedit-clangd-hover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("compile_flags.txt"), "-std=c++20\n").unwrap();
        let main = dir.join("main.cpp");
        let text =
            "// Adds one.\nint helper(int x) { return x + 1; }\nint main() { return helper(1); }\n";
        std::fs::write(&main, text).unwrap();
        let key = ServerKey {
            command: "clangd",
            root: dir.clone(),
        };
        let mut s = Server::start(key, egui::Context::default(), None, false).unwrap();
        s.did_open(1, &main, "cpp", text.to_owned());
        let at = LspPos {
            line: 2,
            col: 22,
            col16: 22,
        };
        let mut answer = None;
        let start = std::time::Instant::now();
        let mut last_ask = std::time::Instant::now() - std::time::Duration::from_secs(5);
        while start.elapsed().as_secs() < 30 && answer.is_none() {
            if s.ready && s.pending.is_empty() && last_ask.elapsed().as_secs() >= 2 {
                last_ask = std::time::Instant::now();
                s.hover(1, at);
            }
            for e in s.poll() {
                if let Event::Hover { text, .. } = e
                    && !text.is_empty()
                {
                    answer = Some(text);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let text = answer.expect("hover answered");
        println!("{text}");
        assert!(text.contains("helper"), "{text}");
        assert!(text.contains("```cpp"), "{text}");
        assert!(!text.contains("###"), "headings are made compact: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
