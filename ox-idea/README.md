<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="src-tauri/icons/source/oxidea-wordmark-dark.svg">
    <img src="src-tauri/icons/source/oxidea-wordmark.svg" alt="OXidea" height="56">
  </picture>
</p>

<p align="center">
  The IDE you know, rebuilt in Rust.
</p>

<p align="center">
  <a href="https://github.com/l0ng-ai/oxidea/releases/latest"><b>Download</b></a>
  &nbsp;·&nbsp;
  <a href="#benchmark">Benchmark</a>
  &nbsp;·&nbsp;
  <a href="#plugins">Plugins</a>
  &nbsp;·&nbsp;
  <a href="#community">Community</a>
</p>

https://github.com/user-attachments/assets/234dd54c-1255-4b3a-b35c-533d78d737c0

OXidea brings the IntelliJ IDEA way of working — its keymap, its tool windows,
its search-everything habits — to a native app with a Rust core.

Editor, code insight, debugger, Git, terminal, database and HTTP clients,
for 60+ languages. Builds for macOS, Windows and Linux. On Windows, if you'd
rather skip the installer, each release also ships a portable zip — unpack it
and run `oxidea.exe`.

## Benchmark

Opening [freqtrade](https://github.com/freqtrade/freqtrade) (Python, ~770 files)
with warm caches, median of 5 runs:

|                             | OXidea          | IntelliJ IDEA 2026.2.3 |
| --------------------------- | ----------------- | ---------------------- |
| Window with the project     | **0.9 s**         | 3.1 s                  |
| Ready (CPU settled)         | **1.1 s**         | 13.2 s                 |
| Memory (USS, all processes) | **169 MB**        | 2011 MB                |
| CPU time until ready        | **2.4 s**         | 47.9 s                 |
| Idle CPU                    | **4 %** of a core | 7 % of a core          |
| Quit                        | **0.3 s**         | 5.9 s                  |

A release build of OXidea from `main` and IDEA with default settings and plugins, on
Windows 11, 32-core AMD, 62 GB RAM.
IDEA analyses Python more deeply, so "ready" doesn't mean the same work on
both sides. Both are measured from the outside, the same way: see
[`perf/compare`](perf/compare) to run it on your machine and projects.

## Plugins

Plugins add actions, completion, quick fixes, tool windows, custom editors,
themes, file icons and languages. They run as sandboxed WebAssembly components;
themes, icon themes and languages need no code at all.

Install them from Settings › Plugins, or start your own with
`oxidea-plugin new` — see [Writing OXidea plugins](plugins/README.md) and
the [official plugins](plugins/official). The plugin API is still a preview.

## The built-in agent

OXidea ships with a built-in coding agent (OX Hareness). Its framework layer
lives in this repository: `crates/agentscope-acp` adapts the agent to the
[Agent Client Protocol](https://agentclientprotocol.com), and it builds directly
on the framework crates of this repo — `agentscope-core`, `agentscope-harness`,
`agentscope-browser`, `agentscope-db` and the six model-provider crates under
[`crates/`](../crates). That framework is a Rust port of **AgentScope Java 2.0.4**
([agentscope-ai/agentscope-java](https://github.com/agentscope-ai/agentscope-java));
the module mapping is documented in [`docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md).

## Community

- [Discord](https://discord.gg/2Zy4yn7gP) — questions, feedback and release news.
- WeChat group — scan to join:

  <img src="docs/assets/wechat-group.png" alt="WeChat group QR code" width="200">

### Contributors

<a href="https://github.com/l0ng-ai/oxidea/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=l0ng-ai/oxidea&max=100" alt="OXidea contributors">
</a>

## License

[Apache 2.0](LICENSE). Bundled third-party assets are listed in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).

<sub>Not affiliated with JetBrains. IntelliJ IDEA is a trademark of JetBrains s.r.o.</sub>
