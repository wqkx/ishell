//! 轻量代码高亮 + 缩进探测 + 语法 lint（括号/引号/常见结构），无外部重依赖。
//!
//! 高亮：按扩展名取语言规格（注释/字符串风格 + 关键字集），单遍扫描分词后逐段着色。
//! 覆盖常见语言的注释、字符串、数字、关键字；不做完整 AST，常规语法问题尽量标出。

mod indent;
mod lang;
mod lint;
mod token;

pub use indent::{detect_indent, Indent};
pub use lang::{completion_words, is_code};
pub use lint::{lint_enabled, lint_syntax};
pub use token::{highlight_segment, line_states, LineState};

#[cfg(test)]
mod tests {
    use super::lang::lang_for;
    use super::token::{tokenize, Tok};
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
        assert_eq!(
            tok_at("@app.route\ndef f(): pass\n", "py", "@app.route"),
            Tok::Keyword
        );
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

    /// 回归：高亮器不能因「错误范围落在多字节字符中间」而 panic。
    /// 真实崩溃场景：编辑器同一帧里先算 lint_ranges、再由 handle_input 粘贴改内容、
    /// 最后用**旧** lint_ranges 绘制新内容——旧字节偏移落进汉字中间，token.rs 按该边界
    /// 切片直接 panic，整个应用闪退（未保存改动全丢）。根因已在 view.rs 用「缓存重算
    /// 移到 handle_input 之后」修掉；这里锁住高亮器自身的防御：边界不同步也只影响
    /// 下划线范围，绝不 crash。
    #[test]
    fn highlight_segment_survives_error_range_inside_multibyte_char() {
        // '维' 占 3 字节；构造一个 end 落在它中间的错误范围（真实 panic 里就是 bytes 25..28 的 27）
        let line = "x = [1, 2 3] # 这是多维数组";
        let mid = line.find('维').unwrap() + 1; // 汉字内部，非字符边界
        assert!(!line.is_char_boundary(mid), "用例前提：该偏移确实在字符中间");
        // 不 panic 即通过（此前会 `end byte index N is not a char boundary`）
        // from_ref：本意就是「只含这一个 Range 的切片」，写成 &[0..mid] 会被 clippy 误认为
        // 想要 0..mid 这个区间的所有值（single_range_in_vec_init）。
        let err = 0..mid;
        let job = highlight_segment(
            line,
            0..line.len(),
            "py",
            12.0,
            std::slice::from_ref(&err),
            LineState::Normal,
        );
        assert!(!job.sections.is_empty());
        // 窗口边界本身落在字符中间时同样不能崩
        let job2 = highlight_segment(line, 0..mid, "py", 12.0, &[], LineState::Normal);
        let _ = job2;
    }

}
