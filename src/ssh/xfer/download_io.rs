//! Download file I/O helpers.

/// 在指定偏移定位写入（跨平台）。
pub(super) fn pwrite(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut off = offset;
        let mut b = buf;
        while !b.is_empty() {
            let n = file.seek_write(b, off)?;
            b = &b[n..];
            off += n as u64;
        }
        Ok(())
    }
}
