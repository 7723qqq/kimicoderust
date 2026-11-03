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
      
revision: b2c89a8fe8d80ce9b97a5f08206c20f28072e4b0
updated_at: "2026-11-02T00:33:59.541Z"
fingerprint: 29c0ae93fa7833b6ee76e6ad6db5505ada1a548a765d7aa96164f0813acfbd20
source:
  - path: "scripts/check-architecture-drift.mjs"
  - path: "scripts/scan-parity.mjs"
apis: []
---
