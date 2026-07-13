# Changelog

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)。

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
