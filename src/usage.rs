//! 目录占用分析。
//!
//! 点「分析」会把这棵树里每个文件、每个子目录的大小都留下。界面一次只铺当前目录的直接子项，
//! 点进任意一层都用已经留下的数字，不再重算。不跟随符号链接，不跨进别的磁盘。
//! 遍历超过 [`WALK_CAP`] 个目录项就停下，并标明数字不完整。
//!
//! 本机用 Rust 自己遍历（可中途停下）。远端一条 GNU `du -a` 把整棵树的大小打出来，
//! 输出按行流式解析（[`LevelParser`]），不占双份内存，超限降级为「不完整」而不是报错。

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::i18n::tr;

/// 界面上最多铺开的行。其余合成一条「其余 N 项」，合计仍然算在总数里。
const SHOW_CAP: usize = 2000;
/// 按名称搜索时保留的最大命中数（按大小）。
const SEARCH_CAP: usize = 500;
/// 一次遍历最多看多少个目录项。超过就停下并标明数字不完整，避免百万级目录把机器拖死。
/// 界面侧按路径留下的大小也以此为总量上限（一棵大树足以装满）。
pub(crate) const WALK_CAP: u64 = 1_500_000;

#[derive(Clone, Debug)]
pub enum UsageKind {
    /// 当前目录的直接子项，子目录带递归大小。
    Level { path: String },
    /// 只列出这一层的名字，不算子目录大小。
    Names { path: String },
    /// 在这棵树里按名称找：普通词是子串；带 `*` `?` `[...]` 按文件名通配；
    /// 带 `/` 按路径通配。结果按大小从大到小。
    Search { path: String, needle: String },
    /// 磁盘 / 挂载点。
    Mounts,
}

#[derive(Clone, Debug)]
pub struct UsageRow {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
pub struct MountPoint {
    /// 挂载点路径，点了就去统计它。
    pub path: String,
    /// 文件系统类型，例如 `ext4`。
    pub label: String,
    /// 设备，例如 `/dev/nvme0n1p5`。用来认出这是哪块盘。
    pub source: String,
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}

#[derive(Clone, Debug)]
pub struct LevelReport {
    pub path: String,
    pub total: u64,
    pub rows: Vec<UsageRow>,
    /// 没铺出来的子项个数（比 [`SHOW_CAP`] 多出来的那些）。
    pub omitted: u64,
    pub omitted_bytes: u64,
    /// 遍历提前停下（目录项太多）。总数不完整。
    pub partial: bool,
    /// 读不了的目录个数（权限）。
    pub denied: u64,
    /// 这棵树里，当前这一层以下每个文件和每个目录的大小。
    pub nested: Vec<(String, u64)>,
}

#[derive(Clone, Debug)]
pub struct SearchReport {
    pub path: String,
    pub needle: String,
    pub rows: Vec<UsageRow>,
    /// 实际命中数（可能大于 `rows.len()`）。
    pub matched: u64,
    pub truncated: bool,
}

/// 这一层的名字。目录在前，然后按名称排。`size` 都是 0，还没分析。
#[derive(Clone, Debug)]
pub struct NameReport {
    pub path: String,
    pub rows: Vec<UsageRow>,
    /// `head` 截断后至少还有这么多项（具体个数未知时为 1）。
    pub omitted: u64,
}

#[derive(Clone, Debug)]
pub enum UsageBody {
    Level(LevelReport),
    Names(NameReport),
    Search(SearchReport),
    Mounts(Vec<MountPoint>),
}

/// 上级目录。`/` 与 `C:` 没有上级。
pub fn parent_path(path: &str) -> Option<String> {
    let t = path.trim_end_matches(['/', '\\']);
    if t.is_empty() || t == "/" {
        return None;
    }
    if t.len() == 2 && t.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let i = t.bytes().rposition(|c| c == b'/' || c == b'\\')?;
    if i == 0 {
        return Some("/".to_string());
    }
    Some(t[..i].to_string())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(crate) fn path_ok(path: &str) -> Result<(), String> {
    if path.is_empty() || path.chars().any(|c| c.is_control()) {
        Err(tr("路径无效", "Invalid path").into())
    } else {
        Ok(())
    }
}

fn base_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

/// 远端整棵树的大小。GNU `du -a -b` 会打出每个文件和每个目录。
///
/// 不带 `-a` 时 `du` 只打目录，文件大小被折进总数，下面几层的文件就没有数字。
/// 目录名单只用来分辨当前这一层里谁是文件夹——`du -x` 不跨设备，挂载点目录在 du
/// 输出里整行缺失，解析时靠这份名单补成占位行。名单必须**先于** du 输出（流式解析
/// 处理 du 行时就要知道谁是目录）。
pub fn remote_level_cmd(path: &str) -> String {
    let q = shell_quote(path);
    format!(
        "root={q}\n\
         echo ISHELL_TY\n\
         find \"$root\" -mindepth 1 -maxdepth 1 -xdev -type d -print 2>/dev/null\n\
         echo ISHELL_DU\n\
         du -x -a -b -- \"$root\"\n\
         echo ISHELL_END\n"
    )
}

/// 远端按名称搜索。普通词是大小写不敏感子串（`-iname` 包在 `*` 里）；带 glob 元字符的
/// 按文件名通配；带 `/` 的按路径通配（`-ipath`，`*` 可以跨过目录）。结果在远端按大小取前
/// [`SEARCH_CAP`] 条。多取一条（`SEARCH_CAP + 1`）：拿到第 501 条就说明命中被截断，
/// 否则截断标记永远立不起来。
pub fn remote_search_cmd(path: &str, needle: &str) -> String {
    let root = shell_quote(path);
    let (flag, pat) = search_pattern(needle);
    let pat = shell_quote(&pat);
    let n = SEARCH_CAP + 1;
    format!(
        "root={root}\n\
         pat={pat}\n\
         find \"$root\" -xdev {flag} \"$pat\" -type f -printf '%s\\t%p\\n' 2>/dev/null | sort -nr | head -n {n}\n"
    )
}

/// 由搜索词推出 `find` 的匹配旗标和模式，规则与本地 [`matcher_for`] 一致。
fn search_pattern(needle: &str) -> (&'static str, String) {
    if needle.contains('/') {
        // 路径模式：两端包 `*`，让它落在搜索根之下任意深度；`*` 在 -ipath 里能跨 `/`。
        ("-ipath", format!("*{needle}*"))
    } else if is_glob(needle) {
        ("-iname", needle.to_string())
    } else {
        ("-iname", find_pattern(needle))
    }
}

/// 搜索词里有没有 glob 元字符（`*`、`?`、成对的 `[...]`）。
fn is_glob(needle: &str) -> bool {
    needle.contains('*') || needle.contains('?') || (needle.contains('[') && needle.contains(']'))
}

pub fn remote_mounts_cmd() -> &'static str {
    "df -T -P -B1 2>/dev/null || df -P -k"
}

/// 只列出这一层的名字，不计算子目录大小。`%y` 是 `d`/`f`/`l`。
///
/// `-printf` 是 GNU 扩展。macOS / BSD / BusyBox 的 find 没有它，直接跑会静默输出空，
/// 界面会误显示「这个目录是空的」。先用一次空 `-printf` 探路，不行就打出标记让调用方报错。
pub fn remote_names_cmd(path: &str) -> String {
    let q = shell_quote(path);
    let n = SHOW_CAP + 1;
    format!(
        "root={q}\n\
         if find \"$root\" -mindepth 1 -maxdepth 1 -printf '' 2>/dev/null; then\n\
         find \"$root\" -mindepth 1 -maxdepth 1 \\( -type d -o -type f -o -type l \\) -printf '%y\\t%s\\t%f\\n' 2>/dev/null | head -n {n}\n\
         else\n\
         echo ISHELL_UNSUPPORTED\n\
         fi\n"
    )
}

/// [`remote_names_cmd`] 在不支持 GNU `-printf` 的系统上打出的标记。
pub const NAMES_UNSUPPORTED_MARK: &str = "ISHELL_UNSUPPORTED";

/// 解析 [`remote_names_cmd`] 的输出。
pub fn parse_names(root: &str, text: &str) -> NameReport {
    let mut rows = Vec::new();
    let mut omitted = 0u64;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((ty, rest)) = line.split_once('\t') else {
            continue;
        };
        // `%y\t%s\t%f`。目录的 `%s` 只是目录项本身，不是里面的占用，留到分析再填。
        let (size, name) = match rest.split_once('\t') {
            Some((sz, name)) => (sz.parse::<u64>().unwrap_or(0), name),
            None => (0, rest),
        };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        if rows.len() >= SHOW_CAP {
            omitted = omitted.saturating_add(1);
            continue;
        }
        let is_dir = ty == "d";
        let path = if root == "/" {
            format!("/{name}")
        } else {
            format!("{root}/{name}")
        };
        rows.push(UsageRow {
            name: name.to_string(),
            path,
            size: if is_dir { 0 } else { size },
            is_dir,
        });
    }
    sort_names(&mut rows);
    NameReport {
        path: root.to_string(),
        rows,
        omitted,
    }
}

fn sort_names(rows: &mut [UsageRow]) {
    rows.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// 把用户输入变成 `find -iname` 的字面子串模式，`*` `?` `[` 不再当通配符。
fn find_pattern(needle: &str) -> String {
    let mut p = String::from("*");
    for c in needle.chars() {
        match c {
            '*' | '?' | '[' | ']' | '\\' => {
                p.push('\\');
                p.push(c);
            }
            _ => p.push(c),
        }
    }
    p.push('*');
    p
}

/// 解析 `du -a` 的整棵树，再加上当前这一层的目录名单（整段文本版，测试用；
/// 线上路径走 [`LevelParser`] 流式喂行）。
#[cfg(test)]
pub fn parse_level(root: &str, text: &str) -> Result<UsageBody, String> {
    let mut p = LevelParser::new(root);
    for line in text.lines() {
        p.feed(line);
    }
    Ok(UsageBody::Level(p.finish(0, false)?))
}

/// `du -a` 输出的流式解析器。一行一行喂，不用把整份输出和中间名单都攒在内存里：
/// 顶层行只留最大的 [`SHOW_CAP`] 条（堆），更深层的大小留到 [`WALK_CAP`] 为止。
/// 内存占用与树大小无关，超大树的输出超限后由调用方标 `capped`，这里降级为 partial。
pub struct LevelParser {
    root: String,
    root_trim: String,
    phase: u8, // 0 还没到标记，1 目录名，2 du，3 结束
    total: u64,
    saw_total: bool,
    /// 顶层目录名单（ISHELL_TY 段，先于 du 输出到达）。
    dirs: std::collections::HashSet<String>,
    /// du 真的统计过的顶层目录名。名单里有、du 里没有的，是异设备挂载点。
    seen_dirs: std::collections::HashSet<String>,
    heap: BinaryHeap<Smaller>,
    children: u64,
    children_bytes: u64,
    nested: Vec<(String, u64)>,
    nested_full: bool,
}

impl LevelParser {
    pub fn new(root: &str) -> Self {
        Self {
            root: root.to_string(),
            root_trim: root.trim_end_matches(['/', '\\']).to_string(),
            phase: 0,
            total: 0,
            saw_total: false,
            dirs: std::collections::HashSet::new(),
            seen_dirs: std::collections::HashSet::new(),
            heap: BinaryHeap::new(),
            children: 0,
            children_bytes: 0,
            nested: Vec::new(),
            nested_full: false,
        }
    }

    pub fn feed(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match line {
            "ISHELL_TY" => {
                self.phase = 1;
                return;
            }
            "ISHELL_DU" => {
                self.phase = 2;
                return;
            }
            "ISHELL_END" => {
                self.phase = 3;
                return;
            }
            _ => {}
        }
        if self.phase == 1 {
            self.dirs.insert(base_name(line).to_string());
            return;
        }
        if self.phase != 2 {
            return;
        }
        let Some((size, path)) = split_size_path(line) else {
            return;
        };
        match under_root(&self.root_trim, path) {
            Some(Under::Here) => {
                self.total = size;
                self.saw_total = true;
            }
            Some(Under::Child(name)) => {
                self.children += 1;
                self.children_bytes = self.children_bytes.saturating_add(size);
                let is_dir = self.dirs.contains(name);
                if is_dir {
                    self.seen_dirs.insert(name.to_string());
                }
                consider(
                    &mut self.heap,
                    UsageRow {
                        path: path.to_string(),
                        name: name.to_string(),
                        size,
                        is_dir,
                    },
                    SHOW_CAP,
                );
            }
            // 再往下的每一层。文件和目录都留下，进去任意深都不用重算。
            Some(Under::Deeper) => {
                if self.nested.len() >= WALK_CAP as usize {
                    self.nested_full = true;
                } else {
                    self.nested.push((path.to_string(), size));
                }
            }
            None => {}
        }
    }

    /// `denied`：远端统计时数出来的无权限目录数（本机解析给 0）。
    /// `capped`：输出超限被提前掐断，数字不完整。
    pub fn finish(mut self, denied: u64, capped: bool) -> Result<LevelReport, String> {
        if self.children == 0 && !self.saw_total {
            return Err(tr(
                "无法统计这个目录（需要 GNU du，或没有权限）",
                "Could not measure this directory (needs GNU du, or permission denied)",
            )
            .into());
        }
        let mut rows: Vec<UsageRow> = self.heap.into_iter().map(|s| s.0).collect();
        let kept = rows.len() as u64;
        let kept_bytes = rows.iter().fold(0u64, |a, r| a.saturating_add(r.size));
        // 名单里有、du 没统计的顶层目录：`du -x` 跨不过设备，整行缺失。补占位行露出来，
        // 大小留 0；界面认得这是挂载点的话会回填磁盘用量。
        for name in &self.dirs {
            if !self.seen_dirs.contains(name) {
                let path = if self.root_trim.is_empty() {
                    format!("/{name}")
                } else {
                    format!("{}/{name}", self.root_trim)
                };
                rows.push(UsageRow {
                    name: name.clone(),
                    path,
                    size: 0,
                    is_dir: true,
                });
            }
        }
        rows.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));
        let total = if self.saw_total {
            self.total
        } else {
            self.children_bytes
        };
        Ok(LevelReport {
            path: std::mem::take(&mut self.root),
            total,
            rows,
            omitted: self.children.saturating_sub(kept),
            omitted_bytes: self.children_bytes.saturating_sub(kept_bytes),
            partial: capped || self.nested_full,
            denied,
            nested: std::mem::take(&mut self.nested),
        })
    }
}

enum Under<'a> {
    Here,
    Child(&'a str),
    /// 根下面超过一层的路径。文件和目录都算。
    Deeper,
}

/// `du` 的一行落在根上、直接子项，还是更深处。对不上前缀就丢掉，不能当成总数。
fn under_root<'a>(root: &str, path: &'a str) -> Option<Under<'a>> {
    let path_trim = path.trim_end_matches(['/', '\\']);
    if path_trim == root || path_trim == "." || path == root {
        return Some(Under::Here);
    }
    let rest = path_trim
        .strip_prefix(root)?
        .trim_start_matches(['/', '\\']);
    if rest.is_empty() {
        return Some(Under::Here);
    }
    if !rest.contains('/') && !rest.contains('\\') {
        Some(Under::Child(rest))
    } else {
        Some(Under::Deeper)
    }
}

fn split_size_path(line: &str) -> Option<(u64, &str)> {
    let line = line.trim();
    let mut parts = line.splitn(2, |c: char| c == '\t' || c == ' ');
    let n = parts.next()?.trim();
    let path = parts.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some((n.parse().ok()?, path))
}

/// 解析 `df` 输出。带 Type 列时丢掉 tmpfs 这类虚拟文件系统。
/// 表头可能是中文（`类型` / `1的块`），不能靠英文单词 `Type` 判断有没有类型列。
pub fn parse_df(text: &str) -> Vec<MountPoint> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let scale = df_scale(header);
    let mut out = Vec::new();
    for line in lines {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 6 {
            continue;
        }
        // 有类型：fs type blocks used avail cap mount...
        // 无类型：fs blocks used avail cap mount...
        // 第二列不是数字就是文件系统类型。中文表头里没有英文 "Type"。
        let has_type = cols[1].parse::<u64>().is_err();
        let (label, blocks, used, avail, mount_at) = if has_type {
            if cols.len() < 7 {
                continue;
            }
            (cols[1], cols[2], cols[3], cols[4], 6)
        } else {
            (cols[0], cols[1], cols[2], cols[3], 5)
        };
        if is_virtual_fs(label) || is_virtual_fs(cols[0]) {
            continue;
        }
        let mount = cols[mount_at..].join(" ");
        if mount.is_empty() || !is_presented_mount(&mount) {
            continue;
        }
        let Ok(blocks) = blocks.parse::<u64>() else {
            continue;
        };
        let Ok(used) = used.parse::<u64>() else {
            continue;
        };
        let Ok(avail) = avail.parse::<u64>() else {
            continue;
        };
        out.push(MountPoint {
            path: mount,
            source: cols[0].to_string(),
            label: label.to_string(),
            size: blocks.saturating_mul(scale),
            used: used.saturating_mul(scale),
            avail: avail.saturating_mul(scale),
        });
    }
    // 按路径排序，子挂载点紧跟在父挂载点后面（/boot 后面是 /boot/efi，而不是按长短打散）。
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn df_scale(header: &str) -> u64 {
    // GNU df 的中文表头是「1的块」「1024的块」，不是 1B-blocks。
    // 「1024的块」里不含「1的块」。
    if header.contains("1B-blocks") || header.contains("1-blocks") || header.contains("1的块") {
        1
    } else if header.contains("1024") {
        1024
    } else {
        512
    }
}

/// 系统自己的挂载点，不是用户要看的磁盘：运行时目录、snap、临时盘。
fn is_presented_mount(path: &str) -> bool {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let path = if trimmed.is_empty() { path } else { trimmed };
    const HIDE: &[&str] = &[
        "/run",
        "/dev",
        "/sys",
        "/proc",
        "/tmp",
        "/snap",
        "/var/snap",
    ];
    !HIDE
        .iter()
        .any(|pre| path == *pre || path.starts_with(&format!("{pre}/")))
}

fn is_virtual_fs(name: &str) -> bool {
    matches!(
        name,
        "tmpfs"
            | "devtmpfs"
            | "overlay"
            | "squashfs"
            | "efivarfs"
            | "proc"
            | "sysfs"
            | "cgroup"
            | "cgroup2"
            | "devpts"
            | "securityfs"
            | "pstore"
            | "bpf"
            | "tracefs"
            | "debugfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "configfs"
            | "ramfs"
            | "autofs"
            | "binfmt_misc"
            | "nsfs"
            | "rpc_pipefs"
            | "none"
            | "udev"
    )
}

pub fn parse_search(root: &str, needle: &str, text: &str) -> UsageBody {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("ISHELL_") {
            continue;
        }
        // size \t y \t path    或    size path（sort 之后仍是这个）
        let (size, rest) = match split_size_path(line) {
            Some(v) => v,
            None => continue,
        };
        let (kind, path) = if let Some((k, p)) = rest.split_once('\t') {
            (k, p)
        } else {
            ("f", rest)
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        rows.push(UsageRow {
            name: path.to_string(),
            path: path.to_string(),
            size,
            is_dir: kind.starts_with('d'),
        });
    }
    // 远端命令多取了一条（SEARCH_CAP + 1）专门用来判断截断；这条不算进命中数。
    let truncated = rows.len() > SEARCH_CAP;
    rows.truncate(SEARCH_CAP);
    let matched = rows.len() as u64;
    UsageBody::Search(SearchReport {
        path: root.to_string(),
        needle: needle.to_string(),
        rows,
        matched,
        truncated,
    })
}

/// 留下最大的 `cap` 条。返回 (留下的, 丢掉的个数, 丢掉的字节)。
fn take_largest(mut rows: Vec<UsageRow>, cap: usize) -> (Vec<UsageRow>, u64, u64) {
    rows.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));
    if rows.len() <= cap {
        return (rows, 0, 0);
    }
    let omitted = (rows.len() - cap) as u64;
    let omitted_bytes = rows[cap..]
        .iter()
        .fold(0u64, |a, r| a.saturating_add(r.size));
    rows.truncate(cap);
    (rows, omitted, omitted_bytes)
}

/// 本机统计。`cancel` 在遍历中被置位就马上停，不回报。
/// `progress` 是界面共享的已扫描条目数，遍历一路累加，界面拿它显示进度。
pub fn run_local(
    kind: UsageKind,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<UsageBody, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(String::new());
    }
    match kind {
        UsageKind::Mounts => Ok(UsageBody::Mounts(local_mounts())),
        UsageKind::Names { path } => {
            path_ok(&path)?;
            Ok(UsageBody::Names(list_names(&path)?))
        }
        UsageKind::Level { path } => {
            path_ok(&path)?;
            Ok(UsageBody::Level(scan_level(
                Path::new(&path),
                cancel,
                progress,
            )?))
        }
        UsageKind::Search { path, needle } => {
            path_ok(&path)?;
            let needle = needle_ok(&needle)?;
            Ok(UsageBody::Search(scan_search(
                Path::new(&path),
                &needle,
                cancel,
                progress,
            )?))
        }
    }
}

pub(crate) fn needle_ok(needle: &str) -> Result<String, String> {
    let needle = needle.trim().to_string();
    if needle.is_empty() || needle.chars().any(|c| c.is_control()) {
        Err(tr("请输入要搜索的名称", "Type a name to search for").into())
    } else {
        Ok(needle)
    }
}

fn list_names(root: &str) -> Result<NameReport, String> {
    let root_path = Path::new(root);
    let rd = std::fs::read_dir(root_path).map_err(|e| io_msg(root_path, &e))?;
    let mut rows = Vec::new();
    let mut omitted = 0u64;
    for ent in rd.flatten() {
        let meta = match std::fs::symlink_metadata(ent.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if rows.len() >= SHOW_CAP {
            omitted = omitted.saturating_add(1);
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        let is_dir = meta.is_dir();
        rows.push(UsageRow {
            name,
            path: ent.path().display().to_string(),
            // 文件大小就是目录项上的长度，不用等分析。目录要等这一层算完才有占用。
            size: if is_dir { 0 } else { meta.len() },
            is_dir,
        });
    }
    sort_names(&mut rows);
    Ok(NameReport {
        path: root.to_string(),
        rows,
        omitted,
    })
}

fn scan_level(
    root: &Path,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<LevelReport, String> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| io_msg(root, &e))?;
    if cancel.load(Ordering::Relaxed) {
        return Err(String::new());
    }
    if !meta.is_dir() {
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        return Ok(LevelReport {
            path: root.display().to_string(),
            total: meta.len(),
            rows: vec![UsageRow {
                name,
                path: root.display().to_string(),
                size: meta.len(),
                is_dir: false,
            }],
            omitted: 0,
            omitted_bytes: 0,
            partial: false,
            denied: 0,
            nested: Vec::new(),
        });
    }
    let dev = dev_of(&meta);
    let stop = AtomicBool::new(false);
    // 遍历计数与界面共享：它一路涨，界面把它当进度显示。
    let visited = progress;
    let denied = AtomicU64::new(0);
    let rd = std::fs::read_dir(root).map_err(|e| io_msg(root, &e))?;
    // 文件名只留最大的 SHOW_CAP 条。平铺目录里有几十万个文件时，不必把每个名字都留在内存里。
    let mut file_heap: BinaryHeap<Smaller> = BinaryHeap::new();
    let mut file_n = 0u64;
    let mut file_bytes = 0u64;
    let mut mounts_here = Vec::new();
    let mut dirs = Vec::new();
    for ent in rd.flatten() {
        if stopped(cancel, &stop) {
            break;
        }
        bump(visited, &stop);
        let path = ent.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                denied.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let name = ent.file_name().to_string_lossy().into_owned();
        let shown = path.display().to_string();
        if meta.file_type().is_symlink() {
            remember_file(
                &mut file_heap,
                &mut file_n,
                &mut file_bytes,
                name,
                shown,
                meta.len(),
                false,
            );
        } else if meta.is_dir() {
            if other_device(&meta, dev) {
                // 挂载点：只露出来，不把另一块磁盘的内容算进这一层。点进去再单独统计。
                mounts_here.push(UsageRow {
                    name,
                    path: shown,
                    size: meta.len(),
                    is_dir: true,
                });
            } else {
                dirs.push((path, name, meta.len()));
            }
        } else {
            remember_file(
                &mut file_heap,
                &mut file_n,
                &mut file_bytes,
                name,
                shown,
                meta.len(),
                false,
            );
        }
    }
    let (dir_rows, nested) = size_dirs(dirs, dev, cancel, &stop, visited, &denied);
    let kept: Vec<UsageRow> = file_heap.into_iter().map(|s| s.0).collect();
    let kept_bytes = kept.iter().fold(0u64, |a, r| a.saturating_add(r.size));
    let dropped_n = file_n.saturating_sub(kept.len() as u64);
    let dropped_bytes = file_bytes.saturating_sub(kept_bytes);
    let mount_bytes = mounts_here
        .iter()
        .fold(0u64, |a, r| a.saturating_add(r.size));
    let dir_bytes = dir_rows.iter().fold(0u64, |a, r| a.saturating_add(r.size));
    let mut rows = dir_rows;
    rows.extend(kept);
    let total = dir_bytes
        .saturating_add(file_bytes)
        .saturating_add(mount_bytes);
    let (mut rows, omitted, omitted_bytes) = take_largest(rows, SHOW_CAP);
    rows.extend(mounts_here);
    rows.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));
    let omitted = omitted.saturating_add(dropped_n);
    let omitted_bytes = omitted_bytes.saturating_add(dropped_bytes);
    Ok(LevelReport {
        path: root.display().to_string(),
        total,
        rows,
        omitted,
        omitted_bytes,
        partial: stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed),
        denied: denied.load(Ordering::Relaxed),
        nested,
    })
}

fn size_dirs(
    dirs: Vec<(PathBuf, String, u64)>,
    dev: u64,
    cancel: &AtomicBool,
    stop: &AtomicBool,
    visited: &AtomicU64,
    denied: &AtomicU64,
) -> (Vec<UsageRow>, Vec<(String, u64)>) {
    if dirs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
        .min(dirs.len());
    let mut chunks: Vec<Vec<(PathBuf, String, u64)>> = (0..n).map(|_| Vec::new()).collect();
    for (i, d) in dirs.into_iter().enumerate() {
        chunks[i % n].push(d);
    }
    let parts: Vec<(Vec<UsageRow>, Vec<(String, u64)>)> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in chunks {
            handles.push(scope.spawn(move || {
                let mut out = Vec::with_capacity(chunk.len());
                let mut nested = Vec::new();
                for (path, name, own) in chunk {
                    if stopped(cancel, stop) {
                        break;
                    }
                    let (size, kids) = measure_dir(&path, own, dev, cancel, stop, visited, denied);
                    nested.extend(kids);
                    out.push(UsageRow {
                        name,
                        path: path.display().to_string(),
                        size,
                        is_dir: true,
                    });
                }
                (out, nested)
            }));
        }
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut rows = Vec::new();
    let mut nested = Vec::new();
    for (r, n) in parts {
        rows.extend(r);
        nested.extend(n);
    }
    (rows, nested)
}

/// 这个目录的总大小，以及它里面每一个文件、每一个子目录的大小。
fn measure_dir(
    path: &Path,
    own: u64,
    dev: u64,
    cancel: &AtomicBool,
    stop: &AtomicBool,
    visited: &AtomicU64,
    denied: &AtomicU64,
) -> (u64, Vec<(String, u64)>) {
    record_tree(path, own, dev, cancel, stop, visited, denied)
}

/// 走进 `path`。返回值是它自己的总大小（含目录项本身）。名单里是它下面的全部文件和目录。
fn record_tree(
    path: &Path,
    own: u64,
    dev: u64,
    cancel: &AtomicBool,
    stop: &AtomicBool,
    visited: &AtomicU64,
    denied: &AtomicU64,
) -> (u64, Vec<(String, u64)>) {
    let mut saved = Vec::new();
    if stopped(cancel, stop) {
        return (own, saved);
    }
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            denied.fetch_add(1, Ordering::Relaxed);
            return (own, saved);
        }
    };
    let mut content = 0u64;
    for ent in rd.flatten() {
        if bump(visited, stop) || stopped(cancel, stop) {
            break;
        }
        let child = ent.path();
        let meta = match std::fs::symlink_metadata(&child) {
            Ok(m) => m,
            Err(_) => {
                denied.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let shown = child.display().to_string();
        if meta.file_type().is_symlink() {
            let len = meta.len();
            content = content.saturating_add(len);
            saved.push((shown, len));
            continue;
        }
        if other_device(&meta, dev) {
            continue;
        }
        let len = meta.len();
        if meta.is_dir() {
            let (sz, kids) = record_tree(&child, len, dev, cancel, stop, visited, denied);
            content = content.saturating_add(sz);
            saved.push((shown, sz));
            saved.extend(kids);
        } else {
            content = content.saturating_add(len);
            saved.push((shown, len));
        }
    }
    (own.saturating_add(content), saved)
}

/// 返回 true 表示已经超过上限，调用方应停。
fn bump(visited: &AtomicU64, stop: &AtomicBool) -> bool {
    let n = visited.fetch_add(1, Ordering::Relaxed);
    if n >= WALK_CAP {
        stop.store(true, Ordering::Relaxed);
        true
    } else {
        false
    }
}

fn stopped(cancel: &AtomicBool, stop: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed)
}

fn scan_search(
    root: &Path,
    needle: &str,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<SearchReport, String> {
    if !root.is_dir() {
        return Err(io_msg(
            root,
            &std::io::Error::new(std::io::ErrorKind::Other, "not a directory"),
        ));
    }
    let dev = std::fs::symlink_metadata(root)
        .map(|m| dev_of(&m))
        .unwrap_or(0);
    let matcher = matcher_for(needle);
    let stop = AtomicBool::new(false);
    let visited = progress;
    let denied = AtomicU64::new(0);
    let mut stack = vec![root.to_path_buf()];
    let mut matched = 0u64;
    // 最小堆：堆顶是当前保留项里最小的，方便被更大的挤掉。
    let mut heap: BinaryHeap<Smaller> = BinaryHeap::new();
    while let Some(dir) = stack.pop() {
        if stopped(cancel, &stop) {
            break;
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                denied.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        for ent in rd.flatten() {
            if bump(visited, &stop) || stopped(cancel, &stop) {
                break;
            }
            let path = ent.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let name = ent.file_name().to_string_lossy().into_owned();
            let is_dir = meta.is_dir() && !meta.file_type().is_symlink();
            let hit = !is_dir
                && match &matcher {
                    // 路径模式对完整路径匹配（前缀的 `*` 吃掉搜索根），其余对文件名。
                    Matcher::Path(_) => matcher.hit(&path.to_string_lossy()),
                    _ => matcher.hit(&name),
                };
            if hit {
                matched += 1;
                consider(
                    &mut heap,
                    UsageRow {
                        name: path.display().to_string(),
                        path: path.display().to_string(),
                        size: meta.len(),
                        is_dir: false,
                    },
                    SEARCH_CAP,
                );
            }
            if is_dir && !other_device(&meta, dev) {
                stack.push(path);
            }
        }
    }
    let mut rows: Vec<UsageRow> = heap.into_iter().map(|s| s.0).collect();
    rows.sort_by(|a, b| b.size.cmp(&a.size).then(a.name.cmp(&b.name)));
    Ok(SearchReport {
        path: root.display().to_string(),
        needle: needle.to_string(),
        rows,
        matched,
        truncated: stop.load(Ordering::Relaxed) || matched > SEARCH_CAP as u64,
    })
}

/// 搜索词的匹配方式，规则与远端 [`search_pattern`] 一致：普通词是大小写不敏感的
/// 文件名子串；带 glob 元字符（`*` `?` `[...]`）按文件名通配；带 `/` 按路径通配
/// （两端包 `*`，`*`/`?` 可以跨过 `/`，和 `find -ipath` 一致）。
enum Matcher {
    /// 文件名里包着这个词（已小写）。
    Substring(String),
    /// 文件名通配。
    Name(String),
    /// 完整路径通配。
    Path(String),
}

fn matcher_for(needle: &str) -> Matcher {
    if needle.contains('/') {
        Matcher::Path(format!("*{needle}*"))
    } else if is_glob(needle) {
        Matcher::Name(needle.to_string())
    } else {
        Matcher::Substring(needle.to_lowercase())
    }
}

impl Matcher {
    fn hit(&self, text: &str) -> bool {
        match self {
            Matcher::Substring(n) => {
                if n.is_empty() {
                    return false;
                }
                if n.is_ascii() && text.is_ascii() {
                    return text
                        .as_bytes()
                        .windows(n.len())
                        .any(|w| w.eq_ignore_ascii_case(n.as_bytes()));
                }
                text.to_lowercase().contains(n.as_str())
            }
            Matcher::Name(pat) | Matcher::Path(pat) => glob_match(pat, text),
        }
    }
}

/// 大小写不敏感的 glob 匹配：`*` 任意串、`?` 单个字符、`[...]` 字符类（`!`/`^` 开头取反，
/// `-` 表范围）。`*` 和 `?` 都能跨过 `/`，和 `find -ipath` 的规则一样。
fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().flat_map(char::to_lowercase).collect();
    let t: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    glob_inner(&p, &t)
}

fn glob_inner(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    // 最近一个 `*` 的位置和它当时吃到的文本位置；后面失配就回来让它多吃一个。
    let (mut star, mut star_t) = (usize::MAX, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = pi;
            star_t = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == '[' {
            match class_at(p, pi, t[ti]) {
                Some((true, next)) => {
                    pi = next;
                    ti += 1;
                }
                Some((false, _)) => {
                    if star == usize::MAX {
                        return false;
                    }
                    pi = star + 1;
                    star_t += 1;
                    ti = star_t;
                }
                // 残缺的 `[` 当字面字符。
                None => {
                    if t[ti] == '[' {
                        pi += 1;
                        ti += 1;
                    } else if star == usize::MAX {
                        return false;
                    } else {
                        pi = star + 1;
                        star_t += 1;
                        ti = star_t;
                    }
                }
            }
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `p[i]` 是 `[`：解析一个字符类，返回 (c 是否命中, 类之后的下标)。
/// 找不到收尾的 `]`（残缺模式）返回 None。
fn class_at(p: &[char], i: usize, c: char) -> Option<(bool, usize)> {
    let mut j = i + 1;
    let negate = j < p.len() && (p[j] == '!' || p[j] == '^');
    if negate {
        j += 1;
    }
    // `[]...]` 打头的 `]` 是字面成员。
    if j < p.len() && p[j] == ']' {
        j += 1;
    }
    let body = j;
    let mut hit = false;
    while j < p.len() && p[j] != ']' {
        if j + 2 < p.len() && p[j + 1] == '-' && p[j + 2] != ']' {
            if p[j] <= c && c <= p[j + 2] {
                hit = true;
            }
            j += 3;
        } else {
            if p[j] == c {
                hit = true;
            }
            j += 1;
        }
    }
    if j >= p.len() || j == body {
        return None;
    }
    Some((hit != negate, j + 1))
}

fn remember_file(
    heap: &mut BinaryHeap<Smaller>,
    n: &mut u64,
    bytes: &mut u64,
    name: String,
    path: String,
    size: u64,
    is_dir: bool,
) {
    *n += 1;
    *bytes = bytes.saturating_add(size);
    consider(
        heap,
        UsageRow {
            name,
            path,
            size,
            is_dir,
        },
        SHOW_CAP,
    );
}

fn consider(heap: &mut BinaryHeap<Smaller>, row: UsageRow, cap: usize) {
    if heap.len() < cap {
        heap.push(Smaller(row));
        return;
    }
    if let Some(top) = heap.peek() {
        if row.size > top.0.size {
            heap.pop();
            heap.push(Smaller(row));
        }
    }
}

/// 最小堆序：小的排在堆顶。
struct Smaller(UsageRow);
impl PartialEq for Smaller {
    fn eq(&self, other: &Self) -> bool {
        self.0.size == other.0.size && self.0.name == other.0.name
    }
}
impl Eq for Smaller {}
impl PartialOrd for Smaller {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Smaller {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .0
            .size
            .cmp(&self.0.size)
            .then(other.0.name.cmp(&self.0.name))
    }
}

fn io_msg(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {e}", path.display())
}

#[cfg(unix)]
fn dev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}
#[cfg(not(unix))]
fn dev_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn other_device(meta: &std::fs::Metadata, root_dev: u64) -> bool {
    dev_of(meta) != root_dev
}
#[cfg(not(unix))]
fn other_device(_meta: &std::fs::Metadata, _root_dev: u64) -> bool {
    false
}

fn local_mounts() -> Vec<MountPoint> {
    #[cfg(target_os = "linux")]
    {
        if let Some(v) = linux_mounts() {
            if !v.is_empty() {
                return v;
            }
        }
    }
    #[cfg(windows)]
    {
        return windows_drives();
    }
    #[cfg(not(windows))]
    {
        if let Ok(out) = std::process::Command::new("df")
            .arg("-P")
            .arg("-k")
            .output()
        {
            if out.status.success() {
                return parse_df(&String::from_utf8_lossy(&out.stdout));
            }
        }
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn linux_mounts() -> Option<Vec<MountPoint>> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let mut cols = line.split_whitespace();
        let dev = cols.next()?;
        let mount = unescape_proc(cols.next()?);
        let fstype = cols.next().unwrap_or("");
        if is_virtual_fs(fstype) || !is_presented_mount(&mount) || !seen.insert(mount.clone()) {
            continue;
        }
        let (size, used, avail) = statvfs_bytes(&mount).unwrap_or((0, 0, 0));
        out.push(MountPoint {
            path: mount,
            source: dev.to_string(),
            label: fstype.to_string(),
            size,
            used,
            avail,
        });
    }
    // 按路径排序，子挂载点紧跟在父挂载点后面（/boot 后面是 /boot/efi，而不是按长短打散）。
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}

#[cfg(target_os = "linux")]
fn unescape_proc(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 4]).unwrap_or(""), 8)
            {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

#[cfg(target_os = "linux")]
fn statvfs_bytes(path: &str) -> Option<(u64, u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bsize = st.f_frsize as u64;
    let total = (st.f_blocks as u64).saturating_mul(bsize);
    let free = (st.f_bfree as u64).saturating_mul(bsize);
    let avail = (st.f_bavail as u64).saturating_mul(bsize);
    Some((total, total.saturating_sub(free), avail))
}

#[cfg(windows)]
fn windows_drives() -> Vec<MountPoint> {
    let mut out = Vec::new();
    for c in b'A'..=b'Z' {
        let path = format!("{}:\\", c as char);
        if !Path::new(&path).exists() {
            continue;
        }
        let (size, used, avail) = windows_space(&path).unwrap_or((0, 0, 0));
        out.push(MountPoint {
            path: path.clone(),
            source: path,
            label: "disk".into(),
            size,
            used,
            avail,
        });
    }
    out
}

#[cfg(windows)]
fn windows_space(path: &str) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut avail_caller = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lp: *const u16,
            free_avail: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail_caller, &mut total, &mut free) };
    if ok == 0 {
        return None;
    }
    Some((total, total.saturating_sub(free), avail_caller))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_unix_and_drive() {
        assert_eq!(parent_path("/var/log"), Some("/var".into()));
        assert_eq!(parent_path("/var"), Some("/".into()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path("C:"), None);
        assert_eq!(parent_path(r"C:\Users"), Some("C:".into()));
    }

    #[test]
    fn parse_level_keeps_total_and_marks_dirs() {
        let text = "\
ISHELL_TY
/data/big
ISHELL_DU
100\t/data/small
5000\t/data/big
5100\t/data
ISHELL_END
";
        let UsageBody::Level(r) = parse_level("/data", text).unwrap() else {
            panic!("level");
        };
        assert_eq!(r.total, 5100);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0].name, "big");
        assert!(r.rows[0].is_dir);
        assert!(!r.rows[1].is_dir);
        assert_eq!(r.rows[1].size, 100);
    }

    #[test]
    fn parse_level_lists_files_from_du_a() {
        let text = "\
ISHELL_TY
/data/big
ISHELL_DU
80\t/data/notes.txt
40\t/data/a file.txt
5000\t/data/big
5120\t/data
ISHELL_END
";
        let UsageBody::Level(r) = parse_level("/data", text).unwrap() else {
            panic!("level");
        };
        let file = r.rows.iter().find(|row| row.name == "notes.txt").unwrap();
        assert!(!file.is_dir);
        assert_eq!(file.size, 80);
        assert!(r.rows.iter().any(|row| row.name == "a file.txt"));
        assert!(r.rows.iter().any(|row| row.is_dir && row.name == "big"));
        assert_eq!(r.total, 5120);
        assert_eq!(r.omitted, 0);
    }

    /// `du -x` 跨不过设备：挂载点在 du 输出里整行缺失，但目录名单里有。
    /// 解析要补一条占位行，不然远端永远看不见挂载点。
    #[test]
    fn parse_level_shows_mount_dirs_du_skipped() {
        let text = "\
ISHELL_TY
/data/big
/data/mnt
ISHELL_DU
5000\t/data/big
5000\t/data
ISHELL_END
";
        let UsageBody::Level(r) = parse_level("/data", text).unwrap() else {
            panic!("level");
        };
        let mnt = r.rows.iter().find(|row| row.name == "mnt").unwrap();
        assert!(mnt.is_dir);
        assert_eq!(mnt.size, 0);
        assert_eq!(mnt.path, "/data/mnt");
        assert!(r
            .rows
            .iter()
            .any(|row| row.name == "big" && row.size == 5000));
    }

    #[test]
    fn parse_level_marks_partial_when_nested_overflows() {
        let mut p = LevelParser::new("/d");
        p.feed("ISHELL_DU");
        for i in 0..(WALK_CAP + 10) {
            p.feed(&format!("1\t/d/a/b/{i}"));
        }
        p.feed("10\t/d/a");
        p.feed("20\t/d");
        p.feed("ISHELL_END");
        let r = p.finish(0, false).unwrap();
        assert!(r.partial);
        assert_eq!(r.nested.len(), WALK_CAP as usize);
        assert_eq!(r.total, 20);
        // 提前掐断（输出超限）：没有总数行时退化为子项合计，也是 partial。
        let mut q = LevelParser::new("/d");
        q.feed("ISHELL_DU");
        q.feed("10\t/d/a");
        let r = q.finish(3, true).unwrap();
        assert!(r.partial);
        assert_eq!(r.total, 10);
        assert_eq!(r.denied, 3);
    }

    #[test]
    fn parse_level_keeps_the_next_directory() {
        let text = "\
ISHELL_TY
/data/src
ISHELL_DU
5\t/data/src/lib/a.rs
15\t/data/src/lib
40\t/data/src
100\t/data
ISHELL_END
";
        let UsageBody::Level(r) = parse_level("/data", text).unwrap() else {
            panic!("level");
        };
        assert_eq!(r.total, 100);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].name, "src");
        assert_eq!(r.rows[0].size, 40);
        assert!(r.nested.contains(&("/data/src/lib".into(), 15)));
        assert!(r.nested.contains(&("/data/src/lib/a.rs".into(), 5)));
    }

    #[test]
    fn parse_names_puts_dirs_first() {
        let text = "f\t12\treadme.txt\nd\t4096\tsrc\nl\t8\tlink\nd\t4096\tlib\n";
        let r = parse_names("/data", &text);
        let names: Vec<_> = r.rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["lib", "src", "link", "readme.txt"]);
        assert!(r.rows[0].is_dir && r.rows[1].is_dir);
        assert!(!r.rows[2].is_dir);
        assert_eq!(r.rows[0].path, "/data/lib");
        assert_eq!(r.rows[0].size, 0);
        assert_eq!(r.rows[2].size, 8);
        assert_eq!(r.rows[3].size, 12);
    }

    #[test]
    fn remote_level_asks_for_files() {
        let cmd = remote_level_cmd("/data");
        assert!(cmd.contains("du -x -a -b"), "{cmd}");
        // 目录名单必须先于 du 输出：流式解析处理 du 行时就要知道谁是目录。
        let ty = cmd.find("ISHELL_TY").unwrap();
        let du = cmd.find("ISHELL_DU").unwrap();
        assert!(ty < du, "{cmd}");
        assert!(!cmd.contains("ISHELL_FI"), "{cmd}");
    }

    #[test]
    fn remote_names_probes_gnu_find() {
        let cmd = remote_names_cmd("/data");
        assert!(cmd.contains("-printf ''"), "{cmd}");
        assert!(cmd.contains(NAMES_UNSUPPORTED_MARK), "{cmd}");
    }

    #[test]
    fn remote_search_takes_one_extra_to_detect_truncation() {
        let cmd = remote_search_cmd("/data", "log");
        assert!(
            cmd.contains(&format!("head -n {}", SEARCH_CAP + 1)),
            "{cmd}"
        );
        // 正好 SEARCH_CAP + 1 行输出：截断标记立起来，命中数只算留下的。
        let mut text = String::new();
        for i in 0..(SEARCH_CAP + 1) {
            text.push_str(&format!("{}\t/d/f{i}.log\n", 1000 - i));
        }
        let UsageBody::Search(r) = parse_search("/d", "log", &text) else {
            panic!("search");
        };
        assert!(r.truncated);
        assert_eq!(r.rows.len(), SEARCH_CAP);
        assert_eq!(r.matched, SEARCH_CAP as u64);
        // 少于上限：不截断。
        let UsageBody::Search(r2) = parse_search("/d", "log", "1\t/d/a.log\n") else {
            panic!("search");
        };
        assert!(!r2.truncated);
        assert_eq!(r2.matched, 1);
    }

    #[test]
    fn parse_df_skips_tmpfs_and_scales() {
        let text = "\
Filesystem Type 1024-blocks Used Available Use% Mounted on
/dev/sda1 ext4 1000 400 500 45% /
tmpfs tmpfs 100 10 90 10% /run
";
        let m = parse_df(text);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "/");
        assert_eq!(m[0].size, 1000 * 1024);
        assert_eq!(m[0].label, "ext4");
        assert_eq!(m[0].source, "/dev/sda1");
    }

    #[test]
    fn parse_df_reads_a_chinese_header() {
        let text = "\
文件系统       类型             1的块         已用         可用 容量 挂载点
tmpfs          tmpfs       1665941504      3194880   1662746624   1% /run
/dev/nvme0n1p5 ext4      117474209792 105241014272   6218563584  95% /
/dev/sdb       ext4      492051456000 355935469568 111093817344  77% /mnt/sda1
/dev/nvme0n1p1 vfat         535805952     33001472    502804480   7% /boot/efi
";
        let m = parse_df(text);
        let paths: Vec<_> = m.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["/", "/boot/efi", "/mnt/sda1"]);
        assert_eq!(m[0].source, "/dev/nvme0n1p5");
        assert_eq!(m[0].label, "ext4");
        assert_eq!(m[0].size, 117474209792);
        assert_eq!(m[2].source, "/dev/sdb");
        assert_eq!(m[2].used, 355935469568);
    }

    #[test]
    fn mounts_follow_path_order() {
        let text = "\
Filesystem Type 1B-blocks Used Available Use% Mounted on
/dev/sda1 ext4 100 40 50 40% /
/dev/sdb1 ext4 100 10 80 10% /boot/efi
/dev/sda2 ext4 100 20 70 20% /boot
";
        let m = parse_df(text);
        let paths: Vec<_> = m.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["/", "/boot", "/boot/efi"]);
    }

    #[test]
    fn mounts_hide_runtime_and_snap() {
        let text = "\
Filesystem Type 1B-blocks Used Available Use% Mounted on
/dev/sda1 ext4 100 40 50 40% /
/dev/sdb1 vfat 100 10 80 10% /boot/efi
tmpfs tmpfs 100 1 90 1% /run
/dev/loop0 squashfs 100 100 0 100% /snap/code/1
/dev/sda2 ext4 100 20 70 20% /var/snap/firefox/common/host-hunspell
/dev/sdc1 ext4 100 30 60 30% /mnt/sda1
";
        let mounts = parse_df(text);
        let paths: Vec<_> = mounts.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["/", "/boot/efi", "/mnt/sda1"]);
    }

    #[test]
    fn find_pattern_escapes_globs() {
        assert_eq!(find_pattern("a*b"), "*a\\*b*");
    }

    #[test]
    fn search_pattern_picks_the_right_find_mode() {
        // 普通词：文件名子串。
        assert_eq!(search_pattern("plain"), ("-iname", "*plain*".to_string()));
        // 带元字符：文件名通配，不再包外围 `*`。
        assert_eq!(search_pattern("*.log"), ("-iname", "*.log".to_string()));
        assert_eq!(
            search_pattern("sys?.log"),
            ("-iname", "sys?.log".to_string())
        );
        assert_eq!(
            search_pattern("log[0-9]"),
            ("-iname", "log[0-9]".to_string())
        );
        // 带 `/`：路径通配，两端包 `*` 落到任意深度。
        assert_eq!(
            search_pattern("src/*.rs"),
            ("-ipath", "*src/*.rs*".to_string())
        );
        assert_eq!(
            search_pattern("/var/log/*.log"),
            ("-ipath", "*/var/log/*.log*".to_string())
        );
        let cmd = remote_search_cmd("/data", "*.log");
        assert!(cmd.contains("pat='*.log'"), "{cmd}");
        assert!(cmd.contains("-iname \"$pat\""), "{cmd}");
        let cmd = remote_search_cmd("/data", "src/*.rs");
        assert!(cmd.contains("pat='*src/*.rs*'"), "{cmd}");
        assert!(cmd.contains("-ipath \"$pat\""), "{cmd}");
    }

    #[test]
    fn glob_match_covers_star_question_and_classes() {
        assert!(glob_match("*.log", "syslog.log"));
        assert!(!glob_match("*.log", "syslog.log.bak"));
        // 大小写不敏感。
        assert!(glob_match("SYS?.TXT", "sys1.txt"));
        // `*` 跨 `/`，和 find -ipath 一致。
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(glob_match("src/*.rs", "src/deep/lib.rs"));
        assert!(glob_match("*/deep/*.rs", "a/src/deep/lib.rs"));
        // `?` 只顶一个字符。
        assert!(!glob_match("sys?.log", "sys12.log"));
        // 字符类：范围、取反。
        assert!(glob_match("file[0-9]", "file7"));
        assert!(!glob_match("file[0-9]", "filex"));
        assert!(glob_match("[!a-c]x", "dx"));
        assert!(!glob_match("[!a-c]x", "bx"));
        // 残缺的 `[` 当字面。
        assert!(glob_match("a[", "a["));
        assert!(!glob_match("a[", "ab"));
        // `*` 可以匹配空。
        assert!(glob_match("a*", "a"));
        assert!(!glob_match("a", "ab"));
        assert!(glob_match("*", "anything/at/all"));
    }

    #[test]
    fn matcher_picks_substring_glob_or_path() {
        assert!(matches!(matcher_for("plain"), Matcher::Substring(_)));
        assert!(matches!(matcher_for("*.log"), Matcher::Name(_)));
        assert!(matches!(matcher_for("src/*.rs"), Matcher::Path(_)));
        // 文件名子串命中；通配模式不再是子串（`foo.log.bak` 不该命中 `*.log`）。
        assert!(matcher_for("log").hit("syslog.log"));
        assert!(matcher_for("*.log").hit("foo.log"));
        assert!(!matcher_for("*.log").hit("foo.log.bak"));
        assert!(matcher_for("deep/*.log").hit("/root/a/deep/x.log"));
        assert!(!matcher_for("deep/*.log").hit("/root/a/other/x.log"));
    }

    #[test]
    fn local_search_takes_globs_and_paths() {
        let dir = std::env::temp_dir().join(format!("ishell-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        std::fs::write(dir.join("foo.log"), b"x").unwrap();
        std::fs::write(dir.join("foo.log.bak"), b"x").unwrap();
        std::fs::write(dir.join("deep").join("c.log"), b"x").unwrap();
        let cancel = AtomicBool::new(false);
        let search = |needle: &str| {
            let UsageBody::Search(s) = run_local(
                UsageKind::Search {
                    path: dir.display().to_string(),
                    needle: needle.into(),
                },
                &cancel,
                &AtomicU64::new(0),
            )
            .unwrap() else {
                panic!("search");
            };
            s
        };
        // 通配：只收 .log 结尾的，.bak 不进来；深层也找得到。
        let s = search("*.log");
        assert_eq!(s.matched, 2, "{:?}", s.rows);
        // 子串：三者都带 log。
        let s = search("log");
        assert_eq!(s.matched, 3, "{:?}", s.rows);
        // 路径模式：只要 deep 下面的。
        let s = search("deep/*.log");
        assert_eq!(s.matched, 1, "{:?}", s.rows);
        assert!(s.rows[0].path.contains("deep"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_level_and_search_see_a_nested_file() {
        let dir = std::env::temp_dir().join(format!("ishell-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub").join("deep")).unwrap();
        std::fs::write(dir.join("a.txt"), vec![1u8; 1000]).unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), vec![2u8; 4000]).unwrap();
        std::fs::write(dir.join("sub").join("deep").join("c.txt"), vec![3u8; 100]).unwrap();
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let UsageBody::Level(level) = run_local(
            UsageKind::Level {
                path: dir.display().to_string(),
            },
            &cancel,
            &progress,
        )
        .unwrap() else {
            panic!("level");
        };
        assert!(progress.load(Ordering::Relaxed) >= 5);
        let sub = level.rows.iter().find(|r| r.name == "sub").unwrap();
        let file = level.rows.iter().find(|r| r.name == "a.txt").unwrap();
        assert!(sub.is_dir && sub.size >= 4100, "{sub:?}");
        assert!(!file.is_dir && file.size == 1000);
        let deep = dir.join("sub").join("deep").display().to_string();
        let deep_file = dir
            .join("sub")
            .join("deep")
            .join("c.txt")
            .display()
            .to_string();
        let inner_file = dir.join("sub").join("b.txt").display().to_string();
        assert!(
            level.nested.iter().any(|(p, s)| p == &deep && *s >= 100),
            "{:?}",
            level.nested
        );
        assert!(
            level
                .nested
                .iter()
                .any(|(p, s)| p == &deep_file && *s == 100),
            "{:?}",
            level.nested
        );
        assert!(
            level
                .nested
                .iter()
                .any(|(p, s)| p == &inner_file && *s == 4000),
            "{:?}",
            level.nested
        );
        let UsageBody::Search(search) = run_local(
            UsageKind::Search {
                path: dir.display().to_string(),
                needle: "b.txt".into(),
            },
            &cancel,
            &AtomicU64::new(0),
        )
        .unwrap() else {
            panic!("search");
        };
        assert_eq!(search.matched, 1);
        assert_eq!(search.rows[0].size, 4000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
