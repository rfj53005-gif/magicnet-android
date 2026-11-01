# 来源与修改声明 / Provenance and modifications

本仓库是**个人自用的修改版**，不是上游官方产物，与上游作者无隶属或背书关系。

## 上游

- `LIghtJUNction/magicnet-android` — <https://github.com/LIghtJUNction/magicnet-android>，MIT，© 2025 LIghtJUNction
- 核心子模块 `sing-box` 派生自 `SagerNet/sing-box`（GPL-3.0）

## 本仓库的修改

- Go 安装器移植为 Rust（`crates/magicnet-installer`、`crates/magicnet-components`），并修复下载重定向（`--location`）与 Go 风格参数解析（`--flag=value`）；
- WebUI 概览页重排（状态与内存同行、操作收成一行、星座去重）；
- `scripts/build-sing-box.sh` 增加 `android/arm64` 构建：NDK clang、CGO、Rust2 静态库、Android 标签去 `with_lxd`；
- `.gitmodules` 指向本账号的镜像仓，使干净克隆可解析固定提交。

## 许可

MagicNet 与模块部分沿用 **MIT**（见 `LICENSE`）；`sing-box` 子模块及其修改沿用 **GPL-3.0**（见 `sing-box/LICENSE`）。

## 免责

仅供个人使用，不提供任何担保；使用风险自负。
本仓库不包含订阅、节点凭据、个人配置或设备数据。
