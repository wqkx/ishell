#!/usr/bin/env bash
# 截图优化脚本：把英文界面原图压缩 + 重命名到 docs/screenshots/ 下的固定文件名。
#
# 用法：
#   1) 把原图放到 docs/screenshots/raw/，文件名（不含扩展名）用下面的标准名之一：
#        hero  files  conn  fwd  gpu  proc  edit          （扩展名 .png/.jpg/.jpeg 均可）
#   2) 在仓库根目录执行：  bash docs/optimize_screenshots.sh
#   3) 检查 docs/screenshots/*.png 无误后 git add/commit。
#
# 自动探测可用工具（magick > convert > 仅 pngquant > 仅复制），统一：
#   - 限制最大宽度 1600px（超大截图等比缩小，README 里更清爽、体积更小）
#   - 去除元数据
#   - PNG 有损量化压缩（pngquant，若安装）
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/docs/screenshots"
RAW="$OUT/raw"
MAXW=1600
NAMES=(hero files conn fwd gpu proc edit)

have() { command -v "$1" >/dev/null 2>&1; }

if [[ ! -d "$RAW" ]]; then
  echo "未找到 $RAW —— 已创建，请把原图放进去（文件名用：${NAMES[*]}），再重跑本脚本。"
  mkdir -p "$RAW"
  exit 0
fi

# 选定缩放工具
RESIZER=""
if have magick; then RESIZER="magick"
elif have convert; then RESIZER="convert"
fi
have pngquant && QUANT=1 || QUANT=0

[[ -z "$RESIZER" ]] && echo "提示：未装 ImageMagick，跳过等比缩放（仅压缩/复制）。装一下更好：apt install imagemagick"
[[ "$QUANT" == 0 ]] && echo "提示：未装 pngquant，跳过有损压缩（体积偏大）。装一下更好：apt install pngquant"

shopt -s nullglob
done_any=0
for name in "${NAMES[@]}"; do
  src=""
  for ext in png jpg jpeg PNG JPG JPEG; do
    [[ -f "$RAW/$name.$ext" ]] && { src="$RAW/$name.$ext"; break; }
  done
  [[ -z "$src" ]] && continue

  dst="$OUT/$name.png"
  tmp="$OUT/.$name.tmp.png"

  if [[ -n "$RESIZER" ]]; then
    "$RESIZER" "$src" -auto-orient -strip -resize "${MAXW}x>" "$tmp"
  else
    cp "$src" "$tmp"
  fi

  if [[ "$QUANT" == 1 ]]; then
    pngquant --force --skip-if-larger --quality=70-92 --output "$dst" "$tmp" 2>/dev/null || mv "$tmp" "$dst"
    rm -f "$tmp"
  else
    mv "$tmp" "$dst"
  fi

  printf '  ✓ %-6s -> %s  (%s)\n' "$name" "docs/screenshots/$name.png" "$(du -h "$dst" | cut -f1)"
  done_any=1
done

[[ "$done_any" == 0 ]] && echo "raw/ 里没找到任何标准名的图（${NAMES[*]}）。"
echo "完成。检查 docs/screenshots/ 后提交即可。"
