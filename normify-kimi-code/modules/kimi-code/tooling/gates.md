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
      
revision: 28f4b101d79bba811d53e53662341c0bb0fe93c0
updated_at: "2026-11-03T16:01:43.476Z"
fingerprint: e6abeb45e6dd7fd0ccb02547c361313f8000027a1a5e39a2b5e311f00149f5b0
source:
  - path: "scripts/check-architecture-drift.mjs"
  - path: "scripts/scan-parity.mjs"
apis: []
---
