---
uid: "51e00001"
id: kimi-code.sdk.node.native-client
parent: kimi-code.sdk.node
name: {zh: "原生 RPC 客户端", en: "Native RPC client"}
description:
  zh: >
      原生传输：SDKRpcClientNative、ensurePluginStore 种子逻辑与插件 API 面。
      
  en: >
      The native transport: SDKRpcClientNative, ensurePluginStore seeding and the plugin API surface.
      
revision: 467cc88fb4ad878d4eb4f3caf0f9e5b9e1e776e9
updated_at: "2026-11-01T03:24:40.963Z"
fingerprint: fcd2935d3f23c1604fc4c13ec2ca2de7493924044ad8b158c3de17e7871ff5a5
source:
  - path: "packages/node-sdk/src/native/sdk-rpc-client-native.ts"
apis:
  - protocol: rpc
    path: "createSession"
    description:
      zh: >
          创建 agent 会话
          
      en: >
          Create an agent session
          
  - protocol: rpc
    path: "prompt"
    description:
      zh: >
          运行一轮 prompt
          
      en: >
          Run a prompt turn
          
  - protocol: rpc
    path: "listPlugins"
    description:
      zh: >
          列出已安装插件
          
      en: >
          List installed plugins
          
deps:
  - kind: call
    to: kimi-code.engine.napi.bindings
    from_api: "rpc:listPlugins"
    to_api: "rpc:pluginList"
    label: {zh: "napi 调用", en: "napi calls"}
---
