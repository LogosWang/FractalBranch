# 分形树枝（FractalBranch）

Windows 桌面分类整理与分区小工具，用来替代小智桌面的分区功能：轻量、启动快、稳定，能继承和导出现有分类结构。

- 规划和验收标准：[docs/PLAN.md](docs/PLAN.md)
- 技术验证记录：[docs/phase0-results.md](docs/phase0-results.md)

## 构建和安装

需要 Rust 工具链（`stable-x86_64-pc-windows-gnu`）。

```sh
cargo build --release -p fractal-branch
cargo test -p fractal-branch
```

## 打包

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

生成单文件安装包 `dist\FractalBranch-Setup-<版本>.exe`。安装包就是程序本身，文件名带 `Setup` 或“安装”时走安装流程：
复制到 `%LOCALAPPDATA%\Programs\FractalBranch`，登记开机自启、开始菜单快捷方式和系统的卸载入口，不需要管理员权限。

图标由 `scripts/make_icon.py` 生成到 `assets/app.ico`，构建时用 `windres` 编进 exe。

## 文件

- 程序数据：`%LOCALAPPDATA%\FractalBranch`（`layout.json`、备份、图标缓存、日志）
- 首次运行时，如果发现小智桌面的数据，会只读导入它的分类结构。

## 命令行

- `--install`、`--uninstall`：安装、卸载，加 `--quiet` 不弹对话框
- `--run`：文件名带 Setup 时也按普通程序运行
- `--snapshot 路径.png`：把当前布局画成图片，不创建窗口、不影响桌面
- `--autostart`：开机自启时使用，小智还在运行时静默退出

环境变量 `FRACTALBRANCH_DATA` 可以把数据目录指向别处。`spikes/desktop-mount/selftest.ps1` 用它在临时数据上驱动真实窗口和菜单做自测。

## 源码结构（`app/src`）

| 文件 | 内容 |
|---|---|
| `layout.rs` | 布局模型、和磁盘对账、读写 |
| `xz.rs` | 小智桌面数据导入 |
| `desktop.rs` | 桌面窗口树、原生图标层、崩溃守护 |
| `gfx.rs` | WARP + DirectComposition 渲染 |
| `icons.rs` | 后台取图标、磁盘缓存 |
| `watch.rs` | 桌面目录监视 |
| `view.rs` | 几何、绘制、点击判定 |
| `shell.rs` | 打开、右键菜单、拖放、重命名 |
| `app.rs` | 窗口、交互、托盘、菜单 |
| `install.rs` | 一键安装和卸载 |
| `snapshot.rs` | 离屏预览 |
