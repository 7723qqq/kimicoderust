---
uid: e0e00013
id: kimi-code.engine.core.injection.interruption-reminder
parent: kimi-code.engine.core.injection
name: {zh: "中断提醒", en: "Interruption reminder"}
description:
  zh: >
      上一回合被用户中断时，在下一回合步首注入一次性提醒；历史里已有就不再重复。
      
  en: >
      A one-shot reminder at the next turn's head when the previous turn was interrupted by the user; suppressed when history already carries one.
      
revision: f22a80d7ce2cfd1a1e88dff32aa14f9120c909d2
updated_at: "2026-11-11T19:20:38.909Z"
fingerprint: 43372183f17d74b06979a2dcef02eed5ae83ba1fbade88ae9bac66ec93469040
source:
  - path: "packages/kimi-agent/src/injection/interruption_reminder.rs"
apis: []
---
