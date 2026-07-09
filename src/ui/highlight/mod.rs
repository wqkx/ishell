//! 轻量代码高亮 + 缩进探测 + 语法 lint（括号/引号/常见结构），无外部重依赖。
//!
//! 高亮：按扩展名取语言规格（注释/字符串风格 + 关键字集），单遍扫描分词后逐段着色。
//! 覆盖常见语言的注释、字符串、数字、关键字；不做完整 AST，常规语法问题尽量标出。

mod indent;
mod token;
mod lint;

pub use indent::{detect_indent, Indent};
pub use token::{
    completion_words, highlight_segment, is_code, line_states, LineState,
};
pub use lint::{lint_enabled, lint_syntax};

#[cfg(test)]
mod tests {
    use super::*;
    use super::token::{lang_for, tokenize, Tok};

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
