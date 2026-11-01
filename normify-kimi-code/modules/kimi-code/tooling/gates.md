---
uid: "70e00001"
id: kimi-code.tooling.gates
parent: kimi-code.tooling
name: {zh: "质量门禁", en: "Quality gates"}
description:
  zh: >
      CI 与 pre-commit 门禁：架构漂移指纹、locale 键一致、Rust-TS parity、上游 delta 分诊。
      
  en: >
      CI and pre-commit gates: architecture drift fingerprints, locale-key consistency, Rust-TS parity, upstream delta triage.
      
revision: b46cc711fbf0f7be46d03a1ff637613ab2b7e8a3
updated_at: "2026-11-01T07:59:04.624Z"
fingerprint: 4aa2de8359bd1e197a9bff393e34bf8233b3a23d42a1b81bf9604e9c1db65cc2
source:
  - path: "scripts/check-architecture-drift.mjs"
  - path: "scripts/scan-parity.mjs"
apis: []
---
