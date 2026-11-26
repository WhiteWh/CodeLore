//! Narrow bindings to the public libclang C ABI. TU handles never escape this module.
use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use super::{digest, error, library::Library};
use crate::LoreError;

type Handle = *mut c_void;
type Visit = extern "C" fn(Cursor, Cursor, Handle) -> c_uint;
type Include = extern "C" fn(Handle, *mut SourceLocation, c_uint, Handle);

#[repr(C)]
#[derive(Clone, Copy)]
struct Cursor {
    kind: c_uint,
    xdata: c_int,
    data: [*const c_void; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SourceLocation {
    data: [*const c_void; 2],
    index: c_uint,
}

#[repr(C)]
struct SourceRange {
    data: [*const c_void; 2],
    start: c_uint,
    end: c_uint,
}

#[repr(C)]
struct CXString {
    data: *const c_void,
    flags: c_uint,
}

macro_rules! api {
    ($($field:ident : $name:literal ($($arg:ty),*) -> $result:ty),+ $(,)?) => {
        pub(super) struct Clang {
            $($field: unsafe extern "C" fn($($arg),*) -> $result,)+
            _library: Library,
        }

        impl Clang {
            pub fn load(path: &Path) -> Result<Self, LoreError> {
                let library = Library::open(path)?;
                // Each symbol is paired with its signature from clang-c/Index.h.
                $(let name = CStr::from_bytes_with_nul(concat!("clang_", $name, "\0").as_bytes()).expect("C symbol");
                let $field = unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $result>(library.symbol(name)?) };)+
                Ok(Self { $($field,)+ _library: library })
            }
        }
    };
}

api!(
    version_string: "getClangVersion"() -> CXString,
    create_index: "createIndex"(c_int, c_int) -> Handle,
    dispose_index: "disposeIndex"(Handle) -> (),
    parse_tu: "parseTranslationUnit2FullArgv"(Handle, *const c_char, *const *const c_char, c_int, Handle, c_uint, c_uint, *mut Handle) -> c_int,
    dispose_tu: "disposeTranslationUnit"(Handle) -> (),
    root_cursor: "getTranslationUnitCursor"(Handle) -> Cursor,
    visit_children: "visitChildren"(Cursor, Visit, Handle) -> c_uint,
    is_null: "Cursor_isNull"(Cursor) -> c_int,
    is_definition: "isCursorDefinition"(Cursor) -> c_uint,
    is_virtual: "CXXMethod_isVirtual"(Cursor) -> c_uint,
    linkage: "getCursorLinkage"(Cursor) -> c_uint,
    spelling: "getCursorSpelling"(Cursor) -> CXString,
    display: "getCursorDisplayName"(Cursor) -> CXString,
    usr: "getCursorUSR"(Cursor) -> CXString,
    canonical: "getCanonicalCursor"(Cursor) -> Cursor,
    parent: "getCursorSemanticParent"(Cursor) -> Cursor,
    referenced: "getCursorReferenced"(Cursor) -> Cursor,
    definition: "getCursorDefinition"(Cursor) -> Cursor,
    cursor_loc: "getCursorLocation"(Cursor) -> SourceLocation,
    cursor_extent: "getCursorExtent"(Cursor) -> SourceRange,
    range_start: "getRangeStart"(SourceRange) -> SourceLocation,
    range_end: "getRangeEnd"(SourceRange) -> SourceLocation,
    expansion_loc: "getExpansionLocation"(SourceLocation, *mut Handle, *mut c_uint, *mut c_uint, *mut c_uint) -> (),
    spelling_loc: "getSpellingLocation"(SourceLocation, *mut Handle, *mut c_uint, *mut c_uint, *mut c_uint) -> (),
    file_name: "getFileName"(Handle) -> CXString,
    file_contents: "getFileContents"(Handle, Handle, *mut usize) -> *const u8,
    inclusions: "getInclusions"(Handle, Include, Handle) -> (),
    diagnostic_count: "getNumDiagnostics"(Handle) -> c_uint,
    diagnostic: "getDiagnostic"(Handle, c_uint) -> Handle,
    severity: "getDiagnosticSeverity"(Handle) -> c_uint,
    format_diagnostic: "formatDiagnostic"(Handle, c_uint) -> CXString,
    dispose_diagnostic: "disposeDiagnostic"(Handle) -> (),
    get_target: "getTranslationUnitTargetInfo"(Handle) -> Handle,
    target_triple: "TargetInfo_getTriple"(Handle) -> CXString,
    dispose_target: "TargetInfo_dispose"(Handle) -> (),
    get_string: "getCString"(CXString) -> *const c_char,
    dispose_string: "disposeString"(CXString) -> (),
);

// CXCursorKind constants from clang-c/Index.h.
const FUNCTION: u32 = 8;
const METHOD: u32 = 21;
const CONSTRUCTOR: u32 = 24;
const DESTRUCTOR: u32 = 25;
const CONVERSION: u32 = 26;
const FUNCTION_TEMPLATE: u32 = 30;
const CALL: u32 = 103;
const LAMBDA: u32 = 144;
const TRANSLATION_UNIT: u32 = 350;
const EXTERNAL_LINKAGE: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Point {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub byte: u32,
}

#[derive(Clone, Debug)]
pub(super) struct Span {
    pub point: Point,
    pub start: u32,
    pub end: u32,
    pub first_line: u32,
    pub last_line: u32,
}

#[derive(Clone)]
pub(super) struct Function {
    pub usr: String,
    pub name: String,
    pub short: String,
    pub signature: String,
    pub owner: PathBuf,
    pub external: bool,
    pub location: Span,
}

pub(super) struct Call {
    pub caller: Function,
    pub callee: Option<Function>,
    pub definition: Option<Function>,
    pub site: Span,
    pub spelling: Option<Point>,
    pub dispatch: &'static str,
}

#[derive(Default)]
pub(super) struct Parsed {
    pub target: String,
    pub diagnostics: Vec<String>,
    pub errors: bool,
    pub inputs: BTreeMap<PathBuf, String>,
    pub definitions: Vec<Function>,
    pub calls: Vec<Call>,
}

struct Tu<'a> {
    api: &'a Clang,
    index: Handle,
    handle: Handle,
}

impl Drop for Tu<'_> {
    fn drop(&mut self) {
        // clang owns both handles. The index outlives its TU.
        unsafe {
            if !self.handle.is_null() {
                (self.api.dispose_tu)(self.handle);
            }
            (self.api.dispose_index)(self.index);
        }
    }
}

impl Clang {
    fn text(&self, value: CXString) -> String {
        // The bytes remain valid until clang_disposeString.
        unsafe {
            let ptr = (self.get_string)(CXString { data: value.data, flags: value.flags });
            let text = if ptr.is_null() { String::new() } else { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
            (self.dispose_string)(value);
            text
        }
    }

    pub fn version(&self) -> String {
        self.text(unsafe { (self.version_string)() })
    }

    pub fn parse(&self, file: &Path, cwd: &Path, argv: &[String]) -> Result<Parsed, LoreError> {
        let file_name = CString::new(crate::config::simplify(file).to_string_lossy().as_bytes()).map_err(error)?;
        let strings: Vec<_> = argv.iter().map(|arg| CString::new(arg.as_bytes())).collect::<Result<_, _>>().map_err(error)?;
        let args: Vec<_> = strings.iter().map(|arg| arg.as_ptr()).collect();
        let index = unsafe { (self.create_index)(0, 0) };
        if index.is_null() {
            return Err(error("libclang could not create an index"));
        }
        let mut tu = Tu { api: self, index, handle: null_mut() };
        let code = unsafe { (self.parse_tu)(tu.index, file_name.as_ptr(), args.as_ptr(), args.len() as c_int, null_mut(), 0, 0, &mut tu.handle) };
        if code != 0 || tu.handle.is_null() {
            return Ok(Parsed { diagnostics: vec![format!("libclang could not parse the translation unit (CXErrorCode {code})")], errors: true, ..Parsed::default() });
        }
        let mut walk = Walk { tu: &tu, primary: file, cwd, paths: HashMap::new(), parsed: Parsed::default() };
        let target = unsafe { (self.get_target)(tu.handle) };
        if !target.is_null() {
            walk.parsed.target = self.text(unsafe { (self.target_triple)(target) });
            unsafe { (self.dispose_target)(target) };
        }
        for n in 0..unsafe { (self.diagnostic_count)(tu.handle) } {
            let item = unsafe { (self.diagnostic)(tu.handle, n) };
            if item.is_null() {
                continue;
            }
            let severity = unsafe { (self.severity)(item) };
            if severity >= 3 {
                walk.parsed.errors = true;
                walk.parsed.diagnostics.push(self.text(unsafe { (self.format_diagnostic)(item, 1) }));
            }
            unsafe { (self.dispose_diagnostic)(item) };
        }
        // Collect handles in callbacks. All Rust path and AST processing happens outside FFI callbacks.
        let mut files = Vec::<Handle>::new();
        unsafe { (self.inclusions)(tu.handle, collect_file, (&mut files as *mut Vec<Handle>).cast()) };
        for file in files {
            if let Some(path) = walk.path(file) {
                if let Some(bytes) = walk.bytes(file) {
                    walk.parsed.inputs.insert(path, digest(bytes));
                }
            }
        }
        walk.visit(unsafe { (self.root_cursor)(tu.handle) }, None);
        Ok(walk.parsed)
    }
}

extern "C" fn collect_file(file: Handle, _: *mut SourceLocation, _: c_uint, files: Handle) {
    // libclang calls this synchronously with the vector supplied by parse().
    unsafe { &mut *files.cast::<Vec<Handle>>() }.push(file);
}

extern "C" fn collect_child(cursor: Cursor, _: Cursor, children: Handle) -> c_uint {
    // The children vector belongs to the current visitChildren invocation.
    unsafe { &mut *children.cast::<Vec<Cursor>>() }.push(cursor);
    1 // CXChildVisit_Continue
}

struct Walk<'a> {
    tu: &'a Tu<'a>,
    primary: &'a Path,
    cwd: &'a Path,
    paths: HashMap<usize, Option<PathBuf>>,
    parsed: Parsed,
}

impl Walk<'_> {
    fn path(&mut self, file: Handle) -> Option<PathBuf> {
        if file.is_null() {
            return None;
        }
        self.paths.entry(file as usize).or_insert_with(|| {
            let raw = self.tu.api.text(unsafe { (self.tu.api.file_name)(file) });
            std::fs::canonicalize(self.cwd.join(raw)).ok()
        }).clone()
    }

    fn bytes(&self, file: Handle) -> Option<&[u8]> {
        let mut length = 0;
        let bytes = unsafe { (self.tu.api.file_contents)(self.tu.handle, file, &mut length) };
        if bytes.is_null() {
            None
        } else {
            // A clang source buffer remains owned by the live TU.
            Some(unsafe { std::slice::from_raw_parts(bytes, length) })
        }
    }

    fn point(&mut self, loc: SourceLocation, spelling: bool) -> Option<(Point, Handle)> {
        let (mut line, mut column, mut byte) = (0, 0, 0);
        let mut file = null_mut();
        let get = if spelling { self.tu.api.spelling_loc } else { self.tu.api.expansion_loc };
        unsafe { get(loc, &mut file, &mut line, &mut column, &mut byte) };
        Some((Point { file: self.path(file)?, line, column, byte }, file))
    }

    fn span(&mut self, cursor: Cursor) -> Option<(Span, Handle)> {
        let api = self.tu.api;
        let (point, file) = self.point(unsafe { (api.cursor_loc)(cursor) }, false)?;
        let first = self.point(unsafe { (api.range_start)((api.cursor_extent)(cursor)) }, false);
        let last = self.point(unsafe { (api.range_end)((api.cursor_extent)(cursor)) }, false);
        let mut span = Span { start: point.byte, end: point.byte, first_line: point.line, last_line: point.line, point };
        if let (Some((first, _)), Some((last, _))) = (first, last) {
            if first.file == span.point.file && last.file == span.point.file && first.byte <= last.byte {
                span.start = first.byte;
                span.end = last.byte;
                span.first_line = first.line;
                span.last_line = last.line;
            }
        }
        Some((span, file))
    }

    fn function(&mut self, cursor: Cursor) -> Option<Function> {
        let api = self.tu.api;
        let canonical = unsafe { (api.canonical)(cursor) };
        let usr = api.text(unsafe { (api.usr)(canonical) });
        if usr.is_empty() {
            return None;
        }
        let short = api.text(unsafe { (api.spelling)(cursor) });
        let display = api.text(unsafe { (api.display)(cursor) });
        let mut parents = Vec::new();
        let mut parent = unsafe { (api.parent)(cursor) };
        while unsafe { (api.is_null)(parent) } == 0 && parent.kind != TRANSLATION_UNIT {
            let name = api.text(unsafe { (api.spelling)(parent) });
            if !name.is_empty() {
                parents.push(name);
            }
            parent = unsafe { (api.parent)(parent) };
        }
        parents.reverse();
        let prefix = if parents.is_empty() { String::new() } else { format!("{}::", parents.join("::")) };
        let owner = self.point(unsafe { (api.cursor_loc)(canonical) }, false)?.0.file;
        Some(Function {
            usr, name: format!("{prefix}{short}"), short, signature: format!("{prefix}{display}"),
            owner, external: unsafe { (api.linkage)(canonical) } == EXTERNAL_LINKAGE,
            location: self.span(cursor)?.0,
        })
    }

    fn call(&mut self, cursor: Cursor, caller: &Function) -> Option<Call> {
        let api = self.tu.api;
        let (site, file) = self.span(cursor)?;
        let referenced = unsafe { (api.referenced)(cursor) };
        let callee = match referenced.kind {
            FUNCTION | METHOD => self.function(unsafe { (api.canonical)(referenced) }),
            CONSTRUCTOR | DESTRUCTOR | CONVERSION => return None,
            _ => None,
        };
        // libclang also visits implicit operator-> and conversion calls. The pilot indexes named calls.
        if callee.as_ref().is_some_and(|target| target.short.starts_with("operator")) {
            return None;
        }
        let spelling = self.point(unsafe { (api.cursor_loc)(cursor) }, true).map(|(p, _)| p).filter(|p| p != &site.point);
        let dispatch = if let Some(target) = &callee {
            let virtual_method = referenced.kind == METHOD && unsafe { (api.is_virtual)(referenced) } != 0;
            let qualified = virtual_method && spelling.is_none() && self.bytes(file)
                .and_then(|bytes| bytes.get(site.start as usize..site.end as usize))
                .is_some_and(|source| qualified_call(source, &target.short));
            if virtual_method && !qualified { "virtual_static_target" } else { "direct" }
        } else {
            "unknown"
        };
        let definition = if callee.is_some() {
            let cursor = unsafe { (api.definition)(referenced) };
            if unsafe { (api.is_null)(cursor) } == 0 { self.function(cursor) } else { None }
        } else {
            None
        };
        Some(Call { caller: caller.clone(), callee, definition, site, spelling, dispatch })
    }

    fn visit(&mut self, cursor: Cursor, enclosing: Option<&Function>) {
        let api = self.tu.api;
        if cursor.kind != TRANSLATION_UNIT {
            let primary = self.point(unsafe { (api.cursor_loc)(cursor) }, false);
            // Header definitions can be targets, but walking their bodies once per
            // includer multiplies the graph by the transitive UBT include closure.
            if !primary.is_some_and(|(p, _)| p.file == self.primary) {
                return;
            }
        }
        let callable = matches!(cursor.kind, FUNCTION | METHOD | CONSTRUCTOR | DESTRUCTOR | CONVERSION | FUNCTION_TEMPLATE);
        let here = if callable && unsafe { (api.is_definition)(cursor) } != 0 {
            self.function(cursor)
        } else {
            None
        };
        if let Some(function) = &here {
            self.parsed.definitions.push(function.clone());
        }
        // A nested callable or lambda must never attribute calls to its enclosing function.
        let active = if callable || cursor.kind == LAMBDA { here.as_ref() } else { enclosing };
        if cursor.kind == CALL {
            if let Some(call) = active.and_then(|caller| self.call(cursor, caller)) {
                self.parsed.calls.push(call);
            }
        }
        let mut children = Vec::<Cursor>::new();
        unsafe { (api.visit_children)(cursor, collect_child, (&mut children as *mut Vec<Cursor>).cast()) };
        for child in children {
            self.visit(child, active);
        }
    }
}

fn qualified_call(bytes: &[u8], short: &str) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else { return false };
    let Some((before, _)) = text.split_once('(') else { return false };
    let Some(qualifier) = before.trim_end().strip_suffix(short).and_then(|p| p.trim_end().strip_suffix("::")) else { return false };
    qualifier.split("::").all(|part| {
        let part = part.trim();
        !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}
