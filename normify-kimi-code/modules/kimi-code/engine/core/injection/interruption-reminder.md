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
      
revision: 3b18eb137f8d3057962e2f89ee9e34901ced8df4
updated_at: "2026-11-06T22:57:55.011Z"
fingerprint: 6541c1e827b4c7809ca495950749bfd3cde13ac25201b66d44e1884e54b76dc0
source:
  - path: "packages/kimi-agent/src/injection/interruption_reminder.rs"
apis: []
---
