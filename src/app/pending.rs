//! Session 上「待 UI 消费」的事件缓冲：从 Session 字段拆出，行为不变。
//! worker → Session.pending → App::process_frame_events 消费。

use crate::proto::Eol;

/// 单会话内、尚未被 App 帧循环取走的异步结果。
#[derive(Default)]
pub(super) struct SessionPending {
    /// 已读取待填充到占位编辑器标签的文件（id, path, content, encoding, eol, mtime）
    pub open: Vec<(u64, String, String, String, Eol, u32)>,
    /// 保存成功回报的新 mtime（请求 id, path, mtime）
    pub saved: Vec<(u64, String, u32)>,
    /// 保存写入进度（path, done, total）——驱动编辑器标签「珊瑚→绿」保存动画
    pub save_progress: Vec<(String, u64, u64)>,
    /// 跟随读取返回：(路径, 新增字节, 新 offset, 是否截断/轮转)
    pub tail: Vec<(String, Vec<u8>, u64, bool)>,
    /// PDF 页数查询返回：(占位标签 id, 页数)
    pub pdf_info: Vec<(u64, u32)>,
    /// PDF 单页 PNG 返回：(路径, 页码, PNG 字节)
    pub pdf_page: Vec<(String, u32, Vec<u8>)>,
    /// PDF 查找返回：(路径, 命中列表, 失败消息)
    pub pdf_search: Vec<(String, Vec<(u32, String)>, Option<String>)>,
    /// 文档原始字节返回：(占位标签 id, 字节)
    pub doc: Vec<(u64, Vec<u8>)>,
    /// 外部改动冲突（请求 id, path）
    pub conflict: Vec<(u64, String)>,
    /// 保存失败（网络/权限等）：(请求 id, 路径, 原因)
    pub save_failed: Vec<(u64, String, String)>,
    /// 需要在 App 层弹 toast 的警告（如编码丢字）
    pub warn: Vec<String>,
    /// 打开时发现实际超限（id, path, size）——移除占位标签 + 弹「打开大文件」确认
    pub too_large: Vec<(u64, String, u64)>,
    /// 待新建的占位编辑器标签（id, path）——双击打开时立即建
    pub placeholder: Vec<(u64, String)>,
    /// 文件下载进度（id, done, total），驱动占位标签进度条
    pub load_progress: Vec<(u64, u64, u64)>,
    /// 文件打开失败（id, message）——移除占位标签 + 提示
    pub load_fail: Vec<(u64, String)>,
    /// 已读取待打开到看图工具的图片（path, 原始字节）
    pub image: Vec<(String, Vec<u8>)>,
}

impl SessionPending {
    /// 是否还有未消费事件（用于测试/调试）。
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.open.is_empty()
            && self.saved.is_empty()
            && self.save_progress.is_empty()
            && self.tail.is_empty()
            && self.pdf_info.is_empty()
            && self.pdf_page.is_empty()
            && self.pdf_search.is_empty()
            && self.doc.is_empty()
            && self.conflict.is_empty()
            && self.save_failed.is_empty()
            && self.warn.is_empty()
            && self.too_large.is_empty()
            && self.placeholder.is_empty()
            && self.load_progress.is_empty()
            && self.load_fail.is_empty()
            && self.image.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::SessionPending;

    #[test]
    fn default_pending_is_empty() {
        let p = SessionPending::default();
        assert!(p.is_empty());
        let mut p = SessionPending::default();
        p.warn.push("x".into());
        assert!(!p.is_empty());
        p.warn.clear();
        assert!(p.is_empty());
    }
}
