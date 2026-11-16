//! Guesses compiler flags for C/C++ files when a project has no compilation
//! database, so clangd can resolve `#include`s without manual setup.
//!
//! Two sources, like VSCode's C/C++ extension: `.vscode/c_cpp_properties.json`
//! if the project has one, else a scan of the project for header directories.
//! Nothing is written into the project: the flags are sent to clangd in memory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

const MAX_DIRS: usize = 20_000;
const MAX_DEPTH: usize = 16;
const MAX_FLAGS: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Flags {
    pub std_c: Option<String>,
    pub std_cpp: Option<String>,
    pub args: Vec<String>,
}

/// Whether clangd will find a real compilation database for this file.
pub fn has_compile_db(file: &Path) -> bool {
    file.ancestors().skip(1).any(|d| {
        d.join("compile_commands.json").exists()
            || d.join("build/compile_commands.json").exists()
            || d.join("compile_flags.txt").exists()
    })
}

/// The directory whose headers belong to this file's project.
pub fn project_dir(file: &Path, workspace: Option<&Path>) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dirs: Vec<&Path> = file
        .ancestors()
        .skip(1)
        .take_while(|d| Some(*d) != home.as_deref() && d.parent().is_some())
        .collect();
    if let Some(d) = dirs
        .iter()
        .find(|d| d.join(".vscode/c_cpp_properties.json").is_file())
    {
        return d.to_path_buf();
    }
    if let Some(d) = dirs.iter().find(|d| d.join(".git").exists()) {
        return d.to_path_buf();
    }
    workspace
        .filter(|w| file.starts_with(w))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file.parent().unwrap_or(file).to_path_buf())
}

/// Removes `//` and `/* */` comments and trailing commas from JSONC.
fn strip_jsonc(text: &str) -> String {
    let b: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == ',' {
            // A comma directly before `}` or `]` is a trailing comma.
            let next = b[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

struct VsConfig {
    /// (directory, recursive)
    includes: Vec<(PathBuf, bool)>,
    flags: Flags,
}

fn expand(s: &str, ws: &Path) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut s = s
        .replace("${workspaceFolder}", &ws.to_string_lossy())
        .replace("${workspaceRoot}", &ws.to_string_lossy())
        .replace("${userHome}", &home);
    if let Some(rest) = s.strip_prefix("~/") {
        s = format!("{home}/{rest}");
    }
    while let Some(start) = s.find("${env:") {
        let end = start + s[start..].find('}')?;
        let val = std::env::var(&s[start + 6..end]).unwrap_or_default();
        s.replace_range(start..=end, &val);
    }
    (!s.contains("${")).then_some(s)
}

fn std_flag(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    (!s.is_empty() && !s.contains("${")).then(|| format!("-std={s}"))
}

fn parse_vscode(text: &str, ws: &Path) -> Option<VsConfig> {
    let json: Value = serde_json::from_str(&strip_jsonc(text)).ok()?;
    let configs = json.get("configurations")?.as_array()?;
    let cfg = configs
        .iter()
        .find(|c| c["name"] == "Linux")
        .or_else(|| configs.first())?;
    let strings = |key: &str| -> Vec<String> {
        cfg[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut includes = Vec::new();
    for p in strings("includePath") {
        let Some(p) = expand(&p, ws) else { continue };
        match p.strip_suffix("/**") {
            Some(base) => includes.push((PathBuf::from(base), true)),
            None => includes.push((PathBuf::from(p), false)),
        }
    }
    let mut flags = Flags {
        std_c: std_flag(&cfg["cStandard"]),
        std_cpp: std_flag(&cfg["cppStandard"]),
        args: Vec::new(),
    };
    for d in strings("defines") {
        flags.args.push(format!("-D{d}"));
    }
    for f in strings("forcedInclude") {
        if let Some(f) = expand(&f, ws) {
            flags.args.push("-include".into());
            flags.args.push(f);
        }
    }
    flags.args.extend(strings("compilerArgs"));
    Some(VsConfig { includes, flags })
}

fn is_header(name: &str) -> bool {
    matches!(
        name.rsplit_once('.').map(|(_, e)| e),
        Some("h" | "hpp" | "hh" | "hxx" | "ipp" | "inl")
    )
}

fn skipped_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "node_modules"
            | "target"
            | ".cache"
            | ".idea"
            | ".vscode"
            | "__pycache__"
            | "build"
    ) || name.starts_with("build-")
        || name.starts_with("cmake-build")
}

/// Finds directories named `include` and other directories that hold headers.
/// Bounded in size and depth; never follows symlinks.
fn scan(root: &Path, max_dirs: usize) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut includes, mut headers) = (Vec::new(), Vec::new());
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut seen = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        seen += 1;
        if seen > max_dirs || includes.len() + headers.len() >= MAX_FLAGS {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let (mut subdirs, mut has_header, mut nested_repo) = (Vec::new(), false, false);
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if name == ".git" {
                    nested_repo = true;
                } else if !skipped_dir(&name) {
                    subdirs.push(e.path());
                }
            } else if ft.is_file() {
                if name == ".git" {
                    nested_repo = true; // a worktree or submodule checkout
                } else if is_header(&name) {
                    has_header = true;
                }
            }
        }
        if nested_repo && dir != root {
            continue;
        }
        if dir.file_name().is_some_and(|n| n == "include") {
            includes.push(dir.clone());
        } else if has_header {
            headers.push(dir.clone());
        }
        if depth < MAX_DEPTH {
            subdirs.sort();
            queue.extend(subdirs.into_iter().map(|d| (d, depth + 1)));
        }
    }
    (includes, headers)
}

fn cmake_standard(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("CMakeLists.txt")).ok()?;
    for pat in ["CMAKE_CXX_STANDARD", "cxx_std_"] {
        if let Some(i) = text.find(pat) {
            let digits: String = text[i + pat.len()..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if digits.len() == 2 {
                return Some(format!("-std=c++{digits}"));
            }
        }
    }
    None
}

fn push_dir(args: &mut Vec<String>, seen: &mut HashSet<String>, flag: &str, dir: &Path) {
    let d = dir.to_string_lossy().into_owned();
    if seen.insert(format!("{flag}{d}")) {
        args.push(flag.to_string());
        args.push(d);
    }
}

/// Works out the flags for a project directory. Reads the disk: call it off the UI thread.
pub fn compute(project: &Path) -> Flags {
    let vs = std::fs::read_to_string(project.join(".vscode/c_cpp_properties.json"))
        .ok()
        .and_then(|t| parse_vscode(&t, project));
    let mut flags = Flags::default();
    let mut seen = HashSet::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    match &vs {
        Some(vs) => {
            flags = vs.flags.clone();
            for (dir, recursive) in &vs.includes {
                if dir.starts_with(project) {
                    push_dir(&mut flags.args, &mut seen, "-I", dir);
                    if *recursive {
                        roots.push(dir.clone());
                    }
                } else {
                    // Outside the project (e.g. Qt): a plain system include dir.
                    push_dir(&mut flags.args, &mut seen, "-isystem", dir);
                }
            }
        }
        None => {
            push_dir(&mut flags.args, &mut seen, "-I", project);
            roots.push(project.to_path_buf());
            flags.std_cpp = cmake_standard(project);
        }
    }
    let mut late = Vec::new();
    for root in roots {
        let (includes, headers) = scan(&root, MAX_DIRS);
        for d in includes {
            push_dir(&mut flags.args, &mut seen, "-I", &d);
        }
        late.extend(headers);
    }
    // Searched after the system directories, so `<vector>` doesn't probe hundreds of
    // project folders, but still found like VSCode's `/**` would.
    for d in late {
        push_dir(&mut flags.args, &mut seen, "-idirafter", &d);
    }
    flags
}

/// The newest `build*` directory of the project that holds a `build.ninja`.
fn ninja_build_dir(project: &Path) -> Option<PathBuf> {
    std::fs::read_dir(project)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            (n == "build" || n.starts_with("build-") || n.starts_with("cmake-build"))
                && e.path().join("build.ninja").is_file()
        })
        .max_by_key(|e| {
            e.path()
                .join("build.ninja")
                .metadata()
                .and_then(|m| m.modified())
                .ok()
        })
        .map(|e| e.path())
}

fn cache_dir(project: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let name: String = project
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    base.join("rsedit/compdb").join(name)
}

/// Asks `ninja -t compdb` (no build happens, ~50 ms) for a compilation database and
/// stores it in rsedit's cache directory, not in the project. clangd then infers
/// headers' flags from neighbouring sources, exactly as for a real database.
pub fn ninja_compdb(project: &Path) -> Option<PathBuf> {
    let build = ninja_build_dir(project)?;
    let out = std::process::Command::new("ninja")
        .args(["-t", "compdb"])
        .current_dir(&build)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.len() < 3 {
        return None;
    }
    let dir = cache_dir(project);
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join("compile_commands.json"), &out.stdout).ok()?;
    Some(dir)
}

static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<Flags>>>> = OnceLock::new();

/// Cached [`compute`]: several servers in one repository share a scan.
pub fn compute_cached(project: &Path) -> Arc<Flags> {
    let cache = CACHE.get_or_init(Default::default);
    if let Some(f) = cache.lock().expect("flags cache").get(project) {
        return f.clone();
    }
    let flags = Arc::new(compute(project));
    cache
        .lock()
        .expect("flags cache")
        .insert(project.to_path_buf(), flags.clone());
    flags
}

/// Forgets cached flags (a `c_cpp_properties.json` or `CMakeLists.txt` changed).
pub fn invalidate() {
    if let Some(cache) = CACHE.get() {
        cache.lock().expect("flags cache").clear();
    }
}

/// The compile command for one file.
pub fn command(flags: &Flags, lang: &str, file: &Path) -> Vec<String> {
    let (driver, std, x) = if lang == "c" {
        ("clang", &flags.std_c, "c")
    } else {
        ("clang++", &flags.std_cpp, "c++")
    };
    let mut cmd = vec![driver.to_string()];
    cmd.extend(std.clone());
    cmd.extend(flags.args.iter().cloned());
    cmd.extend(["-x".into(), x.into()]);
    cmd.push(file.to_string_lossy().into_owned());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rsedit-cppflags-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    #[test]
    fn parses_jsonc_vscode_config() {
        let text = r#"{
            // comment
            "configurations": [{
                "name": "Linux", /* block */
                "includePath": ["${workspaceFolder}/**", "/usr/include/qt6/**", "${workspaceFolder}/src/shared", "${bogus}/x",],
                "defines": ["FOO=1"],
                "cppStandard": "c++23",
                "cStandard": "c17",
            }],
            "version": 4,
        }"#;
        let ws = Path::new("/p");
        let vs = parse_vscode(text, ws).unwrap();
        assert_eq!(
            vs.includes,
            vec![
                (PathBuf::from("/p"), true),
                (PathBuf::from("/usr/include/qt6"), true),
                (PathBuf::from("/p/src/shared"), false),
            ]
        );
        assert_eq!(vs.flags.std_cpp.as_deref(), Some("-std=c++23"));
        assert_eq!(vs.flags.std_c.as_deref(), Some("-std=c17"));
        assert_eq!(vs.flags.args, vec!["-DFOO=1"]);
    }

    #[test]
    fn scan_finds_hidden_include_dirs_and_skips_noise() {
        let d = tmp("scan");
        touch(&d.join("vendor/.prefix/include/msgpack.hpp"));
        touch(&d.join("a/include/a.hpp"));
        touch(&d.join("build/include/b.hpp"));
        touch(&d.join("node_modules/x/include/x.h"));
        touch(&d.join("wt/.git"));
        touch(&d.join("wt/include/w.hpp"));
        touch(&d.join("src/shared/s.h"));
        let (inc, hdr) = scan(&d, 100);
        assert!(inc.contains(&d.join("vendor/.prefix/include")));
        assert!(inc.contains(&d.join("a/include")));
        assert_eq!(inc.len(), 2, "{inc:?}");
        assert_eq!(hdr, vec![d.join("src/shared")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_respects_the_directory_cap() {
        let d = tmp("cap");
        for i in 0..10 {
            touch(&d.join(format!("d{i}/include/x.h")));
        }
        let (inc, _) = scan(&d, 3);
        assert!(inc.len() < 10);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn compile_db_and_project_dir_detection() {
        let d = tmp("proj");
        touch(&d.join("build/compile_commands.json"));
        touch(&d.join("src/x.cpp"));
        assert!(has_compile_db(&d.join("src/x.cpp")));
        let e = tmp("proj2");
        touch(&e.join(".vscode/c_cpp_properties.json"));
        touch(&e.join("sub/CMakeLists.txt"));
        touch(&e.join("sub/x.cpp"));
        assert!(!has_compile_db(&e.join("sub/x.cpp")));
        assert_eq!(project_dir(&e.join("sub/x.cpp"), None), e);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn compute_without_vscode_uses_heuristic_and_cmake_standard() {
        let d = tmp("heur");
        std::fs::write(d.join("CMakeLists.txt"), "set(CMAKE_CXX_STANDARD 20)").unwrap();
        touch(&d.join("vendor/.prefix/include/msgpack.hpp"));
        touch(&d.join("src/shared/s.h"));
        let f = compute(&d);
        assert_eq!(f.std_cpp.as_deref(), Some("-std=c++20"));
        let joined = f.args.join(" ");
        assert!(joined.contains(&format!(
            "-I {}",
            d.join("vendor/.prefix/include").display()
        )));
        assert!(joined.contains(&format!("-idirafter {}", d.join("src/shared").display())));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn command_picks_driver_and_standard() {
        let f = Flags {
            std_c: Some("-std=c17".into()),
            std_cpp: Some("-std=c++23".into()),
            args: vec!["-I".into(), "/i".into()],
        };
        let c = command(&f, "c", Path::new("/a.c"));
        assert_eq!(c.first().unwrap(), "clang");
        assert!(c.contains(&"-std=c17".to_string()));
        let cpp = command(&f, "cpp", Path::new("/a.hpp"));
        assert_eq!(cpp.first().unwrap(), "clang++");
        assert_eq!(cpp.last().unwrap(), "/a.hpp");
    }
}
