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
      
revision: d18f512026ab0cfb3688c4662bf934e25caff595
updated_at: "2026-10-31T16:23:51.520Z"
fingerprint: 0cc05e9c43c3372ac149a16b811e2a5d390af37bb5e8948f0bd900a40509e632
source:
  - path: "packages/kimi-agent/src/session/mod.rs"
  - path: "packages/kimi-agent/src/session/sqlite_store.rs"
  - path: "packages/kimi-agent/src/session/patch.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.server.transcript
    label: {zh: "会话落盘", en: "persist sessions"}
---
