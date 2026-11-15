---
uid: e0e00010
id: kimi-code.engine.core.injection
parent: kimi-code.engine.core
name: {zh: "回合注入层", en: "Turn injections"}
description:
  zh: >
      把状态、模式与技能折成 <system-reminder> 注入到步首。注册表是唯一入口，DomainValueSource 是它读领域值的唯一契约。
      
  en: >
      Folds state, modes and skills into <system-reminder> at each step head. The registry is the single entry point, and DomainValueSource the one contract it reads domain values through.
      
revision: 2c47ec0cfa7f2a2418c6b44ec10ec14d29f5f7cf
updated_at: "2026-11-15T18:51:51.049Z"
fingerprint: 20daae2d18c8accd7f5b5ba23fa3a3c142afd94a18a0c5fb0faf3a409ef7042f
source:
  - path: "packages/kimi-agent/src/injection/mod.rs"
  - path: "packages/kimi-agent/src/injection/state.rs"
---
