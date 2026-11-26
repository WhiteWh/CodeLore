//! Read one selected compilation context. The compiler command is never executed.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{digest, error, path_text};
use crate::LoreError;

pub(super) struct Command {
    pub file: PathBuf,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub responses: BTreeMap<PathBuf, String>,
}

pub(super) fn read(path: &Path, roots: &[&Path], prefix: Option<&str>) -> Result<Vec<Command>, LoreError> {
    let path = fs::canonicalize(path).map_err(|e| error(format!("{}: {e}", path.display())))?;
    let base = path.parent().expect("compilation database parent");
    let text = fs::read_to_string(&path).map_err(error)?;
    let entries: Vec<Value> = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(error)?;
    let mut commands = BTreeMap::<PathBuf, Command>::new();
    for entry in entries {
        let field = |name| entry[name].as_str().ok_or_else(|| error(format!("missing compdb field {name}")));
        let cwd = base.join(field("directory")?);
        let path = cwd.join(field("file")?);
        let file = match fs::canonicalize(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(error(format!("{}: {e}", path.display()))),
        };
        if !roots.iter().any(|root| file.strip_prefix(root).ok().is_some_and(|relative| wanted(relative, prefix))) {
            continue;
        }
        if !matches!(file.extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx")) {
            continue;
        }
        let cwd = fs::canonicalize(&cwd).map_err(error)?;
        let raw = if let Some(arguments) = entry["arguments"].as_array() {
            arguments.iter().map(|v| v.as_str().map(str::to_owned).ok_or_else(|| error("compdb arguments must be strings"))).collect::<Result<Vec<_>, _>>()?
        } else {
            split_command(field("command")?)?
        };
        let (compiler, raw) = raw.split_first().ok_or_else(|| error("empty compdb command"))?;
        let compiler_name = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler).to_ascii_lowercase();
        let cl = matches!(compiler_name.as_str(), "cl" | "cl.exe" | "clang-cl" | "clang-cl.exe")
            || raw.iter().any(|arg| arg == "--driver-mode=cl");
        let mut responses = BTreeMap::new();
        let args = expand(raw, &cwd, &mut responses, 0)?;
        let argv = analysis_arguments(&args, &file, &cwd, cl)?;
        if let Some(previous) = commands.get(&file) {
            if previous.argv != argv || previous.cwd != cwd || previous.responses != responses {
                return Err(error(format!("{} has multiple compile commands; select one target in the compdb", file.display())));
            }
        } else {
            commands.insert(file.clone(), Command { file, cwd, argv, responses });
        }
    }
    Ok(commands.into_values().collect())
}

pub(super) fn wanted(relative: &Path, prefix: Option<&str>) -> bool {
    prefix.is_none_or(|prefix| relative.to_string_lossy().replace('\\', "/").starts_with(prefix))
}

fn expand(args: &[String], cwd: &Path, files: &mut BTreeMap<PathBuf, String>, level: u8) -> Result<Vec<String>, LoreError> {
    if level > 8 {
        return Err(error("response file nesting exceeds eight levels"));
    }
    let mut out = Vec::new();
    for arg in args {
        if let Some(response) = arg.strip_prefix('@') {
            let path = fs::canonicalize(cwd.join(response)).map_err(|e| error(format!("response file {response}: {e}")))?;
            let bytes = fs::read(&path).map_err(error)?;
            let text = if let Some(raw) = bytes.strip_prefix(&[0xff, 0xfe]) {
                if raw.len() % 2 != 0 {
                    return Err(error("response file has incomplete UTF-16 code unit"));
                }
                let utf16: Vec<_> = raw.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
                String::from_utf16(&utf16).map_err(error)?
            } else {
                std::str::from_utf8(&bytes).map_err(error)?.trim_start_matches('\u{feff}').to_owned()
            };
            files.insert(path, digest(bytes));
            out.extend(expand(&split_command(&text)?, cwd, files, level + 1)?);
        } else {
            out.push(arg.clone());
        }
    }
    Ok(out)
}

#[cfg(windows)]
pub(super) fn split_command(text: &str) -> Result<Vec<String>, LoreError> {
    use std::ffi::c_void;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CommandLineToArgvW(line: *const u16, count: *mut i32) -> *mut *mut u16;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(value: *mut c_void) -> *mut c_void;
    }
    // The dummy executable gives the first real argument the same quoting rules as the rest.
    let line: Vec<_> = format!("lore {}", text.replace(['\r', '\n'], " ")).encode_utf16().chain([0]).collect();
    let mut count = 0;
    let pointers = unsafe { CommandLineToArgvW(line.as_ptr(), &mut count) };
    if pointers.is_null() {
        return Err(error(std::io::Error::last_os_error()));
    }
    let mut result = Vec::new();
    // Win32 allocates the array and all strings in one block.
    unsafe {
        for &ptr in std::slice::from_raw_parts(pointers, count as usize).iter().skip(1) {
            let mut len = 0;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            result.push(String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)));
        }
        LocalFree(pointers.cast());
    }
    Ok(result)
}

#[cfg(not(windows))]
pub(super) fn split_command(text: &str) -> Result<Vec<String>, LoreError> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && matches!(c, '"' | '\'') {
            started = true;
            quote = Some(c);
        } else if c == '\\' && quote != Some('\'') {
            started = true;
            word.push(chars.next().ok_or_else(|| error("incomplete compile-command escape"))?);
        } else if c.is_whitespace() && quote.is_none() {
            if started {
                out.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            started = true;
            word.push(c);
        }
    }
    if quote.is_some() {
        return Err(error("unterminated compile-command quotation"));
    }
    if started {
        out.push(word);
    }
    Ok(out)
}

fn analysis_arguments(args: &[String], file: &Path, cwd: &Path, cl: bool) -> Result<Vec<String>, LoreError> {
    let mut out = vec![if cl { "clang-cl".to_owned() } else { "clang++".to_owned() }, "-working-directory".to_owned(), path_text(cwd)];
    let with_value = ["-I", "-D", "-U", "-isystem", "-iquote", "-idirafter", "-include", "-imacros", "-isysroot", "-resource-dir", "-target", "--target", "-x", "-std", "/I", "/D", "/U", "/FI", "/external:I"];
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if !arg.starts_with('-') && (cwd.join(arg) == file || fs::canonicalize(cwd.join(arg)).ok().as_deref() == Some(file)) {
            continue;
        }
        if matches!(arg.as_str(), "-o" | "-MF" | "-MT" | "-MQ" | "-MJ" | "/Fo" | "/Fd" | "/Fp" | "/sourceDependencies" | "/experimental:log") {
            args.next().ok_or_else(|| error(format!("missing value for {arg}")))?;
            continue;
        }
        if matches!(arg.as_str(), "-c" | "-fsyntax-only" | "-MD" | "-MMD" | "-MP" | "/c" | "/nologo" | "--driver-mode=cl")
            || ["/Fo", "/Fd", "/Fp", "/Yu", "/Yc", "-MF", "-MT", "-MQ", "-MJ"].iter().any(|flag| arg.starts_with(flag))
        {
            continue;
        }
        if with_value.contains(&arg.as_str()) {
            let value = args.next().ok_or_else(|| error(format!("missing value for {arg}")))?;
            out.extend([arg.clone(), value.clone()]);
            continue;
        }
        let safe = ["-I", "-D", "-U", "-std=", "--target=", "--sysroot=", "-resource-dir=", "-stdlib=", "-W", "-O", "-g",
            "/I", "/D", "/U", "/FI", "/external:I", "/external:W", "/std:", "/EH", "/wd", "/we", "/Zc:", "/diagnostics:",
            "/O", "/Gw", "/Gy", "/GF", "/FC", "/GR", "/MD", "/MT", "/TP", "/TC", "/permissive", "/bigobj", "/utf-8", "/Z7",
            "/errorReport:", "/d2ExtendedWarningInfo", "/experimental:deterministic"]
            .iter().any(|prefix| arg.starts_with(prefix)) || matches!(arg.as_str(),
            "-fms-extensions" | "-fms-compatibility" | "-fexceptions" | "-fno-exceptions" | "-fcxx-exceptions" |
            "-fno-rtti" | "-frtti" | "-fPIC" | "-fPIE" | "-pthread" | "-m32" | "-m64" | "-nostdinc" | "-nostdinc++" | "/X");
        if !safe || ["-Wp,", "-Wa,", "-Wl,"].iter().any(|prefix| arg.starts_with(prefix)) {
            return Err(error(format!("unsupported analysis argument {arg:?} for {}; supply an analysis compdb", file.display())));
        }
        out.push(arg.clone());
    }
    Ok(out)
}
