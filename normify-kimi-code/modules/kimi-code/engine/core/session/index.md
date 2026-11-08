---
uid: e1e00002
id: kimi-code.engine.core.session
parent: kimi-code.engine.core
name: {zh: "会话与存储", en: "Sessions & store"}
description:
  zh: >
      会话生命周期与补丁：创建/恢复/分叉、session patch、SQLite 存储与标题生成。
      
  en: >
      Session lifecycle and patching: create/resume/fork, session patch, SQLite store, title generation.
      
revision: 7fa481a8f2e4f854e39e3477be972466b263eb96
updated_at: "2026-11-08T09:21:52.406Z"
fingerprint: fb6d5ef3f218a6acb3a5667145dd3c4055db003e460234c07a51b4a74889419d
source:
  - path: "packages/kimi-agent/src/session/mod.rs"
  - path: "packages/kimi-agent/src/session/sqlite_store.rs"
  - path: "packages/kimi-agent/src/session/patch.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.server.transcript
    label: {zh: "会话落盘", en: "persist sessions"}
---
