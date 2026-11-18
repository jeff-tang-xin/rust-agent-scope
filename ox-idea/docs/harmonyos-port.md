# OXidea 鸿蒙 PC 移植方案

> 状态：**P0 已落实，架构定案**（分支 `feat/harmonyos`，已推 origin）。
> 定位：把 OXidea（Tauri 2 桌面 IDE）移植到**鸿蒙 PC**（HarmonyOS 5/6 桌面版）。
> **定案：Linux 子系统跑完整后端 + 鸿蒙 HAP 前端壳**，而非纯原生 HAP。
> 事实与配置详情见知识库 `oxidea-harmonyos-port-feasibility`。

## 〇、结论速览（架构定案）

- **最优架构 = 融合开发引擎 Linux 子系统跑完整后端（Rust + jdtls + JDK21）+ 鸿蒙 HAP 前端壳**，localhost/共享文件夹通信。
- 为何不走纯原生 HAP：① 原生沙箱**禁止 fork 子进程**（spawn jdtls/终端/Git 行不通）② 官方仅**毕昇 JDK 17**（jdtls 需 21+，禁 `sun.*` 反射、无 AWT/Swing）。
- **已有 Tauri 鸿蒙分支 / cj-tauri（仓颉版）**，前端壳不必从零造。
- 本机 DevEco 工具链就绪，Rust 后端已实证可编鸿蒙 AArch64 `.so`（P2 完成，用作薄壳 native 入口）。
- **最低成本捷径待验证**：融合开发引擎能否跑 Linux GUI 应用——若能，OXidea Linux 整包可能零移植直接跑。
- **产品目标（用户定案 A）**：做**鸿蒙 PC 完整 IDE 产品**，HAP 一键安装运行；非在平板/手机跑。
- **测试约束**：用户只有鸿蒙平板+手机、无 PC 真机；本机 DevEco 仅 **phone 模拟器镜像**（HarmonyOS 7.0.0 x86），能测 HAP 壳界面，**测不了 PC 后端**（融合开发引擎是 PC 特性）。
- **传输要点**：ArkWeb 是浏览器、不能开 raw TCP，前端 `invoke` 需 **WebSocket 或 NAPI 桥**接 oxidea-server（当前 `--listen` 是 raw TCP + Content-Length 帧）。
- **命门待确认**：HAP 能否编程式控制融合开发引擎（启动/管理 Linux 进程）——决定一键全功能 HAP 可行性。

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

## 三、定案架构与 JDK 运行机制

### 核心问题：JDK 21/25（jdtls/IDEA 需要）怎么在鸿蒙 PC 跑？

**关键概念区分**：「鸿蒙官方支持毕昇 JDK 17」指的是**原生应用层认证的 JDK**，不是通用 JVM。
要在鸿蒙 PC 跑 jdtls/IDEA 这类需要 JDK 21/25 的重型 Java 程序，**不在原生沙箱里跑，而在 Linux 子系统里跑标准 Linux OpenJDK**。

| 途径 | 怎么跑 JDK 21/25 | 现实性 |
|---|---|---|
| **① Linux 子系统（融合开发引擎，最优）** | openEuler Linux 环境里跑**标准 OpenJDK 21/25 Linux 版**（aarch64/x64），当普通 Linux 进程，完整 JVM，**无 fork/反射/Swing 限制** | 最高，官方兜底 |
| ② JVM 共享库嵌入（社区 Eclipse 移植） | `libjvm.so` 编 OHOS 嵌进 app 进程，NAPI/JNI 交互 | 重工程，fork 限制仍在 |
| ③ OpenHarmony 标准系统开发板 | Linux 内核跑 OpenJDK 移植 | 开发板，非消费级 PC |

**IDEA 怎么跑**：IntelliJ IDEA 是 Swing 桌面 Java 应用 → 原生沙箱跑不了（毕昇 JDK17 无 AWT/Swing + fork 禁）；
**在 Linux 子系统里用完整 Linux JDK 直接跑**（若融合开发引擎支持 Linux GUI，IDEA/OXidea Linux 整包可零移植运行）。

### 定案分层架构

```
鸿蒙 PC
├─ 展示层：鸿蒙原生 HAP 壳（ArkWeb 渲染 OXidea 前端 dist/）
│    └─ invoke ── localhost / 共享文件夹 ──┐
└─ 融合开发引擎 Linux 子系统（openEuler）   │
     └─ OXidea 完整后端 ◀─────────────────┘
          ├─ Rust 后端（现有 Linux build）
          ├─ jdtls + 完整 JDK 21/25（标准 Linux OpenJDK，无限制）
          └─ 终端 / Git / 运行调试（自由 spawn）
```

### 现成地基：oxidea-server 远程开发架构（P4 已实证）

OXidea **已有完整的客户端-服务端远程开发架构**，鸿蒙几乎直接复用：
- `ide-host` = 无 UI toolkit 的 IDE 后端（Host：项目/终端/进程/LSP/插件）。
- `oxidea-server`（headless 后端）：`--stdio`（SSH exec）/ `--connect`（relay）/ **`--listen <port>`（本次新增，localhost TCP）**。
- JSON-RPC 协议 v2：`invoke {window, cmd, args}` → 命令结果；`event`/`channel` 通知回传。前端 `remote_dev.rs` 的 `remote_invoke` 已走此协议。
- `rpc::serve<R: AsyncRead, W: AsyncWrite>` 传输无关，`TcpStream` 直接传入。

**本次实现 + 实证**：给 oxidea-server 加 `--listen <port>` 模式（TcpListener accept → split → rpc::serve；ide-host tokio 加 `net` feature）。
本机冒烟测试通过：`oxidea-server --listen 18765` → TCP 客户端发 `initialize` → 返回 `ServerInfo {arch, os, protocol:2, version, home}`。
**鸿蒙映射**：Linux 子系统跑 `oxidea-server --listen <port>`，HAP 前端 ArkWeb 经 localhost TCP/WebSocket 发 `invoke`、收 `event`——前端业务代码与后端几乎全复用。

### 前端壳选型（已有 Tauri 鸿蒙适配，非从零）

| 方案 | 说明 | 成熟度 |
|---|---|---|
| **cj-tauri（仓颉版）** | 业务代码零修改；WebView 换 ArkWeb + `desktopMode` + `ohos.permission.INTERNET` | 较成熟，有成功案例 |
| **Rust 版 Tauri 鸿蒙分支** | 官方文档提及，Rust 后端 + HAP | 生态不如 cj-tauri |
| **自建 ArkTS + ArkWeb + NAPI** | ide-ohos `.so` 做薄壳 native 入口，转发到 Linux 后端 | 工作量大，兜底 |

### 为何不走纯原生 HAP（路径已否决）
原生沙箱**禁止 fork**（spawn jdtls/pty/git 行不通）+ 官方仅 JDK17（jdtls 需 21+、禁 `sun.*` 反射、无 AWT/Swing）。
故 ide-ohos `.so` 只做**薄壳入口**（NAPI 转发到 Linux 后端），不在原生沙箱 spawn jdtls。

## 四、阶段计划（重排）

| 阶段 | 目标 | 状态 / 完成标准 |
|---|---|---|
| **P0 前提验证** | 进程权限 / JDK / 兼容层 | ✅ 已落实（见五） |
| **P-opt 捷径验证** | 融合开发引擎跑 Linux GUI | 待验证：若能跑 OXidea Linux 整包则零移植 |
| **P1 工具链** | DevEco + NDK + Rust OHOS target | 🟡 本机就绪（见六） |
| **P2 Rust 编鸿蒙** | 后端编译 + link | ✅ 完成：ide-ohos link 成 AArch64 `.so`（薄壳入口） |
| **P3 前端 HAP 壳** | 选型 + ArkWeb 装前端 + invoke | 评估 cj-tauri / Rust 鸿蒙分支 / 自建；前端 dist/ 渲染 |
| **P4 后端部署 + 通信** 🟡 传输已实证 | Linux 子系统跑后端 + IPC | oxidea-server `--listen` localhost TCP 已实现 + 冒烟通过；剩 Linux 子系统部署 + 前端对接 |
| **P5 集成真机** | HAP + 子系统联调 | 鸿蒙 PC 真机编辑/导航/运行可用 |

## 五、P0 三项已落实（2026-11-18 权威分析）

1. **进程权限**：强沙箱不放开；PC 有 `system_grant` 免弹窗权限（文件读写下载/文档/桌面 + `FILE_ACCESS_PERSIST` 持久化任意目录；PTRACE `kernel.DEBUGGER`；抓包 `kernel.NET_RAW`）。**普通应用禁止 fork**，`KILL_APP_PROCESSES` 仅系统级；受限权限可申请（开放能力管理，约 3 工作日）。→ 完整 IDE 后端不能放原生沙箱。
2. **JDK**：官方仅**毕昇 JDK 17**（jdtls 需 21+，不够）；无 AWT/Swing（曾预计 2026-06，需确认）；禁反射 `sun.*`（IllegalAccessException）；无 JDK21+ 时间表。社区 JVM 共享库嵌入（Eclipse 移植）重工程化。
3. **Linux 兼容层（最优兜底）**：华为官方**融合开发引擎**（openEuler Linux 子系统，轻量虚拟化，一键跑大多数 Linux CLI/应用）；社区 **Harmonix**（QEMU，跑 Alpine/Ubuntu）。→ 后端放 Linux 子系统用完整 JDK，前端 HAP 壳。

> 详见知识库 `oxidea-harmonyos-port-feasibility`。搜索引擎反爬（必应/DuckDuck/百度），事实由用户提供权威分析落实。

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

**仍待解决（架构定案后）**：
- P-opt：验证融合开发引擎能否跑 Linux GUI（决定 OXidea Linux 整包能否零移植）。
- P3：前端 HAP 壳选型（cj-tauri / Rust 鸿蒙分支 / 自建 ArkTS+ArkWeb）+ 接前端 dist/。
- P4：OXidea 后端 headless server 模式 + Linux 子系统部署 + localhost/共享文件夹通信。
- ide-ohos 重定位为薄壳 native 入口（NAPI 转发到 Linux 后端），不在原生沙箱 spawn jdtls。

## 七、复用与改动面

| 部分 | 复用程度 | 改动 |
|---|---|---|
| 前端 `src/` + `dist/` | 高 | HAP 壳 ArkWeb 加载；invoke 经 localhost 转 Linux 后端 |
| Rust 后端 crates | 高 | 直接用现有 Linux build 跑在 Linux 子系统（无需 OHOS 适配）；ide-ohos 仅薄壳入口 |
| Tauri 壳 `src-tauri/` | 中 | 前端壳用 cj-tauri / Rust 鸿蒙分支 / 自建 ArkTS；后端 headless server 化 |
| Java 扩展 `java/java-usages` | 高 | jar 逻辑复用，jdtls + 完整 JDK21/25 在 Linux 子系统跑 |

## 八、Java「鸿蒙版」澄清

- 早期鸿蒙 2/3 支持 Java API 开发应用；NEXT 主推 ArkTS / 仓颉 Cangjie，Java 应用 API 非主力。
- 毕昇 JDK = 华为 OpenJDK 发行版（鲲鹏 ARM + openEuler），**不等于鸿蒙 PC 可用 JDK**。
- jdtls 需标准 JDK 21+：鸿蒙原生仅毕昇 JDK17 不够 → **放 Linux 子系统跑标准 Linux OpenJDK 21/25**（见三）。