# Tauri + React + Typescript

This template should help get you started developing with Tauri, React and Typescript in Vite.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## 文档

- [本地自记忆系统](docs/memory/README.md) — 架构与 Mermaid 图、目录与数据模型、打分与注入格式、命令、配置、治理、隐私边界、迁移与备份。
- [System 1 决策层](deliverables/design-decision-layer.md) — Laya（本地）/ Jev（云端）/ System 2 三级决策漏斗、敏感数据钉死本地、决策缓存与审计；本地推理 sidecar 见 `scripts/laya-sidecar/`。
- 模块设计（只做设计，不含实现代码）：
  [Context](deliverables/design-context-runtime.md) ·
  [Tool Runtime](deliverables/design-tool-runtime.md) ·
  [Task Engine](deliverables/design-task-engine.md) ·
  [Memory 2.0](deliverables/design-memory-2.md) ·
  [Security & Permission](deliverables/design-security-permission.md) ·
  [多 Agent 并行执行](deliverables/design-multi-agent.md)
