//! 缩进风格探测。

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
    let best = (1..=8)
        .max_by_key(|&d| (counts[d], usize::from(d == 4)))
        .unwrap_or(4);
    if counts[best] == 0 {
        return Indent::Spaces(4); // 没有可判定的缩进，默认 4
    }
    Indent::Spaces(best)
}
