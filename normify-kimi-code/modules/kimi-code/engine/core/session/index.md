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
      
revision: b5bf884dce712b7fb6a56859dac2cca05d87b18a
updated_at: "2026-10-31T15:13:40.668Z"
fingerprint: 71a3260b0204c6ebdbe2cdf1f653ad0757cc585df0be06378de891b60fde64b9
source:
  - path: "packages/kimi-agent/src/session/mod.rs"
  - path: "packages/kimi-agent/src/session/sqlite_store.rs"
  - path: "packages/kimi-agent/src/session/patch.rs"
deps:
  - kind: dataflow
    to: kimi-code.engine.server.transcript
    label: {zh: "会话落盘", en: "persist sessions"}
---
