//! What the code map reads in a file: its language by extension, the symbols it
//! defines and the imports it names. Patterns, not parsers: a definition line is
//! found by its shape, an import by its keyword. That is what lets one binary
//! read twenty languages with no grammar to ship; what it misses is a symbol
//! written in an unusual shape, never a whole file - the file is still chunked
//! and searchable as text.
use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    Js,
    Ts,
    Go,
    Java,
    Kotlin,
    CSharp,
    C,
    Cpp,
    Shader,
    Lua,
    Ruby,
    Php,
    Swift,
    Shell,
    PowerShell,
    /// Indexed as text: no symbols, no imports.
    Text,
}

impl Lang {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Js => "js",
            Self::Ts => "ts",
            Self::Go => "go",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::CSharp => "csharp",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Shader => "shader",
            Self::Lua => "lua",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Swift => "swift",
            Self::Shell => "shell",
            Self::PowerShell => "powershell",
            Self::Text => "text",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        ALL.iter().copied().find(|lang| lang.name() == name)
    }

    /// The extensions an import without one may mean, in the order tried.
    pub fn resolve_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Python => &["py", "pyi"],
            Self::Js | Self::Ts => &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"],
            Self::Lua => &["lua"],
            Self::Ruby => &["rb"],
            Self::Php => &["php"],
            _ => &[],
        }
    }
}

const ALL: [Lang; 18] = [
    Lang::Rust, Lang::Python, Lang::Js, Lang::Ts, Lang::Go, Lang::Java, Lang::Kotlin, Lang::CSharp, Lang::C, Lang::Cpp,
    Lang::Shader, Lang::Lua, Lang::Ruby, Lang::Php, Lang::Swift, Lang::Shell, Lang::PowerShell, Lang::Text,
];

/// The language of a path by its extension (or its name, for build files);
/// None leaves the file out unless `atlas.include` names its extension.
pub fn detect(path: &str) -> Option<Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "CMakeLists.txt" | "Makefile" | "Dockerfile" | "Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" => return Some(Lang::Text),
        _ => {}
    }
    let ext = name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())?;
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "js" | "jsx" | "mjs" | "cjs" => Lang::Js,
        "ts" | "tsx" | "mts" | "cts" => Lang::Ts,
        "go" => Lang::Go,
        "java" => Lang::Java,
        "kt" | "kts" => Lang::Kotlin,
        "cs" => Lang::CSharp,
        "c" => Lang::C,
        "h" | "hh" | "hpp" | "hxx" | "cc" | "cpp" | "cxx" | "inl" | "ipp" | "m" | "mm" => Lang::Cpp,
        "hlsl" | "hlsli" | "fx" | "fxh" | "glsl" | "vert" | "frag" | "comp" | "geom" | "tesc" | "tese" | "usf" | "ush" | "shader" | "cginc" | "wgsl" | "metal" => Lang::Shader,
        "lua" => Lang::Lua,
        "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "swift" => Lang::Swift,
        "sh" | "bash" | "zsh" => Lang::Shell,
        "ps1" | "psm1" => Lang::PowerShell,
        _ => return None,
    })
}

/// One definition: its name, what it is, its line (1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    /// The definition's indentation: chunk boundaries prefer the outermost.
    pub indent: usize,
}

/// One import as written, and its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub spec: String,
    pub line: usize,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("an atlas pattern is valid")
}

struct Patterns {
    /// (pattern, kind-group or fixed kind, name group)
    defs: Vec<(Regex, Kind, usize)>,
}

enum Kind {
    Group(usize),
    Fixed(&'static str),
}

const CONTROL: [&str; 14] = ["if", "for", "while", "switch", "catch", "return", "else", "do", "sizeof", "new", "delete", "throw", "case", "using"];

fn patterns(lang: Lang) -> &'static Patterns {
    macro_rules! cached {
        ($build:expr) => {{
            static CELL: OnceLock<Patterns> = OnceLock::new();
            CELL.get_or_init(|| Patterns { defs: $build })
        }};
    }
    match lang {
        Lang::Rust => cached!(vec![
            (re(r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async|const|unsafe|extern(?:\s+"[^"]*")?)\s+)*(fn|struct|enum|trait|type|union|mod|static|const)\s+([A-Za-z_][A-Za-z0-9_]*)"#), Kind::Group(1), 2),
            (re(r"^\s*macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)"), Kind::Fixed("macro"), 1),
            (re(r"^\s*(?:unsafe\s+)?impl(?:\s*<[^{]*?>)?\s+(?:[^{;]*?\s+for\s+)?&?([A-Za-z_][A-Za-z0-9_]*)"), Kind::Fixed("impl"), 1),
        ]),
        Lang::Python => cached!(vec![(re(r"^\s*(?:async\s+)?(def|class)\s+([A-Za-z_]\w*)"), Kind::Group(1), 2)]),
        Lang::Js | Lang::Ts => cached!(vec![
            (re(r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(function\*?|class|interface|enum|namespace)\s+([A-Za-z_$][\w$]*)"), Kind::Group(1), 2),
            (re(r"^\s*(?:export\s+)?(?:declare\s+)?type\s+([A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*="), Kind::Fixed("type"), 1),
            (re(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[A-Za-z_$][\w$]*\s*=>)"), Kind::Fixed("function"), 1),
            (re(r"^\s+(?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*(?::\s*[^{]+)?\{\s*$"), Kind::Fixed("method"), 1),
        ]),
        Lang::Go => cached!(vec![
            (re(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)"), Kind::Fixed("func"), 1),
            (re(r"^type\s+([A-Za-z_]\w*)"), Kind::Fixed("type"), 1),
        ]),
        Lang::Java | Lang::Kotlin | Lang::CSharp | Lang::Swift => cached!(vec![
            (re(r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|sealed|partial|open|data|inner|readonly|unsafe|fileprivate)\s+)*(class|interface|enum|record|struct|object|namespace|protocol|extension|actor)\s+([A-Za-z_][\w.]*)"), Kind::Group(1), 2),
            (re(r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|override|virtual|async|sealed|extern|unsafe|new|synchronized|suspend|open|inline|operator|mutating|fileprivate)\s+)*(?:fun|func)\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?([A-Za-z_]\w*)"), Kind::Fixed("function"), 1),
            (re(r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|override|virtual|async|sealed|extern|unsafe|new|synchronized)\s+)+[\w<>\[\],.?\s]*?\b([A-Za-z_]\w*)\s*\([^;]*$"), Kind::Fixed("method"), 1),
        ]),
        Lang::C | Lang::Cpp | Lang::Shader => cached!(vec![
            (re(r"^\s*(?:template\s*<[^>]*>\s*)?(?:typedef\s+)?(class|struct|enum(?:\s+class)?|union|namespace|cbuffer)\s+(?:[A-Z_]+\s+)?([A-Za-z_]\w*)\s*(?:final\s*)?(?::[^;{]*)?(?:\{.*)?$"), Kind::Group(1), 2),
            (re(r"^(?:template\s*<[^>]*>\s*)?(?:[A-Za-z_][\w:<>,]*[\s*&]+)+((?:[A-Za-z_]\w*::)*~?[A-Za-z_]\w*)\s*\([^;]*$"), Kind::Fixed("function"), 1),
            (re(r"^\s*#\s*define\s+([A-Za-z_]\w*)\("), Kind::Fixed("macro"), 1),
        ]),
        Lang::Lua => cached!(vec![(re(r"^\s*(?:local\s+)?function\s+([\w.:]+)"), Kind::Fixed("function"), 1)]),
        Lang::Ruby => cached!(vec![(re(r"^\s*(def|class|module)\s+(?:self\.)?([\w:?!]+)"), Kind::Group(1), 2)]),
        Lang::Php => cached!(vec![(re(r"^\s*(?:(?:abstract|final|public|private|protected|static)\s+)*(class|interface|trait|enum|function)\s+&?([A-Za-z_]\w*)"), Kind::Group(1), 2)]),
        Lang::Shell => cached!(vec![(re(r"^\s*(?:function\s+)?([A-Za-z_][\w-]*)\s*\(\)\s*\{?"), Kind::Fixed("function"), 1)]),
        Lang::PowerShell => cached!(vec![(re(r"(?i)^\s*(function|filter|class)\s+([\w-]+)"), Kind::Group(1), 2)]),
        Lang::Text => cached!(Vec::new()),
    }
}

/// The definitions of a file's text.
pub fn symbols(lang: Lang, text: &str) -> Vec<Symbol> {
    let patterns = patterns(lang);
    let mut out = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (at, line) in lines.iter().enumerate() {
        if line.len() > 400 {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') && !matches!(lang, Lang::C | Lang::Cpp | Lang::Shader) || trimmed.starts_with('*') {
            continue;
        }
        for (pattern, kind, group) in &patterns.defs {
            let Some(captures) = pattern.captures(line) else { continue };
            let Some(name) = captures.get(*group).map(|m| m.as_str().to_owned()) else { continue };
            if CONTROL.contains(&name.as_str()) || name.len() < 2 && lang != Lang::Go {
                continue;
            }
            let kind = match kind {
                Kind::Group(group) => captures.get(*group).map_or("symbol", |m| m.as_str()).split_whitespace().next().unwrap_or("symbol").to_owned(),
                Kind::Fixed(kind) => (*kind).to_owned(),
            };
            // A C-family "function" is a definition only when its body follows,
            // not a call or a declaration: a `{` before any `;` within three lines.
            if matches!(lang, Lang::C | Lang::Cpp | Lang::Shader | Lang::Java | Lang::CSharp) && (kind == "function" || kind == "method") && !body_follows(&lines, at) {
                continue;
            }
            let indent = line.len() - trimmed.len();
            out.push(Symbol { name, kind, line: at + 1, indent });
            break;
        }
    }
    out
}

fn body_follows(lines: &[&str], at: usize) -> bool {
    for line in lines.iter().skip(at).take(4) {
        let code = line.split("//").next().unwrap_or("");
        let brace = code.find('{');
        let semi = code.find(';');
        match (brace, semi) {
            (Some(b), Some(s)) => return b < s,
            (Some(_), None) => return true,
            (None, Some(_)) => return false,
            (None, None) => {}
        }
    }
    false
}

fn import_patterns(lang: Lang) -> &'static [Regex] {
    macro_rules! cached {
        ($($pattern:expr),* $(,)?) => {{
            static CELL: OnceLock<Vec<Regex>> = OnceLock::new();
            CELL.get_or_init(|| vec![$(re($pattern)),*]).as_slice()
        }};
    }
    match lang {
        Lang::Rust => cached!(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+((?:crate|super|self)::[^;]+);",
        ),
        Lang::Python => cached!(r"^\s*import\s+([\w.]+(?:\s*,\s*[\w.]+)*)", r"^\s*from\s+(\.*[\w.]*)\s+import\s+\(?([\w*, ]+)"),
        Lang::Js | Lang::Ts => cached!(
            r#"(?:^|\s)(?:import|export)\s[^'"]*?from\s*['"]([^'"]+)['"]"#,
            r#"^\s*import\s*['"]([^'"]+)['"]"#,
            r#"require\(\s*['"]([^'"]+)['"]\s*\)"#,
            r#"import\(\s*['"]([^'"]+)['"]\s*\)"#,
        ),
        Lang::C | Lang::Cpp | Lang::Shader => cached!(r#"^\s*#\s*(?:include|import)\s*["<]([^">]+)[">]"#),
        Lang::Go => cached!(r#"^\s*import\s+(?:\w+\s+)?"([^"]+)""#, r#"^\s+(?:\w+\s+|\.\s+|_\s+)?"([^"]+)"\s*$"#),
        Lang::Java | Lang::Kotlin => cached!(r"^\s*import\s+(?:static\s+)?([\w.]+)"),
        Lang::CSharp => cached!(r"^\s*(?:global\s+)?using\s+(?:static\s+)?([A-Za-z_][\w.]*)\s*;"),
        Lang::Lua => cached!(r#"require\s*\(?\s*['"]([^'"]+)['"]"#),
        Lang::Ruby => cached!(r#"^\s*require_relative\s+['"]([^'"]+)['"]"#),
        Lang::Php => cached!(r#"(?:require|include)(?:_once)?\s*\(?\s*(?:__DIR__\s*\.\s*)?['"]([^'"]+)['"]"#),
        _ => &[],
    }
}

/// The imports of a file's text, as written (resolved to files later, once
/// every file is known).
pub fn imports(lang: Lang, text: &str) -> Vec<Import> {
    let patterns = import_patterns(lang);
    if patterns.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut in_go_block = false;
    for (at, line) in text.lines().enumerate() {
        if line.len() > 600 {
            continue;
        }
        let line_no = at + 1;
        if lang == Lang::Go {
            let trimmed = line.trim();
            if trimmed.starts_with("import (") {
                in_go_block = true;
                continue;
            }
            if in_go_block && trimmed.starts_with(')') {
                in_go_block = false;
                continue;
            }
            let pattern = if in_go_block { &patterns[1] } else { &patterns[0] };
            if let Some(found) = pattern.captures(line).and_then(|c| c.get(1)) {
                out.push(Import { spec: found.as_str().to_owned(), line: line_no });
            }
            continue;
        }
        for (n, pattern) in patterns.iter().enumerate() {
            for captures in pattern.captures_iter(line) {
                let Some(first) = captures.get(1) else { continue };
                match lang {
                    Lang::Python if n == 0 => {
                        for name in first.as_str().split(',') {
                            out.push(Import { spec: name.trim().to_owned(), line: line_no });
                        }
                    }
                    Lang::Python => {
                        // `from a.b import c, d`: a.b.c and a.b.d may be modules; a.b is.
                        let base = first.as_str();
                        out.push(Import { spec: base.to_owned(), line: line_no });
                        for name in captures.get(2).map_or("", |m| m.as_str()).split(',') {
                            let name = name.trim();
                            if !name.is_empty() && name != "*" {
                                let joined = if base.ends_with('.') { format!("{base}{name}") } else { format!("{base}.{name}") };
                                out.push(Import { spec: joined, line: line_no });
                            }
                        }
                    }
                    Lang::Rust if n == 1 => {
                        for spec in rust_use_paths(first.as_str()) {
                            out.push(Import { spec, line: line_no });
                        }
                    }
                    Lang::Rust => out.push(Import { spec: format!("mod:{}", first.as_str()), line: line_no }),
                    _ => out.push(Import { spec: first.as_str().to_owned(), line: line_no }),
                }
            }
        }
    }
    out
}

/// `crate::a::{b, c::d}` -> `crate::a::b`, `crate::a::c::d`. One level of
/// braces is read; deeper ones keep their prefix.
fn rust_use_paths(text: &str) -> Vec<String> {
    let text: String = text.split_whitespace().collect();
    let Some(open) = text.find('{') else { return vec![text.trim_end_matches("::*").to_owned()] };
    let prefix = text[..open].trim_end_matches("::");
    let inner = text[open + 1..].trim_end_matches('}');
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let bytes: Vec<char> = inner.chars().collect();
    let mut parts = Vec::new();
    for (i, ch) in bytes.iter().enumerate() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(bytes[start..i].iter().collect::<String>());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(bytes[start..].iter().collect::<String>());
    for part in parts {
        let part = part.split('{').next().unwrap_or("").trim_end_matches("::");
        if part.is_empty() {
            continue;
        }
        if part == "self" {
            out.push(prefix.to_owned());
        } else {
            out.push(format!("{prefix}::{part}"));
        }
    }
    out
}

/// `getUserName` / `get_user_name` / `HTTPServer` -> `get user name`, `http server`:
/// the words a person types for an identifier.
pub fn split_identifier(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in name.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = part.chars().collect();
        let mut current = String::new();
        for (i, &ch) in chars.iter().enumerate() {
            let boundary = i > 0
                && ch.is_uppercase()
                && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit() || chars.get(i + 1).is_some_and(|next| next.is_lowercase()) && chars[i - 1].is_uppercase());
            if boundary && !current.is_empty() {
                words.push(std::mem::take(&mut current).to_lowercase());
            }
            current.push(ch);
        }
        if !current.is_empty() {
            words.push(current.to_lowercase());
        }
    }
    words.retain(|word| word.chars().count() >= 2);
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_definitions_and_imports() {
        let text = "pub mod lang;\nuse crate::index::{terms_of, word_re};\npub(crate) async fn open_index(x: u8) {}\nimpl<'a> Ctx<'a> {\nstruct Hit;\n";
        let names: Vec<String> = symbols(Lang::Rust, text).into_iter().map(|s| format!("{}:{}", s.kind, s.name)).collect();
        assert_eq!(names, ["mod:lang", "fn:open_index", "impl:Ctx", "struct:Hit"]);
        let specs: Vec<String> = imports(Lang::Rust, text).into_iter().map(|i| i.spec).collect();
        assert_eq!(specs, ["mod:lang", "crate::index::terms_of", "crate::index::word_re"]);
    }

    #[test]
    fn c_functions_need_a_body() {
        let text = "int add(int a, int b);\nint add(int a, int b)\n{\n  return a + b;\n}\nstatic void Renderer::draw() {\n}\n";
        let names: Vec<String> = symbols(Lang::Cpp, text).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["add", "Renderer::draw"]);
    }

    #[test]
    fn identifiers_split_into_words() {
        assert_eq!(split_identifier("getUserName"), ["get", "user", "name"]);
        assert_eq!(split_identifier("HTTPServer_v2"), ["http", "server", "v2"]);
    }

    #[test]
    fn python_and_js_imports() {
        let py: Vec<String> = imports(Lang::Python, "import os, a.b\nfrom .x import y, z\n").into_iter().map(|i| i.spec).collect();
        assert_eq!(py, ["os", "a.b", ".x", ".x.y", ".x.z"]);
        let js: Vec<String> = imports(Lang::Ts, "import { a } from './a';\nconst b = require(\"../b\");\n").into_iter().map(|i| i.spec).collect();
        assert_eq!(js, ["./a", "../b"]);
    }
}
