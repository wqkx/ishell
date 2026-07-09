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

#[path = "lang_keywords.rs"]
mod lang_keywords;
use lang_keywords::*;

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
