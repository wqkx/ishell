//! SSH SFTP 文件操作：从 ssh God Object 拆出，行为不变。

#[path = "sftp_list.rs"]
mod list;
#[path = "sftp_read.rs"]
mod read;
#[path = "sftp_write.rs"]
mod write;

pub(super) use list::list_dir;
pub(super) use read::{read_file_chunked, read_image_file, tail_file};
pub(super) use write::{handle_fs_op, sftp_overwrite};

/// 判断一次 SFTP 操作失败是不是明确的"文件不存在"（NoSuchFile 状态码），而不是权限不足、
/// 网络超时、SFTP 会话异常等其它错误——这几种情况都不代表目标真的不存在，一律当成
/// "不存在"会做出错误的判断（比如把一个探测失败、但其实已经存在的文件误判为可安全覆盖的
/// 新建目标）。共享给 `xfer`（上传冲突改名）和 `sftp::write`（保存时的备份步骤）用。
/// 构造一个**只带权限位**的 SETSTAT 属性集。
///
/// 千万别写成 `FileAttributes { permissions: Some(mode), ..Default::default() }`——那句话看起来
/// 是「只设权限」，实际不是。russh-sftp 的 `FileAttributes::default()` **不是**全 `None`：
///
/// ```text
/// size: Some(0), uid: Some(0), gid: Some(0), atime: Some(0), mtime: Some(0),
/// permissions: Some(0o777 | DIR)
/// ```
///
/// 而它的 `Serialize` 是「哪个字段是 Some 就点亮哪个标志位」。于是 `..Default::default()` 发出去的
/// 是一条 **SIZE|UIDGID|PERMISSIONS|ACMODTIME 全开** 的 SETSTAT——按 RFC 语义，服务端会
/// 依次执行 `truncate(path, 0)`、`chmod`、`utimes`、`chown(path, 0, 0)`：
///
/// - `truncate` 排在最前且必然成功 → **文件当场被清成 0 字节**；
/// - `chown` 到 root 对普通用户必然 EPERM → 整条请求回 `PermissionDenied`。
///
/// 也就是说：一次「只想改个权限」的调用，实际后果是**文件被清空、然后报一个权限错误**。
/// 这正是 memory `save-empty-file-sftp-verify` 里记的「保存把文件写空」的真正根因——当时诊断成
/// 「个别服务器在 SETSTAT 时会截断文件」，其实是我们自己每次都在请求截断，任何守规矩的服务器
/// 都会照做。别把这个函数换回结构体字面量。
pub(super) fn perm_only_attrs(mode: u32) -> russh_sftp::protocol::FileAttributes {
    russh_sftp::protocol::FileAttributes {
        size: None,
        uid: None,
        user: None,
        gid: None,
        group: None,
        permissions: Some(mode),
        atime: None,
        mtime: None,
    }
}

pub(super) fn is_sftp_not_found(e: &russh_sftp::client::error::Error) -> bool {
    matches!(
        e,
        russh_sftp::client::error::Error::Status(s)
            if s.status_code == russh_sftp::protocol::StatusCode::NoSuchFile
    )
}

pub(super) fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

pub(super) fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// 递归创建远端目录（`mkdir -p` 语义）：SFTP 没有原生的递归建目录，只能把绝对路径按 `/`
/// 分段、从根往下逐级 `create_dir`。已存在的层会让 `create_dir` 报错，这里一律忽略——真正
/// 的权限/占位（同名文件挡路）等问题会在随后打开/写入文件那步以清晰的错误暴露，不必在这里
/// 抢先判定。best-effort 语义：本函数只负责"尽量把目录建出来"，成败由后续写操作定夺。
pub(super) async fn create_remote_dir_all(
    sftp: &russh_sftp::client::SftpSession,
    dir: &str,
) {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return; // 根目录 "/"：无需创建
    }
    let mut cur = String::new();
    for seg in dir.split('/') {
        if seg.is_empty() {
            continue; // 跳过前导 '/' 切出的空段
        }
        cur.push('/');
        cur.push_str(seg);
        let _ = sftp.create_dir(cur.clone()).await;
    }
}

#[cfg(test)]
mod path_tests {
    use super::{join_remote, remote_parent};

    /// `join_remote` 是全 App 拼远端路径的唯一入口（列目录、上传、下载、冲突探测、
    /// 事务写的 .part/.bak 全走它）。它没测过——这里补上，重点是**不能拼出双斜杠**：
    /// `//a` 和 `/a` 在多数服务器上等价、但不是所有；而 `remote_parent` 又要按 `/` 反解，
    /// 拼出来的形状不规范会一路歪下去。
    #[test]
    fn join_remote_never_doubles_the_separator() {
        assert_eq!(join_remote("/home/u", "a.txt"), "/home/u/a.txt");
        // 根目录：dir 本身就以 / 结尾，不能再加一个
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
        // 尾随斜杠（面包屑/路径栏很容易带出来）
        assert_eq!(join_remote("/home/u/", "a.txt"), "/home/u/a.txt");
        // 相对路径（会话刚连上、家目录还没解析出来时 cwd 可能是 "."）
        assert_eq!(join_remote(".", "a.txt"), "./a.txt");
    }

    /// 名字里的空格/引号/中文原样保留——转义是 `sh_quote` 的职责，不是这里的。
    /// 这条钉住"别顺手在这里做转义"，两边都做会转义两次。
    #[test]
    fn join_remote_does_not_escape_anything() {
        assert_eq!(join_remote("/d", "a b.txt"), "/d/a b.txt");
        assert_eq!(join_remote("/d", "it's.txt"), "/d/it's.txt");
        assert_eq!(join_remote("/d", "报告 (1).pdf"), "/d/报告 (1).pdf");
    }

    /// `remote_parent` 的返回值会被当作「要刷新的目录」发回 UI，也会被当作
    /// mkdir/cp 的目标目录用。根目录附近最容易出错：`/a` 的父级是 `/`（不是空串，
    /// 空串会让后续 join_remote 拼出 `/a` 这种把根丢掉的路径）。
    #[test]
    fn remote_parent_bottoms_out_at_root() {
        assert_eq!(remote_parent("/home/u/a.txt"), "/home/u");
        assert_eq!(remote_parent("/a"), "/");
        assert_eq!(remote_parent("/"), "/");
        // 无斜杠（相对名）：也退到根，绝不返回空串
        assert_eq!(remote_parent("a.txt"), "/");
        assert_eq!(remote_parent(""), "/");
    }

    /// 目录路径常带尾随斜杠（树节点、面包屑），父级要按「去掉尾斜杠后」算，
    /// 否则 `/home/u/sub/` 的父级会被算成它自己。
    #[test]
    fn remote_parent_ignores_trailing_slashes() {
        assert_eq!(remote_parent("/home/u/sub/"), "/home/u");
        assert_eq!(remote_parent("/home/u/sub///"), "/home/u");
        assert_eq!(remote_parent("/a/"), "/");
    }

    /// join 之后再取 parent，必须回到原来的目录——这两个函数在传输路径上是成对使用的
    /// （拼出目标路径 → 失败时取父目录刷新），对不上就会刷新错目录。
    #[test]
    fn join_then_parent_round_trips() {
        for dir in ["/", "/home", "/home/u", "/home/u/深 目录"] {
            let joined = join_remote(dir, "x.txt");
            let back = remote_parent(&joined);
            let expect = if dir == "/" { "/" } else { dir };
            assert_eq!(back, expect, "join_remote({dir:?}, x.txt) = {joined:?} 的父级算错了");
        }
    }
}
