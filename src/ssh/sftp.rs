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
