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
