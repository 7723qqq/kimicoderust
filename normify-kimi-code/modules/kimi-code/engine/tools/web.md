---
uid: e1e00008
id: kimi-code.engine.tools.web
parent: kimi-code.engine.tools
name: {zh: "网络与抓取", en: "Web & fetch"}
description:
  zh: >
      注册工具名：WebSearch、FetchURL、SwitchSearchEngine、NotifyUser 与 GitHub* 族。
      
  en: >
      Registered tool names: WebSearch, FetchURL, SwitchSearchEngine, NotifyUser and the GitHub* family.
      
revision: 64d16e71bf166ad916efc1a3dba5662fbadab1b7
updated_at: "2026-11-08T09:03:28.955Z"
fingerprint: a7d0733d076fb8b1cd95acff4a9a23180e7a125369ae03058aa9ba2fce318b32
source:
  - path: "packages/kimi-agent/src/tools/web_search.rs"
  - path: "packages/kimi-agent/src/tools/fetch_url.rs"
  - path: "packages/kimi-agent/src/tools/github.rs"
  - path: "packages/kimi-agent/src/tools/github/execute.rs"
  - path: "packages/kimi-agent/src/tools/github/http.rs"
  - path: "packages/kimi-agent/src/tools/github/schema.rs"
  - path: "packages/kimi-agent/src/tools/github/specs.rs"
  - path: "packages/kimi-agent/src/tools/read_media.rs"
apis:
  - protocol: rpc
    path: "WebSearch"
    description:
      zh: >
          搜索网页
          
      en: >
          Search the web
          
  - protocol: rpc
    path: "FetchURL"
    description:
      zh: >
          抓取 URL 为 markdown
          
      en: >
          Fetch a URL as markdown
          
  - protocol: rpc
    path: "SwitchSearchEngine"
    description:
      zh: >
          切换搜索引擎
          
      en: >
          Switch the search engine
          
  - protocol: rpc
    path: "GitHubGetPR"
    description:
      zh: >
          GitHub API 操作
          
      en: >
          GitHub API operations
          
---
