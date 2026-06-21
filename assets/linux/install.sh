#!/bin/sh
# iShell Linux 安装脚本：把二进制、图标、桌面项装到用户目录（XDG），
# 装完即可在应用菜单/启动器里看到带 logo 的 iShell。无需 root。
#   用法：./install.sh           安装到 ~/.local
#         PREFIX=/usr/local sudo ./install.sh   安装到系统级
#         ./install.sh uninstall  卸载
set -e

DIR="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin"
ICONS="$PREFIX/share/icons/hicolor/256x256/apps"
APPS="$PREFIX/share/applications"

refresh() {
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -f "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
}

if [ "$1" = "uninstall" ]; then
    rm -f "$BIN/ishell" "$ICONS/ishell.png" "$APPS/iShell.desktop"
    refresh
    echo "已卸载 iShell。"
    exit 0
fi

mkdir -p "$BIN" "$ICONS" "$APPS"
install -m 755 "$DIR/ishell" "$BIN/ishell"
install -m 644 "$DIR/ishell.png" "$ICONS/ishell.png"
# 桌面项里写入二进制的绝对路径（菜单启动器不一定继承 PATH）
sed "s|@EXEC@|$BIN/ishell|g" "$DIR/iShell.desktop" > "$APPS/iShell.desktop"
chmod 644 "$APPS/iShell.desktop"
refresh

echo "已安装："
echo "  二进制 → $BIN/ishell"
echo "  图标   → $ICONS/ishell.png"
echo "  桌面项 → $APPS/iShell.desktop"
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "提示：$BIN 不在 PATH 中，命令行调用 ishell 需将其加入 PATH（菜单启动不受影响）。" ;;
esac
echo "若菜单未立即刷新，注销重登一次即可。"
