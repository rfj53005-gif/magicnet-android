# 第三方组件与许可 / Third-party components

本模块是 **MagicNet 的非官方自用构建**（来源与修改见仓库根 `NOTICE.md`），与上游作者无隶属或背书关系。

| 组件 | 许可 | 来源 |
|---|---|---|
| sing-box（`bin/sing-box` 核心） | **GPL-3.0** | <https://github.com/SagerNet/sing-box>；本构建对应源码 <https://github.com/rfj53005-gif/magicnet-core>（提交号见发布说明） |
| MagicNet 模块与服务（`lib/`、`cli`、`webroot/`） | MIT（© 2025 LIghtJUNction） | <https://github.com/LIghtJUNction/magicnet-android> |
| kamfw / KamHooks / KamModuleX | 见 `lib/kamfw/LICENSE` | <https://github.com/MemDeco-WG> |
| jq / yq | MIT | <https://github.com/jqlang/jq>、<https://github.com/mikefarah/yq> |
| curl | curl license | <https://curl.se/legal.html> |
| eCapture | Apache-2.0 | <https://github.com/gojue/ecapture> |
| zashboard | MIT | <https://github.com/Zephyruso/zashboard> |
| 规则集（geoip / geosite 衍生物） | 各自上游许可 | metacubex / Loyalsoldier / hagezi 等上游仓库 |

sing-box 的 GPL-3.0 全文见本包内 `notices/GPL-3.0.txt`（版权声明见 `notices/sing-box-LICENSE`）。本构建的完整对应源码、构建脚本与固定依赖均公开在上述仓库，满足 GPL 的源码提供义务。

本包仅供个人使用，不提供任何担保。
