//! Imports resolved to files, and what the file graph answers: who imports a
//! file (its blast radius, transitively), and the cycles. An import that names
//! no file of the repository - a package, the standard library - is external
//! and makes no edge.
use super::lang::Lang;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub struct Resolver<'a> {
    files: &'a BTreeMap<String, Lang>,
    /// Files by their last path part, for suffix matches.
    by_name: BTreeMap<String, Vec<&'a str>>,
    /// Files by their folder.
    by_dir: BTreeMap<String, Vec<&'a str>>,
    /// C#: files by the namespaces they declare.
    namespaces: &'a BTreeMap<String, Vec<String>>,
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() { rest.to_owned() } else { format!("{dir}/{rest}") }
}

/// `a/./b/../c` -> `a/c`; None when it climbs above the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

impl<'a> Resolver<'a> {
    pub fn new(files: &'a BTreeMap<String, Lang>, namespaces: &'a BTreeMap<String, Vec<String>>) -> Self {
        let mut by_name: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        let mut by_dir: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for path in files.keys() {
            by_name.entry(file_name(path).to_owned()).or_default().push(path);
            by_dir.entry(parent(path).to_owned()).or_default().push(path);
        }
        Self { files, by_name, by_dir, namespaces }
    }

    fn has(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Files whose path is `tail` or ends in `/tail`.
    fn ending(&self, tail: &str) -> Vec<String> {
        let name = file_name(tail);
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|path| **path == tail || path.ends_with(&format!("/{tail}")))
            .map(|path| (*path).to_owned())
            .collect()
    }

    fn first_existing(&self, candidates: impl IntoIterator<Item = String>) -> Vec<String> {
        candidates.into_iter().find(|candidate| self.has(candidate)).into_iter().collect()
    }

    /// The files `spec`, imported by `src`, names.
    pub fn resolve(&self, src: &str, lang: Lang, spec: &str) -> Vec<String> {
        let dir = parent(src);
        let mut found = match lang {
            Lang::Rust => self.rust(src, spec),
            Lang::Python => self.python(dir, spec),
            Lang::Js | Lang::Ts => self.relative(dir, spec, lang, &["index"]),
            Lang::Ruby | Lang::Php => self.relative(dir, spec, lang, &[]),
            Lang::C | Lang::Cpp | Lang::Shader => {
                let local = normalize(&join(dir, spec)).filter(|path| self.has(path));
                match local {
                    Some(path) => vec![path],
                    None => {
                        let matches = self.ending(spec.trim_start_matches("./"));
                        if matches.len() <= 3 { matches } else { Vec::new() }
                    }
                }
            }
            Lang::Go => self.go(spec),
            Lang::Java | Lang::Kotlin => {
                let path = spec.trim_end_matches('.').replace('.', "/");
                let mut out = Vec::new();
                for ext in ["java", "kt"] {
                    out.extend(self.ending(&format!("{path}.{ext}")));
                }
                if out.is_empty() {
                    if let Some((owner, _)) = path.rsplit_once('/') {
                        for ext in ["java", "kt"] {
                            out.extend(self.ending(&format!("{owner}.{ext}")));
                        }
                    }
                }
                if out.is_empty() && spec.ends_with('.') {
                    out = self.by_dir.iter().filter(|(d, _)| d.ends_with(&path) && !path.is_empty()).flat_map(|(_, files)| files.iter().map(|f| (*f).to_owned())).collect();
                }
                out
            }
            Lang::CSharp => self.namespaces.get(spec).map(|files| files.iter().take(30).cloned().collect()).unwrap_or_default(),
            Lang::Lua => {
                let path = spec.replace('.', "/");
                let mut out = self.ending(&format!("{path}.lua"));
                if out.is_empty() {
                    out = self.ending(&format!("{path}/init.lua"));
                }
                out
            }
            _ => Vec::new(),
        };
        found.retain(|path| path != src);
        found.sort();
        found.dedup();
        found
    }

    fn relative(&self, dir: &str, spec: &str, lang: Lang, index_names: &[&str]) -> Vec<String> {
        if !spec.starts_with('.') {
            return Vec::new();
        }
        let Some(base) = normalize(&join(dir, spec)) else { return Vec::new() };
        let mut candidates = vec![base.clone()];
        for ext in lang.resolve_extensions() {
            candidates.push(format!("{base}.{ext}"));
        }
        for name in index_names {
            for ext in lang.resolve_extensions() {
                candidates.push(format!("{base}/{name}.{ext}"));
            }
        }
        self.first_existing(candidates)
    }

    /// The folder a Rust file's `mod x;` children live in.
    fn module_dir(src: &str) -> String {
        let name = file_name(src);
        let dir = parent(src);
        if matches!(name, "lib.rs" | "main.rs" | "mod.rs") {
            dir.to_owned()
        } else {
            join(dir, name.trim_end_matches(".rs"))
        }
    }

    fn crate_root(&self, src: &str) -> Option<String> {
        let mut dir = parent(src).to_owned();
        loop {
            if self.has(&join(&dir, "lib.rs")) || self.has(&join(&dir, "main.rs")) {
                return Some(dir);
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent(&dir).to_owned();
        }
    }

    fn rust(&self, src: &str, spec: &str) -> Vec<String> {
        if let Some(name) = spec.strip_prefix("mod:") {
            let dir = Self::module_dir(src);
            return self.first_existing([join(&dir, &format!("{name}.rs")), join(&dir, &format!("{name}/mod.rs"))]);
        }
        let mut segments: Vec<&str> = spec.split("::").collect();
        let base = match segments.first().copied() {
            Some("crate") => self.crate_root(src),
            Some("self") => Some(Self::module_dir(src)),
            Some("super") => {
                let mut dir = parent(&Self::module_dir(src)).to_owned();
                while segments.get(1) == Some(&"super") {
                    segments.remove(1);
                    dir = parent(&dir).to_owned();
                }
                Some(dir)
            }
            _ => None,
        };
        let Some(base) = base else { return Vec::new() };
        let rest = &segments[1..];
        for take in (1..=rest.len()).rev() {
            let path = join(&base, &rest[..take].join("/"));
            let found = self.first_existing([format!("{path}.rs"), format!("{path}/mod.rs")]);
            if !found.is_empty() {
                return found;
            }
        }
        // `crate::LoreError`, `super::Hit`: an item of the module itself - the
        // crate root's file, or the module's own file.
        if segments.first() == Some(&"crate") {
            return self.first_existing([join(&base, "lib.rs"), join(&base, "main.rs")]);
        }
        let own = if base.is_empty() { Vec::new() } else { self.first_existing([format!("{base}.rs"), format!("{base}/mod.rs")]) };
        if !own.is_empty() {
            return own;
        }
        self.first_existing([join(&base, "lib.rs"), join(&base, "main.rs")])
    }

    fn python(&self, dir: &str, spec: &str) -> Vec<String> {
        let dots = spec.chars().take_while(|c| *c == '.').count();
        let rest = &spec[dots..];
        let segments: Vec<&str> = rest.split('.').filter(|s| !s.is_empty()).collect();
        if dots > 0 {
            let mut base = dir.to_owned();
            for _ in 1..dots {
                base = parent(&base).to_owned();
            }
            if segments.is_empty() {
                return self.first_existing([join(&base, "__init__.py")]);
            }
            for take in (1..=segments.len()).rev() {
                let path = join(&base, &segments[..take].join("/"));
                let found = self.first_existing([format!("{path}.py"), format!("{path}/__init__.py")]);
                if !found.is_empty() {
                    return found;
                }
            }
            return Vec::new();
        }
        for take in (1..=segments.len()).rev() {
            let path = segments[..take].join("/");
            let mut found = self.ending(&format!("{path}.py"));
            found.extend(self.ending(&format!("{path}/__init__.py")));
            if !found.is_empty() {
                return if found.len() <= 3 { found } else { Vec::new() };
            }
        }
        Vec::new()
    }

    fn go(&self, spec: &str) -> Vec<String> {
        let segments: Vec<&str> = spec.split('/').collect();
        for start in 0..segments.len() {
            let tail = segments[start..].join("/");
            let dirs: Vec<&String> = self.by_dir.keys().filter(|dir| **dir == tail || dir.ends_with(&format!("/{tail}"))).collect();
            if !dirs.is_empty() {
                return dirs
                    .into_iter()
                    .flat_map(|dir| self.by_dir[dir].iter().filter(|path| path.ends_with(".go") && !path.ends_with("_test.go")).map(|path| (*path).to_owned()))
                    .collect();
            }
        }
        Vec::new()
    }
}

/// The files that import any of `targets`, each with its distance (1 = imports
/// it directly), at most `depth` steps away.
pub fn dependents(edges: &[(String, String)], targets: &[String], depth: usize) -> BTreeMap<String, usize> {
    let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (src, dst) in edges {
        reverse.entry(dst.as_str()).or_default().push(src.as_str());
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let start: BTreeSet<&str> = targets.iter().map(String::as_str).collect();
    let mut queue: VecDeque<(&str, usize)> = start.iter().map(|t| (*t, 0)).collect();
    while let Some((node, distance)) = queue.pop_front() {
        if distance >= depth {
            continue;
        }
        for &src in reverse.get(node).into_iter().flatten() {
            if start.contains(src) || seen.contains_key(src) {
                continue;
            }
            seen.insert(src.to_owned(), distance + 1);
            queue.push_back((src, distance + 1));
        }
    }
    seen
}

/// The import cycles: strongly connected groups of two files or more, the
/// largest first (Tarjan's algorithm, iterative).
pub fn cycles(edges: &[(String, String)]) -> Vec<Vec<String>> {
    let mut nodes: BTreeSet<&str> = BTreeSet::new();
    let mut next: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (src, dst) in edges {
        nodes.insert(src);
        nodes.insert(dst);
        next.entry(src.as_str()).or_default().push(dst.as_str());
    }
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let mut low: BTreeMap<&str, usize> = BTreeMap::new();
    let mut on_stack: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut counter = 0;
    let mut out: Vec<Vec<String>> = Vec::new();
    for &root in &nodes {
        if index.contains_key(root) {
            continue;
        }
        let mut work: Vec<(&str, usize)> = vec![(root, 0)];
        while let Some((node, child)) = work.pop() {
            if child == 0 {
                index.insert(node, counter);
                low.insert(node, counter);
                counter += 1;
                stack.push(node);
                on_stack.insert(node);
            }
            let children = next.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if child < children.len() {
                work.push((node, child + 1));
                let to = children[child];
                if !index.contains_key(to) {
                    work.push((to, 0));
                } else if on_stack.contains(to) {
                    let value = low[node].min(index[to]);
                    low.insert(node, value);
                }
                continue;
            }
            if let Some(&(caller, _)) = work.last() {
                let value = low[caller].min(low[node]);
                low.insert(caller, value);
            }
            if low[node] == index[node] {
                let mut group = Vec::new();
                while let Some(top) = stack.pop() {
                    on_stack.remove(top);
                    group.push(top.to_owned());
                    if top == node {
                        break;
                    }
                }
                if group.len() > 1 {
                    group.sort();
                    out.push(group);
                }
            }
        }
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[(&str, Lang)]) -> BTreeMap<String, Lang> {
        paths.iter().map(|(p, l)| ((*p).to_owned(), *l)).collect()
    }

    #[test]
    fn rust_modules_and_crate_paths_resolve() {
        let files = files(&[("src/lib.rs", Lang::Rust), ("src/atlas/mod.rs", Lang::Rust), ("src/atlas/graph.rs", Lang::Rust), ("src/index.rs", Lang::Rust)]);
        let none = BTreeMap::new();
        let r = Resolver::new(&files, &none);
        assert_eq!(r.resolve("src/lib.rs", Lang::Rust, "mod:atlas"), ["src/atlas/mod.rs"]);
        assert_eq!(r.resolve("src/atlas/mod.rs", Lang::Rust, "mod:graph"), ["src/atlas/graph.rs"]);
        assert_eq!(r.resolve("src/atlas/graph.rs", Lang::Rust, "crate::index::terms_of"), ["src/index.rs"]);
        assert_eq!(r.resolve("src/atlas/graph.rs", Lang::Rust, "super::lang::Lang"), ["src/atlas/mod.rs"]);
        assert_eq!(r.resolve("src/atlas/graph.rs", Lang::Rust, "crate::LoreError"), ["src/lib.rs"]);
    }

    #[test]
    fn relative_js_and_c_includes_resolve() {
        let files = files(&[("web/a.ts", Lang::Ts), ("web/lib/index.ts", Lang::Ts), ("eng/r.cpp", Lang::Cpp), ("eng/inc/r.h", Lang::Cpp)]);
        let none = BTreeMap::new();
        let r = Resolver::new(&files, &none);
        assert_eq!(r.resolve("web/a.ts", Lang::Ts, "./lib"), ["web/lib/index.ts"]);
        assert_eq!(r.resolve("web/a.ts", Lang::Ts, "react"), Vec::<String>::new());
        assert_eq!(r.resolve("eng/r.cpp", Lang::Cpp, "r.h"), ["eng/inc/r.h"]);
    }

    #[test]
    fn dependents_and_cycles() {
        let e = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        let edges = vec![e("a", "b"), e("b", "c"), e("c", "a"), e("d", "c"), e("x", "d")];
        let found = dependents(&edges, &["c".to_owned()], 2);
        assert_eq!(found.into_iter().collect::<Vec<_>>(), [("a".to_owned(), 2), ("b".to_owned(), 1), ("d".to_owned(), 1), ("x".to_owned(), 2)]);
        assert_eq!(cycles(&edges), [vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]]);
    }
}
