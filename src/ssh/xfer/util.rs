//! 传输共用工具：路径、退避、tar 解压、冲突重命名。

use std::time::Duration;

use super::super::sftp::{is_sftp_not_found, join_remote};
use super::super::sh_quote;

pub(super) fn extract_tar_gz(path: &std::path::Path, dest: &std::path::Path) -> anyhow::Result<()> {
    let f = std::fs::File::open(path)?;
    let gz = flate2::read::GzDecoder::new(f);
    let mut ar = tar::Archive::new(gz);
    std::fs::create_dir_all(dest)?;
    let dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    for entry in ar.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        if !tar_entry_path_safe(&entry_path) {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh =>
                        format!("拒绝不安全的归档路径：{}", entry_path.display()),
                    crate::i18n::Lang::En =>
                        format!("Refusing unsafe archive path: {}", entry_path.display()),
                }
            );
        }
        // unpack_in 将相对路径落在 dest 下；返回 false 表示被跳过（含 ..）
        if !entry.unpack_in(&dest)? {
            anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh =>
                        format!("归档条目无法安全解压：{}", entry_path.display()),
                    crate::i18n::Lang::En => format!(
                        "Archive entry could not be unpacked safely: {}",
                        entry_path.display()
                    ),
                }
            );
        }
    }
    Ok(())
}

/// 归档条目路径是否可安全解压到目标目录内（相对路径、无 `..`、非绝对）。
pub(super) fn tar_entry_path_safe(p: &std::path::Path) -> bool {
    use std::path::Component;
    if p.as_os_str().is_empty() || p.is_absolute() {
        return false;
    }
    for c in p.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}
/// 下载（单文件或整个目录）并上报进度。大文件用多个并发分段读取流水线化，
/// 抵消 SFTP「单请求等一个往返」的吞吐瓶颈（高延迟链路上提速明显）。
/// 压缩下载一个目录：远端 tar.gz 打包到临时文件 → 单文件并发下载 → 本地解包。
/// 进度按压缩包字节上报。返回 Err 表示不支持/失败（上层回退到逐文件）。
pub(super) fn join_quoted(items: &[String]) -> String {
    let mut s = String::new();
    for p in items {
        s.push_str(&sh_quote(p));
        s.push(' ');
    }
    s
}

/// 直传临时私钥目录的清理守卫：无论正常返回、`?` 早退，还是被取消（future 被 drop），
/// Drop 时都异步清除源主机上的临时私钥目录——避免目标主机私钥残留在源主机（凭据泄露）。
/// 取消路径下本函数栈已被展开，无法 `.await`，故 detach 一个清理任务到当前运行时。
pub(crate) fn local_nonexistent(path: &str) -> String {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return path.to_string();
    }
    let is_dir = p.is_dir();
    let parent = p.parent();
    let fname = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (stem, ext) = split_name(&fname, is_dir);
    for n in 1..10000u32 {
        let cand_name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let cand = match parent {
            Some(d) => d.join(&cand_name),
            None => std::path::PathBuf::from(&cand_name),
        };
        if !cand.exists() {
            return cand.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

/// 给远端目录里的名字找一个不冲突的变体。
/// 找一个远端不存在的候选名（"name (1)"、"name (2)" ...）。metadata 探测遇到明确的
/// "不存在"（NoSuchFile）才当作候选可用；权限不足、网络超时、SFTP 会话异常等其它错误
/// 不能被当成"不存在"直接放行——那样可能把一个探测失败、但其实已经存在的候选名
/// 错误地当成安全目标，交给上层决定要不要重试/放弃（返回错误，而不是悄悄继续猜下一个）。
pub(in crate::ssh) async fn remote_nonexistent(
    sftp: &russh_sftp::client::SftpSession,
    dir: &str,
    name: &str,
    is_dir: bool,
) -> anyhow::Result<String> {
    let (stem, ext) = split_name(name, is_dir);
    for n in 1..10000u32 {
        let cand = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        match sftp.metadata(&join_remote(dir, &cand)).await {
            Ok(_) => continue, // 候选已存在，试下一个
            Err(e) if is_sftp_not_found(&e) => return Ok(cand),
            Err(e) => anyhow::bail!("探测远端候选名失败：{e}"),
        }
    }
    Ok(name.to_string())
}

/// 一项复制/移动在冲突策略下的归宿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ssh) enum CopyDest {
    /// 照常「移入目标目录」，即命令写成 `cp/mv 源 目标目录/`。
    ///
    /// **不能**为了省事改写成显式的 `目标目录/同名`：源是目录、且目标目录里已有同名目录时，
    /// `cp -a 源 目标/名` 的语义是「把源放进那个已存在的目录里面」（得到 `目标/名/名`），
    /// 而 `cp -a 源 目标/` 才是「合并进去」。两者差一个层级，是实打实的行为变化。
    Into,
    /// 换一个不冲突的新名字。此时目标必然不存在，写显式全路径没有上面那个歧义。
    Renamed(String),
    /// 跳过这一项（冲突策略为「跳过」）。
    Skip,
}

/// 按冲突策略给一批 复制/移动 定归宿。
///
/// 策略是「覆盖」时**直接返回、一次探测都不做**：那既是默认值也是原来的行为，多打 N 个
/// SFTP 往返只会让最常见的路径变慢，还平白多出「探测瞬时失败该怎么办」的分支。
///
/// 「跳过 / 重命名」才需要知道目标在不在。探测失败（既不是成功、也不是明确的
/// NoSuchFile）一律向上报错，**不退化成覆盖**：用户特意选了别覆盖，拿一次探测失败当
/// "那就覆盖吧"是把设置反过来执行。
pub(in crate::ssh) async fn plan_copy_move(
    sftp: &russh_sftp::client::SftpSession,
    srcs: &[String],
    dest_dir: &str,
    policy: crate::proto::ConflictPolicy,
) -> anyhow::Result<Vec<(String, CopyDest)>> {
    use crate::proto::ConflictPolicy as P;
    if matches!(policy, P::Overwrite) {
        return Ok(srcs.iter().map(|p| (p.clone(), CopyDest::Into)).collect());
    }
    let mut out = Vec::with_capacity(srcs.len());
    for src in srcs {
        let name = basename(src);
        let taken = match sftp.metadata(&join_remote(dest_dir, &name)).await {
            Ok(_) => true,
            Err(e) if is_sftp_not_found(&e) => false,
            Err(e) => anyhow::bail!(
                "{}",
                match crate::i18n::current() {
                    crate::i18n::Lang::Zh =>
                        format!("检查目标是否已存在失败（{name}）：{e}；已中止，未改动任何文件"),
                    crate::i18n::Lang::En => format!(
                        "failed to check whether the target exists ({name}): {e}; aborted, nothing changed"
                    ),
                }
            ),
        };
        if !taken {
            out.push((src.clone(), CopyDest::Into));
            continue;
        }
        match policy {
            P::Skip => out.push((src.clone(), CopyDest::Skip)),
            P::Rename => {
                // 源是不是目录，决定 split_name 要不要拆后缀（目录名里的点不是扩展名）
                let is_dir = sftp.metadata(src).await.map(|m| m.is_dir()).unwrap_or(false);
                let fresh = remote_nonexistent(sftp, dest_dir, &name, is_dir).await?;
                out.push((src.clone(), CopyDest::Renamed(fresh)));
            }
            P::Overwrite => unreachable!("上面已提前返回"),
        }
    }
    Ok(out)
}

/// 把归宿表拼成一条 shell 脚本；全部跳过时返回 None（调用方据此连命令都不用发）。
///
/// 用 `rc=0; … || rc=1; exit $rc` 而不是 `&&`/`;` 直接串：既要「有一项失败也把其余项做完」
/// （这是原来一条 `cp -a -- 全部源 目标/` 的行为），又要能从退出码看出整体成没成——`;` 串起来
/// 只剩最后一条的退出码，前面失败了会被报成成功。
pub(in crate::ssh) fn copy_move_script(
    plan: &[(String, CopyDest)],
    dest_dir: &str,
    do_move: bool,
) -> Option<String> {
    let tool = if do_move { "mv -f" } else { "cp -a" };
    let mut lines = String::from("rc=0");
    let mut any = false;
    for (src, dest) in plan {
        let target = match dest {
            CopyDest::Skip => continue,
            // 目标强制以 "/" 结尾，令 mv/cp 必须把源「移入目录」：目标不是已存在目录时报错，
            // 而不是把单个文件重命名成目标名——杜绝「拖到目录树后文件被改名、两个目录都找不到」。
            CopyDest::Into => format!("{}/", sh_quote(dest_dir)),
            CopyDest::Renamed(n) => sh_quote(&join_remote(dest_dir, n)),
        };
        any = true;
        // `--` 终止选项解析，避免以 - 开头的文件名被当作开关
        lines.push_str(&format!("\n{tool} -- {} {target} || rc=1", sh_quote(src)));
    }
    any.then(|| {
        lines.push_str("\nexit $rc");
        lines
    })
}

/// 拆分文件名为 (主名, 扩展)；目录或无扩展时扩展为 None（首字符的点不算扩展）。
pub(super) fn split_name(fname: &str, is_dir: bool) -> (String, Option<String>) {
    if is_dir {
        return (fname.to_string(), None);
    }
    match fname.rfind('.') {
        Some(d) if d > 0 => (fname[..d].to_string(), Some(fname[d + 1..].to_string())),
        _ => (fname.to_string(), None),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) const DL_PARALLEL: u64 = 8;
/// 每个分段一次抢占的字节数。
pub(super) const DL_CHUNK: u64 = 1024 * 1024;
/// 单个文件传输遇到瞬时错误时的最大额外重试次数（配合断点续传）。
pub(super) const XFER_RETRIES: u32 = 3;

/// 第 attempt 次重试前的退避时长（300ms·2^n，封顶约 4.8s）。
pub(super) fn xfer_backoff(attempt: u32) -> Duration {
    Duration::from_millis(300u64 * (1u64 << attempt.min(4)))
}

/// 断点信息 sidecar 路径：`<local>.ishellpart`。
pub(super) fn part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart");
    std::path::PathBuf::from(p)
}

/// 下载数据的临时文件路径：`<local>.ishellpart.data`。
/// 数据先写这里，全部完成后 rename 到目标——成功前绝不动目标文件；
/// 取消/失败只留 part 文件，目标（若原本存在）保持完好。
pub(super) fn data_part_path(lpath: &std::path::Path) -> std::path::PathBuf {
    let mut p = lpath.as_os_str().to_os_string();
    p.push(".ishellpart.data");
    std::path::PathBuf::from(p)
}

/// 容纳 n 个分段标志位所需的字节数。
pub(super) fn bitmap_len(n_chunks: u64) -> usize {
    n_chunks.div_ceil(8) as usize
}

/// 下载单个文件：大文件按偏移并发分段读取，定位写入本地，显著提升高延迟链路吞吐。
/// 数据全程写 `<local>.ishellpart.data`，完整后原子 rename 到目标——成功前不动目标文件。
/// `remote_mtime` 参与断点校验（0 = 不允许跨次续传，如临时打包文件）。
pub(super) fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

/// 取「本地」路径的文件名：同时按 `/` 和 `\` 切分，正确处理 Windows 路径
/// （否则 `C:\Users\x\a.txt` 会被当成整体文件名上传，远端文件名也带上盘符路径）。
pub(super) fn local_basename(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_string()
}

/// 把解压出来的目录 `extracted` 移到 `target`，做**镜像覆盖**（目标最终只含这次传输的内容，
/// 不残留旧目录里源端已删除的文件），并保证不管压缩包顶层目录名与调用方要求的目标名是否一致，
/// 最终都落在 `target` 这个确切路径上。
///
/// **不先删旧目标再换新的**：那样一旦「删成功、换新失败」就两头皆空、旧数据不可恢复。改为
/// 先把旧目标 rename 到同目录的备份名（原子、可回滚），换上新目录成功后才删备份，失败则把
/// 备份换回——「成功前不破坏原目标」。备份与目标同目录、故同文件系统内 rename，不涉及跨盘拷贝。
pub(super) fn place_extracted_dir(extracted: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    // 目标不存在：直接换上即可。
    if std::fs::symlink_metadata(target).is_err() {
        return std::fs::rename(extracted, target);
    }
    // 目标存在：旧目标 → 备份（原子）。
    let mut backup = target.as_os_str().to_os_string();
    backup.push(format!(".ishelldl-bak-{}", super::rand_hex(6)));
    let backup = std::path::PathBuf::from(backup);
    std::fs::rename(target, &backup)?;
    match std::fs::rename(extracted, target) {
        Ok(()) => {
            // 换上成功：清理备份（残留无害，失败忽略）。
            let is_dir = std::fs::symlink_metadata(&backup)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            let _ = if is_dir {
                std::fs::remove_dir_all(&backup)
            } else {
                std::fs::remove_file(&backup)
            };
            Ok(())
        }
        Err(e) => {
            // 换上失败：把备份换回原位，尽量恢复原状后再上报错误。
            let _ = std::fs::rename(&backup, target);
            Err(e)
        }
    }
}

#[cfg(test)]
mod copy_move_script_tests {
    use super::{copy_move_script, CopyDest};

    fn plan(items: &[(&str, CopyDest)]) -> Vec<(String, CopyDest)> {
        items.iter().map(|(s, d)| (s.to_string(), d.clone())).collect()
    }

    /// **层级守门人**：不冲突/覆盖的那一项必须写成 `目标目录/`，**不能**写成显式的
    /// `目标目录/同名`。
    ///
    /// 源是目录、且目标目录里已有同名目录时：`cp -a 源 目标/` 是「合并进去」，
    /// 而 `cp -a 源 目标/名` 是「放进那个已存在的目录里面」，会多出一层 `目标/名/名`。
    /// 为了「顺手把目标写全」而改掉这里，就是一次静默的行为变化。
    #[test]
    fn non_conflicting_items_keep_the_trailing_slash_form() {
        let cmd = copy_move_script(
            &plan(&[("/a/proj", CopyDest::Into)]),
            "/b",
            false,
        )
        .expect("有活要干");
        assert!(cmd.contains("cp -a -- '/a/proj' '/b'/"), "应当是「移入目录」形式：\n{cmd}");
        assert!(!cmd.contains("'/b/proj'"), "不得写成显式的目标全路径：\n{cmd}");
    }

    /// 重命名的那一项目标必然不存在，写显式全路径没有歧义。
    #[test]
    fn renamed_items_use_the_explicit_full_path() {
        let cmd = copy_move_script(
            &plan(&[("/a/r.pdf", CopyDest::Renamed("r (1).pdf".into()))]),
            "/b",
            false,
        )
        .expect("有活要干");
        assert!(cmd.contains("'/b/r (1).pdf'"), "重命名应落到显式全路径：\n{cmd}");
    }

    /// 一项失败不能让其余项不做（原来一条 `cp -a -- 全部源 目标/` 就是这个行为），
    /// 但整体退出码要能反映"有东西失败了"——`;` 直接串只剩最后一条的退出码。
    #[test]
    fn one_failure_does_not_stop_the_rest_but_still_shows_up_in_the_exit_code() {
        let cmd = copy_move_script(
            &plan(&[("/a/x", CopyDest::Into), ("/a/y", CopyDest::Into)]),
            "/b",
            false,
        )
        .expect("有活要干");
        assert!(cmd.starts_with("rc=0"), "缺少 rc 初始化：\n{cmd}");
        assert_eq!(cmd.matches("|| rc=1").count(), 2, "每一项都要记失败：\n{cmd}");
        assert!(cmd.ends_with("exit $rc"), "缺少整体退出码：\n{cmd}");
        assert!(!cmd.contains("&&"), "不得用 && 串联（一项失败就不做后面的了）：\n{cmd}");
    }

    /// 跳过的项一条命令都不该出现；全部跳过时连脚本都不用发。
    #[test]
    fn skipped_items_emit_nothing() {
        let cmd = copy_move_script(
            &plan(&[("/a/x", CopyDest::Skip), ("/a/y", CopyDest::Into)]),
            "/b",
            false,
        )
        .expect("还有一项要干");
        assert!(!cmd.contains("/a/x"), "跳过项不该进命令：\n{cmd}");
        assert!(cmd.contains("/a/y"));
        assert!(
            copy_move_script(&plan(&[("/a/x", CopyDest::Skip)]), "/b", false).is_none(),
            "全部跳过时不该产出任何命令"
        );
    }

    /// 移动走 `mv -f`，复制走 `cp -a`；两者都带 `--` 终止选项解析。
    #[test]
    fn move_and_copy_use_their_own_tool_and_terminate_options() {
        let mv = copy_move_script(&plan(&[("/a/-weird", CopyDest::Into)]), "/b", true)
            .expect("有活要干");
        assert!(mv.contains("mv -f -- '/a/-weird'"), "{mv}");
        let cp = copy_move_script(&plan(&[("/a/-weird", CopyDest::Into)]), "/b", false)
            .expect("有活要干");
        assert!(cp.contains("cp -a -- '/a/-weird'"), "{cp}");
    }

    /// 带引号/空格的路径要靠 sh_quote escape，不能破坏命令结构。
    #[test]
    fn paths_are_shell_quoted() {
        let cmd = copy_move_script(
            &plan(&[("/a/it's here.txt", CopyDest::Into)]),
            "/b/my dir",
            false,
        )
        .expect("有活要干");
        assert!(cmd.contains(r#"'/a/it'\''s here.txt'"#), "{cmd}");
        assert!(cmd.contains("'/b/my dir'/"), "{cmd}");
    }
}

#[cfg(test)]
mod tests {
    use super::{place_extracted_dir, tar_entry_path_safe};
    use std::path::Path;

    #[test]
    fn tar_paths_reject_traversal() {
        assert!(tar_entry_path_safe(Path::new("ok/file.txt")));
        assert!(tar_entry_path_safe(Path::new("./nested/a")));
        assert!(!tar_entry_path_safe(Path::new("../escape")));
        assert!(!tar_entry_path_safe(Path::new("a/../../b")));
        assert!(!tar_entry_path_safe(Path::new("/abs/path")));
        assert!(!tar_entry_path_safe(Path::new("")));
    }

    #[test]
    fn place_extracted_dir_renames_into_place_when_target_absent() {
        let tmp = std::env::temp_dir().join(format!("ishell-test-place-{}", rand_suffix()));
        std::fs::create_dir_all(&tmp).unwrap();
        let extracted = tmp.join("old_name");
        std::fs::create_dir_all(&extracted).unwrap();
        std::fs::write(extracted.join("a.txt"), b"hello").unwrap();
        let target = tmp.join("new_name");

        place_extracted_dir(&extracted, &target).unwrap();

        assert!(!extracted.exists());
        assert!(target.join("a.txt").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn place_extracted_dir_mirrors_over_existing_target_dropping_stale_files() {
        let tmp = std::env::temp_dir().join(format!("ishell-test-place-{}", rand_suffix()));
        std::fs::create_dir_all(&tmp).unwrap();
        let extracted = tmp.join("old_name");
        std::fs::create_dir_all(&extracted).unwrap();
        std::fs::write(extracted.join("fresh.txt"), b"fresh").unwrap();
        let target = tmp.join("new_name");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("stale.txt"), b"stale").unwrap();

        place_extracted_dir(&extracted, &target).unwrap();

        // 镜像覆盖：旧目录里源端已不存在的文件不应该被保留下来。
        assert!(!target.join("stale.txt").exists());
        assert!(target.join("fresh.txt").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn place_extracted_dir_replaces_existing_target_file() {
        let tmp = std::env::temp_dir().join(format!("ishell-test-place-{}", rand_suffix()));
        std::fs::create_dir_all(&tmp).unwrap();
        let extracted = tmp.join("old_name");
        std::fs::create_dir_all(&extracted).unwrap();
        let target = tmp.join("new_name");
        std::fs::write(&target, b"i used to be a file").unwrap();

        place_extracted_dir(&extracted, &target).unwrap();

        assert!(target.is_dir());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        (std::process::id() as u64) ^ CTR.fetch_add(1, Ordering::Relaxed)
    }
}
