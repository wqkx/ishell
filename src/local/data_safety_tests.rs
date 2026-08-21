//! **数据安全回归套件**：盯「文件被无端修改/删除」这一类问题。
//!
//! 这个项目在这上面栽过好几次，形态高度一致——某个操作**破坏了它本不该碰的数据**：
//!
//! - `fs::copy(src, src)`：复制粘贴回原目录，把源文件清成 0 字节；
//! - `place_extracted_dir` 先删旧目标再 rename，删成功+rename 失败 = 两头皆空；
//! - 上传直接 `open(目标, TRUNCATE)` 再流式写，一断线就把远端原文件留成半截；
//! - 一条「只改权限」的 SETSTAT 实际捎带了 `size=0`，把文件清空；
//! - 收藏夹用陈旧快照整体覆盖磁盘，抹掉别的标签页刚加的条目。
//!
//! 单点修复挡不住下一次，因为每次出问题的都是**新写的那条路径**。所以这里不按函数写测试，
//! 而是把几条跨操作的不变量固定下来，任何新增/改动的本机文件操作都应当被它们套住：
//!
//! - **A 未点名的数据一律不动**：操作 X 时，与 X 无关的文件必须逐字节不变；
//! - **B 失败即不动**：操作失败时，源与目标都必须和操作前完全一致；
//! - **C 移动不丢**：移动之后，载荷必须能在源或目标之一找到——绝不允许两头皆空；
//! - **D 不跟随符号链接越界**：删/复制一条链接不得改动它指向的东西；
//! - **E 只读操作不写盘**：列目录/读文件跑完，整棵树的快照必须一模一样。
//!
//! 远端（SFTP）侧的对应保障在 `ssh/xfer/upload.rs` 的 live 集成测试里（需真 sshd，
//! 见 `scripts/test-live-sftp.sh`）；本文件只管本机文件系统这一半，无需任何外部依赖。

use std::collections::BTreeMap;
use std::path::Path;

use super::tests::{block_on, test_sink, TmpDir};
use super::*;

// ───────────────────────────── 快照与断言 ─────────────────────────────

/// 一个条目在快照里的样子。用 `symlink_metadata` 采集，所以**符号链接记为链接本身**，
/// 不会跟过去——否则「删链接」和「删目标」在快照上看起来一样，D 类不变量就测不出来了。
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File(Vec<u8>),
    Dir,
    Symlink(String),
}

type Snapshot = BTreeMap<String, Entry>;

/// 递归快照一棵树：相对路径 → 内容/类型。
fn snapshot(root: &Path) -> Snapshot {
    fn walk(root: &Path, cur: &Path, out: &mut Snapshot) {
        let Ok(rd) = std::fs::read_dir(cur) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .into_owned();
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if md.file_type().is_symlink() {
                let target = std::fs::read_link(&p)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.insert(rel, Entry::Symlink(target));
            } else if md.is_dir() {
                out.insert(rel, Entry::Dir);
                walk(root, &p, out);
            } else {
                out.insert(rel, Entry::File(std::fs::read(&p).unwrap_or_default()));
            }
        }
    }
    let mut out = Snapshot::new();
    walk(root, root, &mut out);
    out
}

/// **A 类断言**：除了 `changed` 点名的相对路径，其余条目必须逐字节不变，
/// 也不许凭空多出或少掉。
fn assert_only_changed(before: &Snapshot, after: &Snapshot, changed: &[&str]) {
    let allowed = |k: &str| changed.iter().any(|c| k == *c || k.starts_with(&format!("{c}/")));
    for (k, v) in before {
        if allowed(k) {
            continue;
        }
        match after.get(k) {
            None => panic!("未点名的条目被删了：{k}"),
            Some(now) => assert_eq!(
                now, v,
                "未点名的条目被改了：{k}（这正是「数据被无端修改」那一类）"
            ),
        }
    }
    for k in after.keys() {
        if !allowed(k) && !before.contains_key(k) {
            panic!("凭空多出一个未点名的条目：{k}");
        }
    }
}

/// 整棵树完全没变。
fn assert_identical(before: &Snapshot, after: &Snapshot) {
    assert_only_changed(before, after, &[]);
    assert_eq!(before.len(), after.len(), "条目数量变了");
}

/// 造一棵有点内容的树：几个文件 + 一个子目录，内容各不相同便于定位。
fn make_tree(dir: &Path) {
    std::fs::create_dir_all(dir.join("sub")).expect("mkdir sub");
    std::fs::write(dir.join("a.txt"), b"AAA").expect("w a");
    std::fs::write(dir.join("b.txt"), b"BBB").expect("w b");
    std::fs::write(dir.join("c.log"), b"CCC").expect("w c");
    std::fs::write(dir.join("sub/inner.txt"), b"INNER").expect("w inner");
}

fn run_copy_move(srcs: &[std::path::PathBuf], dest: &Path, do_move: bool, policy: ConflictPolicy) {
    let (sink, _rx) = test_sink();
    block_on(copy_move(
        srcs.iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        &dest.to_string_lossy(),
        do_move,
        policy,
        &sink,
    ));
}

// ───────────────────────────── A：未点名的数据一律不动 ─────────────────────────────

/// 复制一个文件到别的目录，**其余每一个文件都必须逐字节不变**，源也不变。
#[test]
fn copy_touches_nothing_but_its_own_destination() {
    let tmp = TmpDir::new("ds-copy");
    let src = tmp.0.join("src");
    let dst = tmp.0.join("dst");
    make_tree(&src);
    make_tree(&dst);
    std::fs::write(dst.join("keepme"), b"MUST SURVIVE").expect("w keep");

    let before = snapshot(&tmp.0);
    run_copy_move(&[src.join("a.txt")], &dst, false, ConflictPolicy::Overwrite);
    let after = snapshot(&tmp.0);

    // 只允许 dst/a.txt 变（它本来就存在，内容从 AAA 覆盖成 AAA——但仍点名，避免误判）
    assert_only_changed(&before, &after, &["dst/a.txt"]);
    assert_eq!(
        std::fs::read(src.join("a.txt")).expect("源还在"),
        b"AAA",
        "复制把源改了"
    );
}

/// 删除只能删掉点名的那几条，同目录里其余文件一条都不能少。
#[test]
fn delete_only_removes_the_paths_it_was_given() {
    let tmp = TmpDir::new("ds-del");
    make_tree(&tmp.0);
    let before = snapshot(&tmp.0);

    let (sink, _rx) = test_sink();
    block_on(delete_many(
        vec![
            tmp.0.join("a.txt").to_string_lossy().into_owned(),
            tmp.0.join("c.log").to_string_lossy().into_owned(),
        ],
        &sink,
    ));
    let after = snapshot(&tmp.0);

    assert_only_changed(&before, &after, &["a.txt", "c.log"]);
    assert!(!tmp.0.join("a.txt").exists() && !tmp.0.join("c.log").exists());
    assert_eq!(std::fs::read(tmp.0.join("b.txt")).expect("b 还在"), b"BBB");
    assert_eq!(
        std::fs::read(tmp.0.join("sub/inner.txt")).expect("inner 还在"),
        b"INNER"
    );
}

// ───────────────────────────── B：失败即不动 ─────────────────────────────

/// 目标目录不存在 → 操作必须失败，且**源分毫未动、也不许凭空造出目标**。
#[test]
fn a_failed_copy_leaves_everything_exactly_as_it_was() {
    let tmp = TmpDir::new("ds-failcopy");
    let src = tmp.0.join("src");
    make_tree(&src);
    let before = snapshot(&tmp.0);

    run_copy_move(
        &[src.join("a.txt")],
        &tmp.0.join("nonexistent-dir"),
        false,
        ConflictPolicy::Overwrite,
    );

    assert_identical(&before, &snapshot(&tmp.0));
}

/// 同上，移动版本：失败之后源必须还在（这是「两头皆空」那一类事故的守门人）。
#[test]
fn a_failed_move_never_loses_the_payload() {
    let tmp = TmpDir::new("ds-failmove");
    let src = tmp.0.join("src");
    make_tree(&src);
    let before = snapshot(&tmp.0);

    run_copy_move(
        &[src.join("a.txt")],
        &tmp.0.join("nonexistent-dir"),
        true, // move
        ConflictPolicy::Overwrite,
    );

    assert_identical(&before, &snapshot(&tmp.0));
    assert_eq!(
        std::fs::read(src.join("a.txt")).expect("移动失败后源必须还在"),
        b"AAA"
    );
}

/// 目标是一个**文件**而不是目录 → 必须拒绝，且那个文件不能被改。
#[test]
fn copying_into_a_file_is_refused_without_damaging_it() {
    let tmp = TmpDir::new("ds-destfile");
    let src = tmp.0.join("src");
    make_tree(&src);
    let dest_file = tmp.0.join("not-a-dir");
    std::fs::write(&dest_file, b"I AM A FILE").expect("w");
    let before = snapshot(&tmp.0);

    run_copy_move(&[src.join("a.txt")], &dest_file, false, ConflictPolicy::Overwrite);

    assert_identical(&before, &snapshot(&tmp.0));
}

// ───────────────────────────── C：移动不丢 ─────────────────────────────

/// 成功的移动：载荷必须**恰好出现在一处**——目标有、源没有。
/// 两头都有是复制不是移动；两头都没有就是数据丢了。
#[test]
fn a_successful_move_puts_the_payload_in_exactly_one_place() {
    let tmp = TmpDir::new("ds-move");
    let src = tmp.0.join("src");
    let dst = tmp.0.join("dst");
    make_tree(&src);
    std::fs::create_dir_all(&dst).expect("mkdir dst");

    run_copy_move(&[src.join("a.txt")], &dst, true, ConflictPolicy::Overwrite);

    let at_dst = std::fs::read(dst.join("a.txt")).ok();
    let at_src = std::fs::read(src.join("a.txt")).ok();
    assert_eq!(at_dst.as_deref(), Some(&b"AAA"[..]), "目标没拿到载荷");
    assert!(at_src.is_none(), "源还在——这是复制不是移动");
    // 其余文件不受影响
    assert_eq!(std::fs::read(src.join("b.txt")).expect("b"), b"BBB");
}

/// 跳过策略下的移动：既然跳过了，**源必须原封不动**，目标也不能被改。
/// 这里曾经踩过：前端在拖拽那一刻就乐观地把项从源目录列表里删了，跳过时若不刷新源目录，
/// 用户看到的就是「从源目录消失了、目标目录里也没有」——和数据丢失长得一模一样。
#[test]
fn a_skipped_move_leaves_both_sides_untouched() {
    let tmp = TmpDir::new("ds-skipmove");
    let src = tmp.0.join("src");
    let dst = tmp.0.join("dst");
    make_tree(&src);
    std::fs::create_dir_all(&dst).expect("mkdir dst");
    std::fs::write(dst.join("a.txt"), b"EXISTING").expect("w");
    let before = snapshot(&tmp.0);

    run_copy_move(&[src.join("a.txt")], &dst, true, ConflictPolicy::Skip);

    assert_identical(&before, &snapshot(&tmp.0));
}

// ───────────────────────────── D：不跟随符号链接越界 ─────────────────────────────

/// 删除一条符号链接，**不能**把它指向的文件删掉。
///
/// 这是最容易出事的一类：`is_dir()` 会跟随链接，一旦拿它判类型，指向目录的链接就会走
/// `remove_dir_all`，把链接目标那棵树整个删掉——而那棵树可能根本不在用户选中的范围里。
///
/// ⚠ 诚实说明：**今天这条是「钉子」而不是「门禁」**。做过反向对照——把 `symlink_metadata`
/// 换成会跟随链接的 `metadata`，这条测试**照样通过**，因为 Rust std 的 `remove_dir_all` /
/// `remove_file` 本身就是 lstat 语义（前者对符号链接直接报错、不肯递归进去）。也就是说
/// 当前实现是双保险。留着它是因为：一旦将来有人改成手写递归删除、或先 `canonicalize`
/// 再删，std 那层保护就没了，这条会立刻炸。别因为"它现在总是绿的"就删掉。
#[test]
fn deleting_a_symlink_never_touches_its_target() {
    let tmp = TmpDir::new("ds-symdel");
    let outside = tmp.0.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::write(outside.join("precious.txt"), b"PRECIOUS").expect("w");

    let work = tmp.0.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    let link_to_dir = work.join("link-dir");
    let link_to_file = work.join("link-file");
    std::os::unix::fs::symlink(&outside, &link_to_dir).expect("symlink dir");
    std::os::unix::fs::symlink(outside.join("precious.txt"), &link_to_file).expect("symlink file");

    let (sink, _rx) = test_sink();
    block_on(delete_many(
        vec![
            link_to_dir.to_string_lossy().into_owned(),
            link_to_file.to_string_lossy().into_owned(),
        ],
        &sink,
    ));

    assert!(!link_to_dir.exists() || std::fs::symlink_metadata(&link_to_dir).is_err());
    assert!(
        outside.join("precious.txt").exists(),
        "删链接把链接指向的文件也删了——数据在用户没选中的目录里丢了"
    );
    assert_eq!(
        std::fs::read(outside.join("precious.txt")).expect("目标还在"),
        b"PRECIOUS"
    );
    assert!(outside.is_dir(), "删指向目录的链接把整个目标目录删了");
}

/// 复制一条符号链接：复制的是**链接本身**，不能顺着它把目标内容写到别处，
/// 更不能改动目标。
#[test]
fn copying_a_symlink_does_not_write_through_to_its_target() {
    let tmp = TmpDir::new("ds-symcopy");
    let outside = tmp.0.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir");
    let target = outside.join("target.txt");
    std::fs::write(&target, b"ORIGINAL").expect("w");

    let work = tmp.0.join("work");
    let dst = tmp.0.join("dst");
    std::fs::create_dir_all(&work).expect("mkdir work");
    std::fs::create_dir_all(&dst).expect("mkdir dst");
    let link = work.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    run_copy_move(&[link], &dst, false, ConflictPolicy::Overwrite);

    assert_eq!(
        std::fs::read(&target).expect("目标还在"),
        b"ORIGINAL",
        "复制链接时把链接指向的文件改了"
    );
}

// ───────────────────────────── 既有守卫的回归 ─────────────────────────────

/// 复制粘贴回**原目录**：必须拒绝，且源文件一个字节都不能少。
/// （`fs::copy` 以 truncate 打开目标，`copy(src, src)` 会把源清成 0 字节。）
#[test]
fn pasting_into_its_own_directory_never_truncates_the_source() {
    let tmp = TmpDir::new("ds-self");
    make_tree(&tmp.0);
    let before = snapshot(&tmp.0);

    run_copy_move(&[tmp.0.join("a.txt")], &tmp.0, false, ConflictPolicy::Overwrite);

    assert_identical(&before, &snapshot(&tmp.0));
}

/// 把目录复制进它自己的子目录：拒绝，且不能在里面留下半棵递归拷出来的树。
#[test]
fn copying_a_directory_into_itself_leaves_no_debris() {
    let tmp = TmpDir::new("ds-nest");
    let proj = tmp.0.join("proj");
    make_tree(&proj);
    let before = snapshot(&tmp.0);

    run_copy_move(&[proj.clone()], &proj.join("sub"), false, ConflictPolicy::Overwrite);

    assert_identical(&before, &snapshot(&tmp.0));
}

// ───────────────────────────── E：只读操作不写盘 ─────────────────────────────

/// 列目录 / 读文件跑完，整棵树的快照必须一模一样。
/// 读路径悄悄改了什么（补 BOM、改行尾、truncate 打开……）在这里会立刻现形。
#[test]
fn read_only_operations_never_modify_anything() {
    let tmp = TmpDir::new("ds-ro");
    make_tree(&tmp.0);
    std::fs::write(tmp.0.join("crlf.txt"), b"line1\r\nline2\r\n").expect("w");
    std::fs::write(tmp.0.join("bin.dat"), [0u8, 1, 2, 255, 254]).expect("w");
    let before = snapshot(&tmp.0);

    let (sink, _rx) = test_sink();
    block_on(async {
        list_dir_event(&tmp.0.to_string_lossy(), 1, &sink).await;
        for f in ["a.txt", "crlf.txt", "bin.dat", "sub/inner.txt"] {
            let p = tmp.0.join(f);
            read_file(&p.to_string_lossy(), false, 1, &sink).await;
        }
    });

    assert_identical(&before, &snapshot(&tmp.0));
}

/// 保存失败（父目录只读）时，原文件必须原封不动——绝不能先截断再发现写不进去。
/// root 会无视权限位，那种环境下跳过。
#[test]
fn a_failed_save_leaves_the_original_file_intact() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过：以 root 运行时权限位不起作用");
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let tmp = TmpDir::new("ds-rosave");
    let dir = tmp.0.join("locked");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let target = dir.join("config.yaml");
    std::fs::write(&target, b"ORIGINAL CONTENT").expect("w");

    // 目录只读：临时文件建不出来 → 保存必须失败
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod ro");

    let (sink, _rx) = test_sink();
    block_on(write_file(
        1,
        &target.to_string_lossy(),
        "REPLACEMENT".to_string(),
        "UTF-8",
        Eol::Lf,
        0,
        true,
        &sink,
    ));

    // 先恢复权限，免得 TmpDir 清理不掉
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod rw");

    assert_eq!(
        std::fs::read(&target).expect("原文件必须还在"),
        b"ORIGINAL CONTENT",
        "保存失败却把原文件改了/清空了"
    );
}
