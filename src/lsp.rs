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
}

#[derive(Clone, Copy, PartialEq)]
enum RequestKind {
    Completion,
    Definition,
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
    /// Open documents and their version numbers.
    pub opened: HashMap<u64, (String, i32)>,
    pub alive: bool,
    /// Last line the server wrote to stderr (for error messages).
    pub stderr: Arc<Mutex<String>>,
    /// What each outstanding request was, to parse its answer.
    pending: HashMap<i64, RequestKind>,
}

impl Server {
    pub fn start(key: ServerKey, ctx: egui::Context) -> std::io::Result<Self> {
        let mut cmd = Command::new(key.command);
        if key.command == "clangd" {
            // Keep clangd's memory down: no background index of the whole project.
            cmd.args(["--background-index=false", "--header-insertion=never"]);
        }
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
            opened: HashMap::new(),
            alive: true,
            stderr,
            pending: HashMap::new(),
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
                "capabilities": {
                    "general": {"positionEncodings": ["utf-32", "utf-16"]},
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "publishDiagnostics": {"relatedInformation": false},
                        "definition": {"linkSupport": true},
                        "completion": {
                            "completionItem": {
                                "snippetSupport": false,
                                "insertReplaceSupport": false,
                                "labelDetailsSupport": true,
                            },
                            "contextSupport": true,
                        },
                    },
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

    pub fn did_open(&mut self, doc: u64, path: &Path, language: &str, text: String) {
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

    /// Tells the server a file was saved (rust-analyzer runs `cargo check` then).
    pub fn did_save(&mut self, doc: u64) {
        if let Some((u, _)) = self.opened.get(&doc).cloned() {
            self.notify("textDocument/didSave", json!({"textDocument": {"uri": u}}));
        }
    }

    /// Handles everything the server sent since the last call.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
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
            let additional = it["additionalTextEdits"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let (s, e2) = range_of(&e["range"])?;
                            Some((s, e2, e["newText"].as_str()?.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default();
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
}
