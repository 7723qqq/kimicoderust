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
      
revision: 28f4b101d79bba811d53e53662341c0bb0fe93c0
updated_at: "2026-11-06T04:54:04.155Z"
fingerprint: a25813db8a7f3f81f3ae9f23da57d53f67aec02146fd8b0dbe93440ed99f93eb
source:
  - path: "packages/kimi-agent/src/injection/mod.rs"
  - path: "packages/kimi-agent/src/injection/state.rs"
---
