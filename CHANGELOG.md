# Changelog

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)。

## [0.15.1] - 2026-07-13

### Fixed
- 窗口长时间最小化后内存暴涨：系统信息快照原先挤在无界 mpsc 队列里，UI 不排空
  （最小化时 egui 基本停止调用 update）就一直堆积；改为 watch 通道只保留最新一份，
  内存占用与最小化时长无关，恢复窗口也不再有「积压爆发式重绘」
- 终端拖拽选区起点漏选按下字符；滚动条边缘拖入文本区时选区路由误判
- 标签栏拖拽重排时起始瞬间的位置跳动（抓取偏移改用真实按下位置）

## [0.15.0] - 2026-07-10

### Added
- 终端真彩色（24-bit RGB）与 Unicode 宽字符渲染
- 大文件打开阈值与只读提示；路径面包屑导航
- 发布流程：`scripts/bump-version.sh`、`RELEASE.md`、CI 版本一致性校验

### Fixed
- 编辑器关闭标签时清理 egui `TextEditState`，修复大文件关闭后内存滞留
- 关闭非当前标签时 `active` 索引错位（编辑器与看图工具）
- 保存并关闭、加载失败等路径统一走 `remove_tab_at`

### Changed
- 大规模模块拆分：所有 `src/**/*.rs` 控制在约 600 行以内
- CI clippy 策略收紧；`rustfmt` 全仓格式化

## [0.14.4] - 2026-07-05

数据安全与评审整改（保存校验、剪贴板、路径框等）。详见 GitHub Release。

[0.15.0]: https://github.com/wqkx/ishell/releases/tag/v0.15.0
[0.14.4]: https://github.com/wqkx/ishell/releases/tag/v0.14.4
