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
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-12T10:20:22.280Z"
fingerprint: b2cb0245964822b8756c8a275832ab4a100693553539fec13e98950f6ab2bdd1
source:
  - path: "packages/kimi-agent/src/session/mod.rs"
  - path: "packages/kimi-agent/src/session/sqlite_store.rs"
  - path: "packages/kimi-agent/src/session/patch.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.server.transcript
    label: {zh: "会话落盘", en: "persist sessions"}
---
