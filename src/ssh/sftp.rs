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
