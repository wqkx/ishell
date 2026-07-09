//! 轻量代码高亮 + 缩进探测 + 语法 lint（括号/引号/常见结构），无外部重依赖。
//!
//! 高亮：按扩展名取语言规格（注释/字符串风格 + 关键字集），单遍扫描分词后逐段着色。
//! 覆盖常见语言的注释、字符串、数字、关键字；不做完整 AST，常规语法问题尽量标出。

use std::ops::Range;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

/// 缩进风格（自动探测）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Indent {
    Spaces(usize),
    Tab,
}

impl Indent {
    /// 一个缩进层级对应的字符串。
    pub fn unit(&self) -> String {
        match self {
            Indent::Spaces(n) => " ".repeat(*n),
            Indent::Tab => "\t".into(),
        }
    }
    /// 人类可读标签（状态栏显示）。
    pub fn label(&self) -> String {
        match self {
            Indent::Spaces(n) => format!("{n} {}", crate::i18n::tr("空格", "spaces")),
            Indent::Tab => "Tab".into(),
        }
    }
}

/// 自动探测文件缩进：Tab 占多数→Tab；否则取「相邻非空行缩进增量」的众数
/// （同票偏好 4）。较旧的 gcd 法会被对齐用的 2 空格行把 4 空格文件误判成 2。
pub fn detect_indent(text: &str) -> Indent {
    let mut tabs = 0usize;
    let mut space_lines = 0usize;
    let mut counts = [0usize; 9]; // 增量 1..=8 的出现次数
    let mut prev = 0usize;
    for line in text.lines() {
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let lead = line.bytes().take_while(|b| *b == b' ').count();
        if lead == line.len() {
            continue; // 空白行不参与
        }
        if lead > 0 {
            space_lines += 1;
        }
        if lead > prev && lead - prev <= 8 {
            counts[lead - prev] += 1;
        }
        prev = lead;
    }
    if tabs > 0 && tabs >= space_lines {
        return Indent::Tab;
    }
    let best = (1..=8).max_by_key(|&d| (counts[d], usize::from(d == 4))).unwrap_or(4);
    if counts[best] == 0 {
        return Indent::Spaces(4); // 没有可判定的缩进，默认 4
    }
    Indent::Spaces(best)
}

/// 语言规格：注释/字符串风格 + 关键字集。
struct Lang {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    strings: &'static [char],
    keywords: &'static [&'static str],
    /// 多行字符串定界符对 (开, 收)：Python 的 """/'''、Rust 的 r#"…"#、Lua 的 [[…]] 等，可跨行
    multi: &'static [(&'static str, &'static str)],
    /// 是否支持字符串前缀（Python 的 f/r/b/u 组合，前缀与字符串一起染色）
    str_prefix: bool,
    /// 是否高亮装饰器（Python 的 @xxx.yyy）
    deco: bool,
}

/// 语言规格缺省值：各分支用 `..BASE` 只填差异字段。
const BASE: Lang = Lang { line: &[], block: None, strings: &[], keywords: &[], multi: &[], str_prefix: false, deco: false };

/// Python / TOML 共用的三引号定界符。
const TRIPLES: &[(&str, &str)] = &[("\"\"\"", "\"\"\""), ("'''", "'''")];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// 近白底上的配色（仿 VSCode Light）。
fn color(t: Tok) -> Color32 {
    match t {
        Tok::Plain => Color32::from_rgb(0x24, 0x29, 0x2e),   // 近黑
        Tok::Comment => Color32::from_rgb(0x00, 0x80, 0x00), // 绿
        Tok::Str => Color32::from_rgb(0xa3, 0x15, 0x15),     // 暗红
        Tok::Num => Color32::from_rgb(0x09, 0x86, 0x58),     // 青绿
        Tok::Keyword => Color32::from_rgb(0x00, 0x00, 0xd0), // 蓝
    }
}

// —— 关键字集（常见子集，够用即可）——
const KW_RUST: &[&str] = &["as","async","await","break","const","continue","crate","dyn","else","enum","extern","false","fn","for","if","impl","in","let","loop","match","mod","move","mut","pub","ref","return","self","Self","static","struct","super","trait","true","type","unsafe","use","where","while","union"];
const KW_PY: &[&str] = &["and","as","assert","async","await","break","class","continue","def","del","elif","else","except","False","finally","for","from","global","if","import","in","is","lambda","None","nonlocal","not","or","pass","raise","return","True","try","while","with","yield","match","case","self"];
const KW_JS: &[&str] = &["async","await","break","case","catch","class","const","continue","debugger","default","delete","do","else","export","extends","false","finally","for","function","if","import","in","instanceof","let","new","null","of","return","static","super","switch","this","throw","true","try","typeof","undefined","var","void","while","yield","interface","type","enum","public","private","readonly"];
const KW_C: &[&str] = &["auto","bool","break","case","char","class","const","constexpr","continue","default","delete","do","double","else","enum","extern","false","float","for","goto","if","inline","int","long","namespace","new","nullptr","operator","private","protected","public","register","return","short","signed","sizeof","static","struct","switch","template","this","true","typedef","typename","union","unsigned","using","virtual","void","volatile","while"];
const KW_GO: &[&str] = &["break","case","chan","const","continue","default","defer","else","fallthrough","for","func","go","goto","if","import","interface","map","package","range","return","select","struct","switch","type","var","nil","true","false"];
const KW_JAVA: &[&str] = &["abstract","boolean","break","byte","case","catch","char","class","const","continue","default","do","double","else","enum","extends","final","finally","float","for","if","implements","import","instanceof","int","interface","long","native","new","null","package","private","protected","public","return","short","static","super","switch","synchronized","this","throw","throws","true","false","try","void","volatile","while","var"];
const KW_SH: &[&str] = &["if","then","else","elif","fi","case","esac","for","while","until","do","done","in","function","select","return","local","export","readonly","declare","echo","exit","break","continue","set","unset"];
const KW_RUBY: &[&str] = &["alias","and","begin","break","case","class","def","defined?","do","else","elsif","end","ensure","false","for","if","in","module","next","nil","not","or","redo","rescue","retry","return","self","super","then","true","undef","unless","until","when","while","yield"];
const KW_SQL: &[&str] = &["select","from","where","insert","into","values","update","set","delete","create","table","drop","alter","add","primary","key","foreign","references","join","left","right","inner","outer","on","group","by","order","having","limit","offset","and","or","not","null","as","distinct","count","sum","avg","min","max","index","view","union","all","like","between","in","exists","case","when","then","else","end"];
const KW_LUA: &[&str] = &["and","break","do","else","elseif","end","false","for","function","goto","if","in","local","nil","not","or","repeat","return","then","true","until","while"];

// —— 常见内置名（补全候选用；高亮不使用，避免满屏关键字色）——
const BI_PY: &[&str] = &["print","len","range","zip","enumerate","dict","list","set","tuple","str","int","float","bool","open","input","sorted","reversed","sum","min","max","abs","round","map","filter","any","all","isinstance","issubclass","super","type","getattr","setattr","hasattr","repr","hash","id","iter","next","format","bytes","bytearray","frozenset","vars","dir","exec","eval","Exception","ValueError","TypeError","KeyError","IndexError","RuntimeError","StopIteration","__init__","__main__","__name__","self"];
const BI_RUST: &[&str] = &["String","Vec","Option","Some","None","Result","Ok","Err","Box","Rc","Arc","RefCell","Cell","HashMap","HashSet","BTreeMap","VecDeque","Cow","println","eprintln","format","vec","panic","assert","assert_eq","todo","unimplemented","unwrap","expect","clone","into","from","iter","collect","default","Default","Clone","Copy","Debug","PartialEq","Send","Sync","usize","isize"];
const BI_JS: &[&str] = &["console","Math","JSON","Promise","Object","Array","String","Number","Boolean","Map","Set","Symbol","Error","Date","RegExp","parseInt","parseFloat","isNaN","setTimeout","setInterval","clearTimeout","fetch","document","window","require","module","exports","length","push","pop","slice","splice","join","split","filter","reduce","forEach","includes","indexOf","toString","async","await"];
const BI_GO: &[&str] = &["fmt","len","cap","make","new","append","copy","delete","panic","recover","error","string","int","int64","float64","bool","byte","rune","uint","Println","Printf","Sprintf","Errorf","context","strings","strconv","errors","time"];
const BI_C: &[&str] = &["printf","sprintf","fprintf","scanf","malloc","calloc","realloc","free","memcpy","memset","strlen","strcmp","strcpy","strcat","std","string","vector","map","set","pair","make_pair","shared_ptr","unique_ptr","cout","cerr","cin","endl","size_t","nullptr_t","int32_t","int64_t","uint32_t","uint64_t","NULL"];
const BI_JAVA: &[&str] = &["System","String","Integer","Long","Double","Boolean","Object","List","ArrayList","Map","HashMap","Set","HashSet","Optional","Stream","Exception","RuntimeException","println","valueOf","toString","equals","hashCode","length","size"];
const BI_SH: &[&str] = &["grep","sed","awk","cut","sort","uniq","head","tail","cat","find","xargs","curl","wget","chmod","chown","mkdir","touch","printf","test","dirname","basename","sleep","kill","wait","source","command","which"];
const BI_LUA: &[&str] = &["print","pairs","ipairs","tostring","tonumber","type","table","string","math","io","os","require","insert","remove","concat","format","gsub","match","gmatch","setmetatable","getmetatable","pcall","error","assert","select","unpack"];

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
    lang.keywords.iter().copied().chain(builtins.iter().copied())
}

fn lang_for(ext: &str) -> Lang {
    let cl: &[char] = &['"', '\'', '`'];
    match ext {
        // r#"…"# raw 字符串（常见一级；r##"…"## 多级暂不支持）
        "rs" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: &['"'], keywords: KW_RUST, multi: &[("r#\"", "\"#")], ..BASE },
        "py" | "pyw" => Lang {
            line: &["#"],
            strings: &['"', '\''],
            keywords: KW_PY,
            multi: TRIPLES,   // docstring / 多行字符串
            str_prefix: true, // f/r/b/u 前缀
            deco: true,       // @装饰器
            ..BASE
        },
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: cl, keywords: KW_JS, deco: true, ..BASE },
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cu" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_C, ..BASE },
        "go" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: cl, keywords: KW_GO, ..BASE },
        "java" | "kt" | "kts" | "swift" | "scala" => Lang { line: &["//"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_JAVA, deco: true, ..BASE },
        "sh" | "bash" | "zsh" | "fish" => Lang { line: &["#"], strings: &['"', '\''], keywords: KW_SH, ..BASE },
        "rb" => Lang { line: &["#"], strings: cl, keywords: KW_RUBY, ..BASE },
        "php" => Lang { line: &["//", "#"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_JS, ..BASE },
        "sql" => Lang { line: &["--"], block: Some(("/*", "*/")), strings: &['"', '\''], keywords: KW_SQL, ..BASE },
        // [[…]] 长字符串（[==[ 多级暂不支持）；--[[ 块注释在 tokenize 中先于 multi 匹配
        "lua" => Lang { line: &["--"], block: Some(("--[[", "]]")), strings: &['"', '\''], keywords: KW_LUA, multi: &[("[[", "]]")], ..BASE },
        // TOML 规范支持 """/''' 多行字符串
        "toml" => Lang { line: &["#"], strings: &['"', '\''], multi: TRIPLES, ..BASE },
        "ini" | "cfg" | "conf" | "yaml" | "yml" => Lang { line: &["#"], strings: &['"', '\''], ..BASE },
        "html" | "xml" | "svg" | "vue" => Lang { block: Some(("<!--", "-->")), strings: &['"', '\''], ..BASE },
        "css" | "scss" | "less" => Lang { block: Some(("/*", "*/")), strings: &['"', '\''], ..BASE },
        "json" => Lang { strings: &['"'], ..BASE },
        // 未知：C 风格注释 + 常见字符串，无关键字（仍高亮注释/字符串/数字）
        _ => Lang { line: &["//", "#"], block: Some(("/*", "*/")), strings: cl, ..BASE },
    }
}

/// 是否为已识别的「代码/结构化」文件：决定编辑器是否显示缩进对齐线、折叠、粘性作用域等
/// 依赖缩进结构的辅助功能。未知扩展名（当作纯文本，如 txt/log/md）返回 false。
pub fn is_code(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "py" | "pyw" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs"
            | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cu"
            | "go" | "java" | "kt" | "kts" | "swift" | "scala"
            | "sh" | "bash" | "zsh" | "fish" | "rb" | "php" | "sql" | "lua"
            | "toml" | "ini" | "cfg" | "conf" | "yaml" | "yml"
            | "html" | "xml" | "svg" | "vue" | "css" | "scss" | "less" | "json"
    )
}

/// 扫描普通引号字符串，返回结束字节位（含收尾引号）。
/// `raw` 时不处理 `\` 转义（Python r"…"）；非反引号串遇换行即结束，避免漏闭合时染色整篇。
fn scan_str(text: &str, start: usize, quote: char, raw: bool) -> usize {
    let n = text.len();
    let mut j = start + quote.len_utf8();
    while j < n {
        let cj = text[j..].chars().next().unwrap();
        if cj == '\\' && !raw {
            j += cj.len_utf8();
            if j < n {
                j += text[j..].chars().next().unwrap().len_utf8();
            }
            continue;
        }
        if cj == quote {
            return j + cj.len_utf8();
        }
        if cj == '\n' && quote != '`' {
            return j;
        }
        j += cj.len_utf8();
    }
    n
}

/// 扫描多行字符串（open 已在 start 处匹配），返回结束字节位（含收尾定界符）；未闭合染到文末。
fn scan_pair(text: &str, start: usize, open: &str, close: &str) -> usize {
    text[start + open.len()..]
        .find(close)
        .map(|e| start + open.len() + e + close.len())
        .unwrap_or(text.len())
}

/// 单遍分词，返回 (字节范围, 类别) 列表（连续 Plain 已合并）。
fn tokenize(text: &str, lang: &Lang) -> Vec<(usize, usize, Tok)> {
    let mut segs: Vec<(usize, usize, Tok)> = Vec::new();
    let n = text.len();
    let mut i = 0usize;
    while i < n {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();
        // 多行字符串（Python 三引号 / Rust r#"…"# / Lua [[…]] 等；先于单字符引号匹配）
        if let Some((o, cl2)) = lang.multi.iter().find(|(o, _)| rest.starts_with(o)) {
            let end = scan_pair(text, i, o, cl2);
            segs.push((i, end, Tok::Str));
            i = end;
            continue;
        }
        // 块注释（先于行注释：Lua 的 --[[ 以 -- 开头，反序会被误判为行注释）
        if let Some((bs, be)) = lang.block {
            if let Some(after) = rest.strip_prefix(bs) {
                let end = after.find(be).map(|e| i + bs.len() + e + be.len()).unwrap_or(n);
                segs.push((i, end, Tok::Comment));
                i = end;
                continue;
            }
        }
        // 行注释
        if lang.line.iter().any(|p| rest.starts_with(*p)) {
            let end = rest.find('\n').map(|e| i + e).unwrap_or(n);
            segs.push((i, end, Tok::Comment));
            i = end;
            continue;
        }
        // 字符串
        if lang.strings.contains(&c) {
            let j = scan_str(text, i, c, false);
            segs.push((i, j, Tok::Str));
            i = j;
            continue;
        }
        // 前缀字符串（Python 的 f"…" / r'…' / rb"…" 等）：前缀与字符串一体染色；
        // raw（含 r/R）时 \ 不作转义
        if lang.str_prefix && matches!(c, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U') {
            let pfx = rest.chars().take_while(|ch| matches!(ch, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U')).count();
            if pfx <= 2 {
                let after = &rest[pfx..];
                if let Some((o, cl2)) = lang.multi.iter().find(|(o, _)| after.starts_with(o)) {
                    let end = scan_pair(text, i + pfx, o, cl2);
                    segs.push((i, end, Tok::Str));
                    i = end;
                    continue;
                }
                if let Some(q) = after.chars().next().filter(|q| lang.strings.contains(q)) {
                    let raw = rest[..pfx].chars().any(|ch| ch == 'r' || ch == 'R');
                    let end = scan_str(text, i + pfx, q, raw);
                    segs.push((i, end, Tok::Str));
                    i = end;
                    continue;
                }
            }
            // 非字符串前缀 → 落入下方标识符分支
        }
        // 数字（含 0x/0b 前缀、下划线分隔、1e-5 科学计数）
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj.is_ascii_alphanumeric() || cj == '.' || cj == '_' {
                    j += cj.len_utf8();
                    // e/E 后允许一个正负号（科学计数）
                    if (cj == 'e' || cj == 'E') && j < n {
                        let cs = text[j..].chars().next().unwrap();
                        if (cs == '+' || cs == '-') && text[j + 1..].chars().next().is_some_and(|d| d.is_ascii_digit()) {
                            j += 1;
                        }
                    }
                } else {
                    break;
                }
            }
            segs.push((i, j, Tok::Num));
            i = j;
            continue;
        }
        // 装饰器（@ident.ident）。注意：无论后面是否跟标识符都必须消费 @ 前进——
        // Plain 合并循环遇 @ 会 break，若此处不前进（如 `a @ b` 矩阵乘、行尾 @）会死循环
        if lang.deco && c == '@' {
            let mut j = i + 1;
            let mut any = false;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj == '_' || cj == '.' || cj.is_alphanumeric() {
                    any = true;
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            segs.push((i, j, if any { Tok::Keyword } else { Tok::Plain }));
            i = j;
            continue;
        }
        // 标识符 / 关键字
        if c == '_' || c.is_alphabetic() {
            let mut j = i;
            while j < n {
                let cj = text[j..].chars().next().unwrap();
                if cj == '_' || cj.is_alphanumeric() {
                    j += cj.len_utf8();
                } else {
                    break;
                }
            }
            let word = &text[i..j];
            let t = if lang.keywords.contains(&word) { Tok::Keyword } else { Tok::Plain };
            segs.push((i, j, t));
            i = j;
            continue;
        }
        // 其余（标点/空白）：合并为一段 Plain，直到下一个可能的特殊起点
        let start = i;
        loop {
            if i >= n {
                break;
            }
            let rest = &text[i..];
            let c = rest.chars().next().unwrap();
            if lang.line.iter().any(|p| rest.starts_with(*p)) {
                break;
            }
            if let Some((bs, _)) = lang.block {
                if rest.starts_with(bs) {
                    break;
                }
            }
            // 多行定界符可能不以引号/字母开头（如 Lua 的 [[），需显式让位
            if lang.multi.iter().any(|(o, _)| rest.starts_with(o)) {
                break;
            }
            if lang.deco && c == '@' {
                break;
            }
            if lang.strings.contains(&c) || c.is_ascii_digit() || c == '_' || c.is_alphabetic() {
                break;
            }
            i += c.len_utf8();
        }
        if i > start {
            segs.push((start, i, Tok::Plain));
        }
    }
    segs
}

/// 行首状态（跨行结构）：常规 / 在块注释内 / 在多行字符串内（记录收尾定界符）。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LineState {
    Normal,
    InComment,
    InStr(&'static str),
}

/// 全文单遍扫描，得出每一行行首所处的状态（供逐行高亮时正确延续
/// docstring / 块注释的着色）。行数 = 换行符数 + 1。
pub fn line_states(text: &str, ext: &str) -> Vec<LineState> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let mut states = vec![LineState::Normal; starts.len()];
    let lang = lang_for(ext);
    if lang.multi.is_empty() && lang.block.is_none() && !lang.strings.contains(&'`') {
        return states; // 该语言没有跨行结构
    }
    for (s, e, tok) in tokenize(text, &lang) {
        let state = match tok {
            Tok::Comment => LineState::InComment, // 行注释不跨行，下方循环自然不命中
            Tok::Str => {
                // 该串的收尾定界符：多行定界符（可能带 f/r/b 前缀字母，检查 s 与跳过前缀后的 k
                // 两个位置）或反引号（JS 模板串 / Go raw 串，可跨行）；单行串不会命中下方循环
                let mut k = s;
                while k < e && text.as_bytes()[k].is_ascii_alphabetic() {
                    k += 1;
                }
                if let Some((_, cl2)) = lang.multi.iter().find(|(o, _)| text[s..].starts_with(o) || text[k..].starts_with(o)) {
                    LineState::InStr(cl2)
                } else if text.as_bytes().get(k) == Some(&b'`') {
                    LineState::InStr("`")
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        // 标记落在该 token 内部的行首（第一个 > s 的行起点起，到 e 之前）
        let mut li = starts.partition_point(|&p| p <= s);
        while li < starts.len() && starts[li] < e {
            states[li] = state;
            li += 1;
        }
    }
    states
}

/// 按行首状态起始分词：先把「延续中的多行结构」收尾，再对剩余部分常规分词。
fn tokenize_with_state(text: &str, lang: &Lang, state: LineState) -> Vec<(usize, usize, Tok)> {
    let mut segs: Vec<(usize, usize, Tok)> = Vec::new();
    let mut i = 0usize;
    match state {
        LineState::InComment => {
            if let Some((_, be)) = lang.block {
                let end = text.find(be).map(|e| e + be.len()).unwrap_or(text.len());
                segs.push((0, end, Tok::Comment));
                i = end;
            }
        }
        LineState::InStr(delim) => {
            let end = text.find(delim).map(|e| e + delim.len()).unwrap_or(text.len());
            segs.push((0, end, Tok::Str));
            i = end;
        }
        LineState::Normal => {}
    }
    if i < text.len() {
        segs.extend(tokenize(&text[i..], lang).into_iter().map(|(s, e, t)| (s + i, e + i, t)));
    }
    segs
}

/// 对整行 `line` 按 `state` 分词，仅对窗口 `win`（字节范围）生成布局；
/// `errors` 的字节范围以窗口起点为 0（调用方已裁剪平移）。
/// 分词整行是为了跨行/行内状态正确；布局只做窗口，超长行不付整行 layout 成本。
pub fn highlight_segment(line: &str, win: Range<usize>, ext: &str, font_size: f32, errors: &[Range<usize>], state: LineState) -> LayoutJob {
    let lang = lang_for(ext);
    // 只分词到窗口右界即可：窗口之后的 token 在下方 `s.min(win.end)` 全被裁掉、纯属浪费。
    // 超长行（日志/JSON/minified）只在左侧可见时，此举把每帧整行分词降为「仅可见前缀」，
    // 根治「拖到底部有超长行时卡顿一下」。win.end 已是字符边界（调用方 char_to_byte 得到）。
    let scan_end = win.end.min(line.len());
    let toks = tokenize_with_state(&line[..scan_end], &lang, state);
    let font = FontId::monospace(font_size);
    let mut job = LayoutJob::default();
    for (s, e, tok) in toks {
        // 裁剪到窗口并转为窗口相对偏移
        let (s, e) = (s.max(win.start), e.min(win.end));
        if s >= e {
            continue;
        }
        let (rs, re) = (s - win.start, e - win.start);
        // 段内若与某错误范围相交，则进一步按边界切分以便只给错误处加下划线
        let mut p = rs;
        while p < re {
            let err = errors.iter().find(|r| r.start <= p && p < r.end && r.start < re);
            let (seg_end, underline) = if let Some(r) = err {
                (r.end.min(re), true)
            } else {
                // 下一个错误起点（若在本段内）作为切点
                let next = errors.iter().filter(|r| r.start > p && r.start < re).map(|r| r.start).min().unwrap_or(re);
                (next, false)
            };
            let mut fmt = TextFormat::simple(font.clone(), color(tok));
            if underline {
                fmt.underline = egui::Stroke::new(1.0, Color32::from_rgb(0xd0, 0x20, 0x20));
            }
            job.append(&line[win.start + p..win.start + seg_end], 0.0, fmt);
            p = seg_end;
        }
    }
    job
}

/// 是否对该语言做括号 lint。仅常见、括号配平规则明确的编程语言才判断；
/// 文本/标记/配置/shell 等不判（避免对不认识的文本误报）。
pub fn lint_enabled(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "cu"
            | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs"
            | "go" | "java" | "kt" | "kts" | "swift" | "scala"
            | "py" | "pyw" | "rb" | "php" | "lua"
            | "json" | "css" | "scss" | "less"
    )
}

/// 常规语法 lint（不依赖外部库/模块解析）：
/// - 括号 () [] {} 配对
/// - 未闭合字符串 / 多行字符串
/// - Python：列表/元组/调用参数中缺逗号、`:` 后空块、混用 Tab/空格缩进
/// - JSON：尾逗号、非法结构的粗检
///
/// 返回：出问题的 0 基行号、字节范围下划线、概述文案。
pub fn lint_syntax(text: &str, ext: &str) -> (Vec<usize>, Vec<Range<usize>>, Option<String>) {
    let lang = lang_for(ext);
    let segs = tokenize(text, &lang);
    let mut bad: Vec<usize> = Vec::new();

    // —— 1) 括号配对（仅 Plain 区）——
    let mut stack: Vec<(char, usize)> = Vec::new();
    for (s, e, tok) in &segs {
        if *tok != Tok::Plain {
            continue;
        }
        let mut p = *s;
        while p < *e {
            let c = text[p..].chars().next().unwrap();
            match c {
                '(' | '[' | '{' => stack.push((c, p)),
                ')' | ']' | '}' => {
                    let open = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    match stack.last() {
                        Some((o, _)) if *o == open => {
                            stack.pop();
                        }
                        _ => bad.push(p),
                    }
                }
                _ => {}
            }
            p += c.len_utf8();
        }
    }
    for (_, pos) in &stack {
        bad.push(*pos);
    }

    // —— 2) 未闭合字符串（tokenize 遇换行/文末截断且无收尾定界符）——
    for (s, e, tok) in &segs {
        if *tok != Tok::Str || *e <= *s {
            continue;
        }
        if str_segment_unclosed(text, *s, *e, &lang) {
            bad.push(e.saturating_sub(1).max(*s));
        }
    }

    // —— 3) 语言特化 ——
    match ext {
        "py" | "pyw" => lint_python(text, &segs, &mut bad),
        "json" => lint_json(text, &segs, &mut bad),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "rs" | "go" | "c" | "h" | "cpp" | "cc" | "cxx" | "java" => {
            lint_comma_in_brackets(text, &segs, &mut bad);
        }
        _ => {}
    }

    bad.sort_unstable();
    bad.dedup();
    let ranges: Vec<Range<usize>> = bad.iter().map(|&b| {
        let end = next_utf8_end(text, b);
        b..end
    }).collect();
    let lines: Vec<usize> = bad.iter().map(|&b| text[..b.min(text.len())].bytes().filter(|x| *x == b'\n').count()).collect();
    let msg = if bad.is_empty() {
        None
    } else {
        Some(match crate::i18n::current() {
            crate::i18n::Lang::Zh => format!("⚠ {} 处语法问题", bad.len()),
            crate::i18n::Lang::En => format!("⚠ {} syntax issue(s)", bad.len()),
        })
    };
    (lines, ranges, msg)
}

fn next_utf8_end(text: &str, b: usize) -> usize {
    if b >= text.len() {
        return b;
    }
    text[b..].chars().next().map(|c| b + c.len_utf8()).unwrap_or(b + 1)
}

/// 判断字符串 token 是否未正确闭合。
fn str_segment_unclosed(text: &str, s: usize, e: usize, lang: &Lang) -> bool {
    let slice = &text[s..e];
    let body = if lang.str_prefix {
        let pfx = slice
            .chars()
            .take_while(|c| matches!(c, 'r' | 'b' | 'f' | 'u' | 'R' | 'B' | 'F' | 'U'))
            .map(|c| c.len_utf8())
            .sum::<usize>()
            .min(slice.len());
        // 前缀后必须是引号/多行定界符，否则不是前缀串
        if pfx > 0 && pfx < slice.len() {
            &slice[pfx..]
        } else {
            slice
        }
    } else {
        slice
    };
    for (o, cl) in lang.multi {
        if body.starts_with(o) {
            // 未闭合：整段不以收尾定界符结束（scan_pair 染到文末时亦如此）
            return body.len() == o.len() || !body.ends_with(cl);
        }
    }
    let Some(q) = body.chars().next().filter(|c| lang.strings.contains(c)) else {
        return false;
    };
    if body.len() == q.len_utf8() {
        return true; // 只有开引号
    }
    if !body.ends_with(q) {
        return true;
    }
    // 以引号结束，但可能是转义的假收尾：…\"
    if body.len() > q.len_utf8() {
        let before = &body[..body.len() - q.len_utf8()];
        // 奇数个连续反斜杠 → 引号被转义 → 未真正闭合
        let mut bs = 0usize;
        for c in before.chars().rev() {
            if c == '\\' {
                bs += 1;
            } else {
                break;
            }
        }
        if bs % 2 == 1 {
            return true;
        }
    }
    false
}

/// 在 () [] {} 内的相邻「值」之间缺逗号时标出（跳过注释/字符串）。
/// 启发式：值结束后仅空白再接另一值，且无逗号 → 缺逗号（如 `[1, 2 3]`）。
fn lint_comma_in_brackets(text: &str, segs: &[(usize, usize, Tok)], bad: &mut Vec<usize>) {
    let mut depth = 0i32;
    // 上一「值」token（跳过纯空白 Plain，避免 `2` 与 `3` 被空格段隔开）
    let mut prev_val: Option<(usize, usize, Tok)> = None;
    for &(s, e, tok) in segs {
        if tok == Tok::Comment {
            continue;
        }
        // 纯空白 Plain：不打断值邻接，但若含括号仍更新深度
        let plain_ws = tok == Tok::Plain && text[s..e].chars().all(|c| c.is_whitespace());
        if tok == Tok::Plain {
            let mut p = s;
            while p < e {
                let c = text[p..].chars().next().unwrap();
                match c {
                    '(' | '[' | '{' => {
                        depth += 1;
                        prev_val = None; // 新括号层重新开始
                    }
                    ')' | ']' | '}' => {
                        depth -= 1;
                        prev_val = None;
                    }
                    ',' => prev_val = None, // 已有逗号，重置
                    _ => {}
                }
                p += c.len_utf8();
            }
        }
        if plain_ws {
            continue;
        }
        if depth <= 0 {
            prev_val = None;
            continue;
        }
        // Plain 里可能混有标点+标识；按「值起点/终点」判断
        if let Some((ps, pe, ptok)) = prev_val {
            if looks_like_value_end(text, ps, pe, ptok) && looks_like_value_start(text, s, e, tok) {
                let between = &text[pe..s];
                if !between.contains(',') && between.chars().all(|c| c.is_whitespace()) {
                    bad.push(s);
                }
            }
        }
        // 更新 prev：仅当本段可作为值终点时记住；纯标点 Plain（如 `, `）清空
        if looks_like_value_end(text, s, e, tok) {
            prev_val = Some((s, e, tok));
        } else if tok == Tok::Plain {
            // 含逗号等分隔符的 Plain 已在上面清空；其它标点也断开
            if text[s..e].contains(',') {
                prev_val = None;
            }
        }
    }
}

fn looks_like_value_end(text: &str, s: usize, e: usize, tok: Tok) -> bool {
    match tok {
        Tok::Num | Tok::Str | Tok::Keyword => true,
        Tok::Plain => {
            let t = text[s..e].trim_end();
            t.ends_with(')') || t.ends_with(']') || t.ends_with('}')
                || t.chars().next_back().is_some_and(|c| c == '_' || c.is_alphanumeric())
        }
        Tok::Comment => false,
    }
}

fn looks_like_value_start(text: &str, s: usize, e: usize, tok: Tok) -> bool {
    match tok {
        Tok::Num | Tok::Str => true,
        Tok::Keyword => {
            // True/False/None/null 等可作值；控制关键字不当值起点
            matches!(
                &text[s..e],
                "True" | "False" | "None" | "true" | "false" | "null" | "undefined" | "nil"
            )
        }
        Tok::Plain => {
            let t = text[s..e].trim_start();
            t.starts_with('(') || t.starts_with('[') || t.starts_with('{')
                || t.chars().next().is_some_and(|c| c == '_' || c.is_alphanumeric() || c == '"' || c == '\'' || c == '`')
        }
        Tok::Comment => false,
    }
}

fn lint_python(text: &str, segs: &[(usize, usize, Tok)], bad: &mut Vec<usize>) {
    lint_comma_in_brackets(text, segs, bad);

    // 混用 Tab / 空格缩进
    let mut saw_space = false;
    let mut saw_tab = false;
    for (i, line) in text.split('\n').enumerate() {
        let mut sp = false;
        let mut tb = false;
        for b in line.bytes() {
            match b {
                b' ' => sp = true,
                b'\t' => tb = true,
                _ => break,
            }
        }
        if sp {
            saw_space = true;
        }
        if tb {
            saw_tab = true;
        }
        if sp && tb {
            // 同行混用：标在行首
            let off = text.split('\n').take(i).map(|l| l.len() + 1).sum::<usize>();
            bad.push(off);
        }
    }
    if saw_space && saw_tab {
        // 文件级混用：标在第一个 Tab 缩进行
        let mut off = 0usize;
        for line in text.split('\n') {
            if line.starts_with('\t') {
                bad.push(off);
                break;
            }
            off += line.len() + 1;
        }
    }

    // `:` 行尾后下一非空行缩进未加深（空块 / 缺 body）——仅启发式，跳过已是 pass/... 的情况
    let lines: Vec<&str> = text.split('\n').collect();
    let mut off = 0usize;
    for i in 0..lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        let code = strip_py_line_comment(trimmed);
        if code.ends_with(':') && !code.ends_with("::") {
            let lead = line.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
            // 找下一非空、非注释行
            let mut j = i + 1;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.is_empty() || t.starts_with('#') {
                    j += 1;
                    continue;
                }
                break;
            }
            if j >= lines.len() {
                // 文件以 `:` 结尾且无 body
                bad.push(off + lead.max(1) - 1);
            } else {
                let next = lines[j];
                let nlead = next.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
                if nlead <= lead && !next.trim_start().starts_with('#') {
                    // 同级或更浅 → 可能缺缩进块（except/elif/else/finally/case 同级合法）
                    let nt = next.trim_start();
                    let ok_peer = nt.starts_with("elif ")
                        || nt.starts_with("else:")
                        || nt.starts_with("except")
                        || nt.starts_with("finally:")
                        || nt.starts_with("case ")
                        || nt == "else:"
                        || nt == "finally:";
                    if !ok_peer {
                        bad.push(off + trimmed.len().saturating_sub(1));
                    }
                }
            }
        }
        off += line.len() + 1;
    }
}

fn strip_py_line_comment(line: &str) -> &str {
    // 粗略：不在字符串内的 #（足够用于 `:` 行尾判断）
    let mut in_s: Option<char> = None;
    let mut chars = line.char_indices();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = in_s {
            if c == '\\' {
                chars.next();
                continue;
            }
            if c == q {
                in_s = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            in_s = Some(c);
            continue;
        }
        if c == '#' {
            return line[..i].trim_end();
        }
    }
    line.trim_end()
}

fn lint_json(text: &str, segs: &[(usize, usize, Tok)], bad: &mut Vec<usize>) {
    // 尾逗号：`,` 后仅空白再遇 `]`/`}`
    let mut i = 0usize;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        // 跳过字符串段
        if let Some(&(s, e, Tok::Str)) = segs.iter().find(|&&(s, e, _)| s <= i && i < e) {
            i = e;
            let _ = s;
            continue;
        }
        if bytes[i] == b',' {
            let mut j = i + 1;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < bytes.len() && matches!(bytes[j], b']' | b'}') {
                bad.push(i);
            }
        }
        i += 1;
    }
    lint_comma_in_brackets(text, segs, bad);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在 `text` 中找到 `pat` 的位置，返回覆盖它的 token 类别。
    fn tok_at(text: &str, ext: &str, pat: &str) -> Tok {
        let lang = lang_for(ext);
        let pos = text.find(pat).expect("pat 必须存在");
        tokenize(text, &lang)
            .into_iter()
            .find(|(s, e, _)| *s <= pos && pos < *e)
            .map(|(_, _, t)| t)
            .expect("pos 必须被某个 token 覆盖")
    }

    #[test]
    fn py_triple_docstring() {
        // 跨行 docstring 整段是字符串，内部的 def 不能当关键字/代码
        let src = "def f():\n    '''doc line1\n    def not_code\n    '''\n    return 1\n";
        assert_eq!(tok_at(src, "py", "doc line1"), Tok::Str);
        assert_eq!(tok_at(src, "py", "not_code"), Tok::Str);
        assert_eq!(tok_at(src, "py", "return"), Tok::Keyword);
        // 双引号版本
        let src2 = "x = \"\"\"a\nb\"\"\"\ny = 1\n";
        assert_eq!(tok_at(src2, "py", "a\nb"), Tok::Str);
        assert_eq!(tok_at(src2, "py", "y"), Tok::Plain);
    }

    #[test]
    fn py_prefix_strings() {
        assert_eq!(tok_at("s = f'hi {x}'\n", "py", "f'hi"), Tok::Str);
        assert_eq!(tok_at("p = r'C:\\dir'\n", "py", "r'C:"), Tok::Str);
        assert_eq!(tok_at("b = rb\"x\"\n", "py", "rb\""), Tok::Str);
        // raw 串的收尾引号不被 \ 吞掉：r'\' 后面的 done 是普通代码
        assert_eq!(tok_at("t = r'\\'\ndone = 1\n", "py", "done"), Tok::Plain);
        // 普通标识符不受前缀误伤
        assert_eq!(tok_at("for i in fs:\n", "py", "for"), Tok::Keyword);
        assert_eq!(tok_at("fs = 1\n", "py", "fs"), Tok::Plain);
    }

    #[test]
    fn py_decorator_and_numbers() {
        assert_eq!(tok_at("@app.route\ndef f(): pass\n", "py", "@app.route"), Tok::Keyword);
        // 科学计数整体是数字
        let src = "x = 1e-5\n";
        let lang = lang_for("py");
        let segs = tokenize(src, &lang);
        let num = segs.iter().find(|(_, _, t)| *t == Tok::Num).unwrap();
        assert_eq!(&src[num.0..num.1], "1e-5");
    }

    #[test]
    fn detect_indent_4_with_alignment_lines() {
        // 4 空格缩进文件，夹杂 2/6 空格的对齐行（旧 gcd 法会误判成 2）
        let src = "def f():\n    x = 1\n    y = (a +\n      b)\n    if x:\n        z = 2\n";
        assert_eq!(detect_indent(src), Indent::Spaces(4));
        // 纯 2 空格文件仍判 2
        let src2 = "def f():\n  x = 1\n  if x:\n    y = 2\n";
        assert_eq!(detect_indent(src2), Indent::Spaces(2));
        // Tab 文件
        assert_eq!(detect_indent("a:\n\tb\n\tc\n"), Indent::Tab);
    }

    #[test]
    fn py_at_no_infinite_loop() {
        // @ 后非标识符（矩阵乘 a @ b、行尾裸 @）不得死循环
        let lang = lang_for("py");
        let segs = tokenize("c = a @ b\n@\n", &lang);
        assert!(!segs.is_empty());
    }

    #[test]
    fn py_line_states() {
        let src = "a = 1\n'''\ndoc\n'''\nb = 2\n";
        let st = line_states(src, "py");
        assert_eq!(st[0], LineState::Normal);
        assert_eq!(st[1], LineState::Normal); // ''' 开始行的行首仍是常规
        assert_eq!(st[2], LineState::InStr("'''"));
        assert_eq!(st[3], LineState::InStr("'''")); // 收尾行行首仍在串内
        assert_eq!(st[4], LineState::Normal);
        // 未闭合：染到文末（文末空行行首无内容，状态无关紧要，不作断言）
        let st2 = line_states("x = '''\ntail", "py");
        assert_eq!(st2[1], LineState::InStr("'''"));
    }

    #[test]
    fn backtick_and_raw_multiline_states() {
        // JS 模板串跨行：中间行行首在串内
        let js = "const s = `line1\nline2`;\nlet x = 1;\n";
        let st = line_states(js, "js");
        assert_eq!(st[1], LineState::InStr("`"));
        assert_eq!(st[2], LineState::Normal);
        // Go 反引号 raw 串
        let go = "s := `a\nb`\nx := 1\n";
        let st = line_states(go, "go");
        assert_eq!(st[1], LineState::InStr("`"));
        assert_eq!(st[2], LineState::Normal);
        // Rust r#"…"# raw 字符串
        let rs = "let s = r#\"a\nb\"#;\nlet x = 1;\n";
        let st = line_states(rs, "rs");
        assert_eq!(st[1], LineState::InStr("\"#"));
        assert_eq!(st[2], LineState::Normal);
        // r#type 原始标识符不能被误当 raw 串
        assert_eq!(tok_at("let r#type = 1;\n", "rs", "type"), Tok::Keyword);
    }

    #[test]
    fn lua_long_string_and_toml_triple() {
        // Lua [[…]] 长字符串（区别于 --[[ ]] 注释）
        let lua = "s = [[a\nb]]\nx = 1\n";
        assert_eq!(tok_at(lua, "lua", "a\nb"), Tok::Str);
        let st = line_states(lua, "lua");
        assert_eq!(st[1], LineState::InStr("]]"));
        // TOML 多行字符串
        let toml = "k = \"\"\"\nv\n\"\"\"\n";
        let st = line_states(toml, "toml");
        assert_eq!(st[1], LineState::InStr("\"\"\""));
    }

    #[test]
    fn rust_block_comment_states() {
        let src = "let a = 1; /* c1\nc2\n*/ let b = 2;\n";
        let st = line_states(src, "rs");
        assert_eq!(st[0], LineState::Normal);
        assert_eq!(st[1], LineState::InComment);
        assert_eq!(st[2], LineState::InComment);
    }

    #[test]
    fn lua_block_comment() {
        // --[[ ]] 是块注释，不能被 -- 行注释规则截断到行尾
        let src = "--[[ line1\nline2 ]] x = 1\n";
        assert_eq!(tok_at(src, "lua", "line2"), Tok::Comment);
        assert_eq!(tok_at(src, "lua", "x = 1"), Tok::Plain);
    }

    #[test]
    fn lint_py_missing_comma() {
        let (lines, ranges, msg) = lint_syntax("x = [1, 2 3]\n", "py");
        assert!(msg.is_some(), "应检出缺逗号");
        assert!(!ranges.is_empty());
        assert!(lines.contains(&0));
        // 合法列表不报
        let (_, _, msg2) = lint_syntax("x = [1, 2, 3]\n", "py");
        assert!(msg2.is_none());
    }

    #[test]
    fn lint_py_unclosed_string_and_bracket() {
        let (_, ranges, msg) = lint_syntax("s = 'hello\nx = 1\n", "py");
        assert!(msg.is_some());
        assert!(!ranges.is_empty());
        let (_, _, msg2) = lint_syntax("x = (1 + 2\n", "py");
        assert!(msg2.is_some());
    }

    #[test]
    fn lint_json_trailing_comma() {
        let (_, ranges, msg) = lint_syntax("{\"a\": 1,}\n", "json");
        assert!(msg.is_some());
        assert!(!ranges.is_empty());
    }
}
