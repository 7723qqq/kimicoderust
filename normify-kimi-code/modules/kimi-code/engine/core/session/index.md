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
      
revision: 2c47ec0cfa7f2a2418c6b44ec10ec14d29f5f7cf
updated_at: "2026-11-15T18:51:51.052Z"
fingerprint: 7b7e22d4971bf158e5384fb3fa477b8c933db1fd08dc55c2ea9d1a90b0998506
source:
  - path: "packages/kimi-agent/src/session/mod.rs"
  - path: "packages/kimi-agent/src/session/sqlite_store.rs"
  - path: "packages/kimi-agent/src/session/patch.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.server.transcript
    label: {zh: "会话落盘", en: "persist sessions"}
---
