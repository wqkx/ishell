//! 语言规格与补全候选。

/// 语言规格：注释/字符串风格 + 关键字集。
pub(super) struct Lang {
    pub(super) line: &'static [&'static str],
    pub(super) block: Option<(&'static str, &'static str)>,
    pub(super) strings: &'static [char],
    pub(super) keywords: &'static [&'static str],
    /// 多行字符串定界符对 (开, 收)：Python 的 """/'''、Rust 的 r#"…"#、Lua 的 [[…]] 等，可跨行
    pub(super) multi: &'static [(&'static str, &'static str)],
    /// 是否支持字符串前缀（Python 的 f/r/b/u 组合，前缀与字符串一起染色）
    pub(super) str_prefix: bool,
    /// 是否高亮装饰器（Python 的 @xxx.yyy）
    pub(super) deco: bool,
}

/// 语言规格缺省值：各分支用 `..BASE` 只填差异字段。
const BASE: Lang = Lang {
    line: &[],
    block: None,
    strings: &[],
    keywords: &[],
    multi: &[],
    str_prefix: false,
    deco: false,
};

/// Python / TOML 共用的三引号定界符。
const TRIPLES: &[(&str, &str)] = &[("\"\"\"", "\"\"\""), ("'''", "'''")];

// —— 关键字集（常见子集，够用即可）——
const KW_RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "union",
];
const KW_PY: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "match", "case", "self",
];
const KW_JS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
    "interface",
    "type",
    "enum",
    "public",
    "private",
    "readonly",
];
const KW_C: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "class",
    "const",
    "constexpr",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "nullptr",
    "operator",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "true",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];
const KW_GO: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
    "nil",
    "true",
    "false",
];
const KW_JAVA: &[&str] = &[
    "abstract",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "true",
    "false",
    "try",
    "void",
    "volatile",
    "while",
    "var",
];
const KW_SH: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done",
    "in", "function", "select", "return", "local", "export", "readonly", "declare", "echo", "exit",
    "break", "continue", "set", "unset",
];
const KW_RUBY: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif",
    "end", "ensure", "false", "for", "if", "in", "module", "next", "nil", "not", "or", "redo",
    "rescue", "retry", "return", "self", "super", "then", "true", "undef", "unless", "until",
    "when", "while", "yield",
];
const KW_SQL: &[&str] = &[
    "select",
    "from",
    "where",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "drop",
    "alter",
    "add",
    "primary",
    "key",
    "foreign",
    "references",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "on",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "and",
    "or",
    "not",
    "null",
    "as",
    "distinct",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "index",
    "view",
    "union",
    "all",
    "like",
    "between",
    "in",
    "exists",
    "case",
    "when",
    "then",
    "else",
    "end",
];
const KW_LUA: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

// —— 常见内置名（补全候选用；高亮不使用，避免满屏关键字色）——
const BI_PY: &[&str] = &[
    "print",
    "len",
    "range",
    "zip",
    "enumerate",
    "dict",
    "list",
    "set",
    "tuple",
    "str",
    "int",
    "float",
    "bool",
    "open",
    "input",
    "sorted",
    "reversed",
    "sum",
    "min",
    "max",
    "abs",
    "round",
    "map",
    "filter",
    "any",
    "all",
    "isinstance",
    "issubclass",
    "super",
    "type",
    "getattr",
    "setattr",
    "hasattr",
    "repr",
    "hash",
    "id",
    "iter",
    "next",
    "format",
    "bytes",
    "bytearray",
    "frozenset",
    "vars",
    "dir",
    "exec",
    "eval",
    "Exception",
    "ValueError",
    "TypeError",
    "KeyError",
    "IndexError",
    "RuntimeError",
    "StopIteration",
    "__init__",
    "__main__",
    "__name__",
    "self",
];
const BI_RUST: &[&str] = &[
    "String",
    "Vec",
    "Option",
    "Some",
    "None",
    "Result",
    "Ok",
    "Err",
    "Box",
    "Rc",
    "Arc",
    "RefCell",
    "Cell",
    "HashMap",
    "HashSet",
    "BTreeMap",
    "VecDeque",
    "Cow",
    "println",
    "eprintln",
    "format",
    "vec",
    "panic",
    "assert",
    "assert_eq",
    "todo",
    "unimplemented",
    "unwrap",
    "expect",
    "clone",
    "into",
    "from",
    "iter",
    "collect",
    "default",
    "Default",
    "Clone",
    "Copy",
    "Debug",
    "PartialEq",
    "Send",
    "Sync",
    "usize",
    "isize",
];
const BI_JS: &[&str] = &[
    "console",
    "Math",
    "JSON",
    "Promise",
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "Map",
    "Set",
    "Symbol",
    "Error",
    "Date",
    "RegExp",
    "parseInt",
    "parseFloat",
    "isNaN",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "fetch",
    "document",
    "window",
    "require",
    "module",
    "exports",
    "length",
    "push",
    "pop",
    "slice",
    "splice",
    "join",
    "split",
    "filter",
    "reduce",
    "forEach",
    "includes",
    "indexOf",
    "toString",
    "async",
    "await",
];
const BI_GO: &[&str] = &[
    "fmt", "len", "cap", "make", "new", "append", "copy", "delete", "panic", "recover", "error",
    "string", "int", "int64", "float64", "bool", "byte", "rune", "uint", "Println", "Printf",
    "Sprintf", "Errorf", "context", "strings", "strconv", "errors", "time",
];
const BI_C: &[&str] = &[
    "printf",
    "sprintf",
    "fprintf",
    "scanf",
    "malloc",
    "calloc",
    "realloc",
    "free",
    "memcpy",
    "memset",
    "strlen",
    "strcmp",
    "strcpy",
    "strcat",
    "std",
    "string",
    "vector",
    "map",
    "set",
    "pair",
    "make_pair",
    "shared_ptr",
    "unique_ptr",
    "cout",
    "cerr",
    "cin",
    "endl",
    "size_t",
    "nullptr_t",
    "int32_t",
    "int64_t",
    "uint32_t",
    "uint64_t",
    "NULL",
];
const BI_JAVA: &[&str] = &[
    "System",
    "String",
    "Integer",
    "Long",
    "Double",
    "Boolean",
    "Object",
    "List",
    "ArrayList",
    "Map",
    "HashMap",
    "Set",
    "HashSet",
    "Optional",
    "Stream",
    "Exception",
    "RuntimeException",
    "println",
    "valueOf",
    "toString",
    "equals",
    "hashCode",
    "length",
    "size",
];
const BI_SH: &[&str] = &[
    "grep", "sed", "awk", "cut", "sort", "uniq", "head", "tail", "cat", "find", "xargs", "curl",
    "wget", "chmod", "chown", "mkdir", "touch", "printf", "test", "dirname", "basename", "sleep",
    "kill", "wait", "source", "command", "which",
];
const BI_LUA: &[&str] = &[
    "print",
    "pairs",
    "ipairs",
    "tostring",
    "tonumber",
    "type",
    "table",
    "string",
    "math",
    "io",
    "os",
    "require",
    "insert",
    "remove",
    "concat",
    "format",
    "gsub",
    "match",
    "gmatch",
    "setmetatable",
    "getmetatable",
    "pcall",
    "error",
    "assert",
    "select",
    "unpack",
];

/// 补全候选：该语言的关键字 + 常见内置名（缓冲词之外的静态补充）。
pub fn completion_words(ext: &str) -> impl Iterator<Item = &'static str> {
    let lang = lang_for(ext);
    let builtins: &[&str] = match ext {
        "py" | "pyw" => BI_PY,
        "rs" => BI_RUST,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "vue" => BI_JS,
        "go" => BI_GO,
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cu" => BI_C,
        "java" | "kt" | "kts" | "swift" | "scala" => BI_JAVA,
        "sh" | "bash" | "zsh" | "fish" => BI_SH,
        "lua" => BI_LUA,
        _ => &[],
    };
    lang.keywords
        .iter()
        .copied()
        .chain(builtins.iter().copied())
}

pub(super) fn lang_for(ext: &str) -> Lang {
    let cl: &[char] = &['"', '\'', '`'];
    match ext {
        // r#"…"# raw 字符串（常见一级；r##"…"## 多级暂不支持）
        "rs" => Lang {
            line: &["//"],
            block: Some(("/*", "*/")),
            strings: &['"'],
            keywords: KW_RUST,
            multi: &[("r#\"", "\"#")],
            ..BASE
        },
        "py" | "pyw" => Lang {
            line: &["#"],
            strings: &['"', '\''],
            keywords: KW_PY,
            multi: TRIPLES,   // docstring / 多行字符串
            str_prefix: true, // f/r/b/u 前缀
            deco: true,       // @装饰器
            ..BASE
        },
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang {
            line: &["//"],
            block: Some(("/*", "*/")),
            strings: cl,
            keywords: KW_JS,
            deco: true,
            ..BASE
        },
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cu" => Lang {
            line: &["//"],
            block: Some(("/*", "*/")),
            strings: &['"', '\''],
            keywords: KW_C,
            ..BASE
        },
        "go" => Lang {
            line: &["//"],
            block: Some(("/*", "*/")),
            strings: cl,
            keywords: KW_GO,
            ..BASE
        },
        "java" | "kt" | "kts" | "swift" | "scala" => Lang {
            line: &["//"],
            block: Some(("/*", "*/")),
            strings: &['"', '\''],
            keywords: KW_JAVA,
            deco: true,
            ..BASE
        },
        "sh" | "bash" | "zsh" | "fish" => Lang {
            line: &["#"],
            strings: &['"', '\''],
            keywords: KW_SH,
            ..BASE
        },
        "rb" => Lang {
            line: &["#"],
            strings: cl,
            keywords: KW_RUBY,
            ..BASE
        },
        "php" => Lang {
            line: &["//", "#"],
            block: Some(("/*", "*/")),
            strings: &['"', '\''],
            keywords: KW_JS,
            ..BASE
        },
        "sql" => Lang {
            line: &["--"],
            block: Some(("/*", "*/")),
            strings: &['"', '\''],
            keywords: KW_SQL,
            ..BASE
        },
        // [[…]] 长字符串（[==[ 多级暂不支持）；--[[ 块注释在 tokenize 中先于 multi 匹配
        "lua" => Lang {
            line: &["--"],
            block: Some(("--[[", "]]")),
            strings: &['"', '\''],
            keywords: KW_LUA,
            multi: &[("[[", "]]")],
            ..BASE
        },
        // TOML 规范支持 """/''' 多行字符串
        "toml" => Lang {
            line: &["#"],
            strings: &['"', '\''],
            multi: TRIPLES,
            ..BASE
        },
        "ini" | "cfg" | "conf" | "yaml" | "yml" => Lang {
            line: &["#"],
            strings: &['"', '\''],
            ..BASE
        },
        "html" | "xml" | "svg" | "vue" => Lang {
            block: Some(("<!--", "-->")),
            strings: &['"', '\''],
            ..BASE
        },
        "css" | "scss" | "less" => Lang {
            block: Some(("/*", "*/")),
            strings: &['"', '\''],
            ..BASE
        },
        "json" => Lang {
            strings: &['"'],
            ..BASE
        },
        // 未知：C 风格注释 + 常见字符串，无关键字（仍高亮注释/字符串/数字）
        _ => Lang {
            line: &["//", "#"],
            block: Some(("/*", "*/")),
            strings: cl,
            ..BASE
        },
    }
}

/// 是否为已识别的「代码/结构化」文件：决定编辑器是否显示缩进对齐线、折叠、粘性作用域等
/// 依赖缩进结构的辅助功能。未知扩展名（当作纯文本，如 txt/log/md）返回 false。
pub fn is_code(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "py"
            | "pyw"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "mjs"
            | "cjs"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cxx"
            | "hpp"
            | "hh"
            | "cu"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "scala"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "rb"
            | "php"
            | "sql"
            | "lua"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "yaml"
            | "yml"
            | "html"
            | "xml"
            | "svg"
            | "vue"
            | "css"
            | "scss"
            | "less"
            | "json"
    )
}
