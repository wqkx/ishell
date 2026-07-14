This directory vendors `vt100` 0.16.2 from crates.io.

iShell carries one behavioral patch in `src/grid.rs`: rows removed by a
top-anchored DECSTBM scrolling region are retained in scrollback. Upstream
0.16.2 discards rows whenever any scrolling region is active, which breaks
inline-history TUIs such as Codex. Regions starting below the first terminal
row remain excluded from scrollback.
