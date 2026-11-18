# OXidea 鸿蒙 PC 移植方案

> 状态：调研中（分支 `feat/harmonyos`，从 main `5d3590f9` 切出）。
> 定位：把 OXidea（Tauri 2 桌面 IDE）移植到**鸿蒙 PC**（HarmonyOS 5/6 桌面版）。
> 原则：**先验证平台能力，再动手**；最大化复用现有前端与 Rust 后端，不造新轮子。
> 可行性详情见知识库 `oxidea-harmonyos-port-feasibility`。

## 〇、结论速览

- **可行移植项目，不是「加个编译 target」**。核心障碍 = Tauri/wry 没有鸿蒙 WebView 后端。
- 鸿蒙 PC 是桌面形态，IDE 产品形态匹配；Java 有鸿蒙生态基础（但需确认鸿蒙 PC 可用 JDK）。
- **推荐路径 B**：鸿蒙 ArkTS 壳 + ArkWeb 装现有前端 + Rust 编 OHOS `.so` + NAPI 桥替代 Tauri IPC。

## 一、已证实事实（来源：en.wikipedia.org/wiki/HarmonyOS）

| 事实 | 内容 |
|---|---|
| 鸿蒙 PC 存在 | HarmonyOS 5.0.1 于 2025-05-19 随 MateBook Pro / Fold PC 上线；HarmonyOS 6 于 2025-10 发布 |
| NEXT 纯血 | HarmonyOS NEXT（=5/6）去 AOSP，**不兼容 Android APK**；旧鸿蒙 1~4.3 才双框架可跑安卓 |
| 开发模型 | DevEco Studio（基于 IntelliJ IDEA）+ ArkTS/ArkUI + 方舟编译器 Ark Compiler |
| Rust target | rustup 有 OHOS tier-3 target：`aarch64-unknown-linux-ohos` / `armv7-…` / `x86_64-…` |

## 二、核心障碍

**Tauri / wry 无鸿蒙 WebView 后端。** Tauri 2 官方平台为 Win/mac/Linux/iOS/Android，wry 的
webview 绑定（WebView2 / WKWebView / WebKitGTK / Android WebView）里没有鸿蒙 ArkWeb。
壳起不来，整个 Tauri 应用就起不来——这是必须做的工程，但可逾越。

## 三、两条路径

### 路径 A：给 wry 加鸿蒙后端（上游级）
- 用 ArkWeb 实现 wry 的 webview trait；Tauri IPC 改走 NAPI。
- 做成后 OXidea 基本按 Tauri 流程编鸿蒙包；工作量大，社区受益。

### 路径 B：ArkTS 壳 + Rust native 库（应用级，推荐）
```
鸿蒙 ArkTS 壳应用 (HAP)
  └─ ArkWeb 组件 ── 加载现有前端 dist/（HTML/JS 基本不动）
       └─ window.__TAURI__.invoke ── shim ──> NAPI 桥
                                              └─ Rust 后端 .so（aarch64-unknown-linux-ohos）
                                                   ├─ ide-core / ide-run / ide-lsp / ide-host …
                                                   └─ spawn jdtls（需鸿蒙 PC JDK）
```
1. 前端塞进 ArkWeb；
2. Rust 后端编 OHOS target 成 `.so`；
3. NAPI 搭 ArkTS↔Rust 桥，替代 Tauri IPC；
4. 前端给 `window.__TAURI__.invoke` 写 shim 映射 NAPI——**前端业务代码大部分复用**。

## 四、阶段计划（每阶段有可检验完成标准）

| 阶段 | 目标 | 完成标准 |
|---|---|---|
| **P0 前提验证（gate）** | 落实第五节三项待验证 | 三项均有华为文档/实测结论；若存在 Linux 兼容层则改走最优路径 |
| **P1 工具链就绪** 🟡 大部分就绪 | OHOS SDK/NDK + DevEco + Rust OHOS target + 鸿蒙 PC JDK | DevEco 26 + NDK API 26 + Rust target 已备（见六）；剩鸿蒙 PC JDK |
| **P2 Rust 后端编译** ✅ 完成 | 纯 Rust + C 依赖 crate 编 OHOS + link | ide-core/ide-run check 通过；ide-ohos link 成 AArch64 `.so`，探针符号导出 |
| **P3 壳 + 桥** 🟡 起点已建 | ArkTS 壳 + ArkWeb + NAPI + invoke shim | ide-ohos cdylib 桥接 crate 已建（链接探针）；剩真 NAPI 桥 + ArkTS 壳 |
| **P4 语言服务** | jdtls/JDK 集成 + 进程/文件权限适配 | 鸿蒙 PC 上 spawn jdtls 并完成一次 Java 跳转 |
| **P5 打包真机** | HAP 打包 + 真机验证 | 鸿蒙 PC 真机安装运行，核心编辑/导航/运行可用 |

## 五、待验证（P0 gate，需华为开发者文档）

1. **进程/文件权限**：鸿蒙 PC 原生应用能否自由 spawn 子进程、访问任意目录（jdtls/终端/Git 的命脉）。
2. **鸿蒙 PC JDK**：是否有现成可跑在鸿蒙 PC 的 JDK（jdtls 需 JDK 21+）。毕昇 JDK 主面向鲲鹏 ARM + openEuler Linux，非现成鸿蒙 PC JDK。
3. **兼容层**：鸿蒙 PC 有无 Linux/Windows 应用兼容层——**若有，现有 Linux 包可能低成本直接跑，是最优路径**，可跳过 P1~P5 大部分工作。

> 搜索引擎反爬（必应地域重定向 / DuckDuck 验证码 / 百度安全验证），上述需直接查华为开发者文档或真机实测。

## 六、工具链与已验证里程碑（2026-11-18 实测）

**本机已具备完整 OHOS 工具链**（DevEco Studio 自带，P1 基本就绪）：
- DevEco Studio **26.0.0.821**（`C:\Program Files\Huawei\DevEco Studio`），自带 jbr（JDK 21+）、hvigor、ohpm、node、模拟器。
- OHOS NDK **API 26**（`sdk\default\openharmony\native`）：OHOS clang **15.0.4** + 各架构 wrapper（aarch64/armv7/x86_64/loongarch64-unknown-linux-ohos）+ lld + llvm-ar + sysroot。
- Rust OHOS target `aarch64-unknown-linux-ohos` **已装**（rustup 有预编译 std，**无需 nightly/build-std**；下载走 7897 代理）。

**P2 概念验证已通过**：
- `cargo check -p ide-core --target aarch64-unknown-linux-ohos` → Finished 9.6s，零错误。
- `cargo check -p ide-run --target aarch64-unknown-linux-ohos`（含 37 个 tree-sitter C 依赖 + portable-pty）→ Finished 45.6s，零错误。

**可用的 NDK 交叉编译配置**（路径含空格，用 8.3 短路径规避 cc-rs 的 CFLAGS 空格解析）：
```
CC_aarch64_unknown_linux_ohos=C:\PROGRA~1\Huawei\DEVECO~1\sdk\default\OPENHA~1\native\llvm\bin\clang.exe
CFLAGS_aarch64_unknown_linux_ohos=--target=aarch64-unknown-linux-ohos --sysroot=C:\PROGRA~1\Huawei\DEVECO~1\sdk\default\OPENHA~1\native\sysroot
AR_aarch64_unknown_linux_ohos=C:\PROGRA~1\Huawei\DEVECO~1\sdk\default\OPENHA~1\native\llvm\bin\llvm-ar.exe
```
注：`aarch64-unknown-linux-ohos-clang` wrapper 是 bash 脚本，Windows cmd 不能直接跑，须用 `clang.exe` + 显式 `--target`/`--sysroot`。

**P2 链接实证已通过**（新建 `ide-ohos` cdylib 桥接 crate，P3 起点）：
- `cargo build -p ide-ohos --target aarch64-unknown-linux-ohos` → 产出 `libide_ohos.so`（4.7MB），ELF 64 位 **AArch64**（machine 183），ide-core 已链入，Finished 22.8s 零错误。
- `llvm-nm -D` 确认导出符号 `oxidea_ohos_probe`（T 全局，NAPI 可调用）。
- linker 配置（env，路径用 8.3 短路径）：
```
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_LINKER=C:\PROGRA~1\Huawei\DEVECO~1\sdk\default\OPENHA~1\native\llvm\bin\clang.exe
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_OHOS_RUSTFLAGS=-C link-arg=--target=aarch64-unknown-linux-ohos -C link-arg=--sysroot=C:\PROGRA~1\Huawei\DEVECO~1\sdk\default\OPENHA~1\native\sysroot
```

**仍待解决**：
- P3：`ide-ohos` 探针替换为真正 NAPI 桥（napi-rs 或手写 NAPI）+ ArkTS 壳 + ArkWeb 装前端 + `invoke` shim。
- Tauri 壳 `src-tauri` 无法编 OHOS（wry 无鸿蒙后端，预期）→ 走路径 B ArkTS 壳。
- 鸿蒙 PC JDK（jdtls 用）+ 进程/文件权限 + 兼容层（P0 三项，需华为文档/真机）。

## 七、复用与改动面

| 部分 | 复用程度 | 改动 |
|---|---|---|
| 前端 `src/` + `dist/` | 高 | 仅加 `invoke` shim 映射 NAPI |
| Rust 后端 crates | 中 | OHOS 编译 + 桌面 API（进程/文件/pty）鸿蒙适配 |
| Tauri 壳 `src-tauri/` | 低 | 鸿蒙不用 Tauri 壳，改 ArkTS 壳 + NAPI 桥（新增） |
| Java 扩展 `java/java-usages` | 中 | jar 逻辑复用，但需鸿蒙 PC JDK 运行 jdtls |

## 八、Java「鸿蒙版」澄清

- 早期鸿蒙 2/3 支持 Java API 开发应用；NEXT 主推 ArkTS / 仓颉 Cangjie，Java 应用 API 非主力。
- 毕昇 JDK = 华为 OpenJDK 发行版（鲲鹏 ARM + openEuler），**不等于鸿蒙 PC 可用 JDK**。
- jdtls 依赖标准 JDK 21+ 运行时，鸿蒙 PC 能否提供是 P0 待验证项 2。