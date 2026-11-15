use crate::callbacks::HostCallbacks;
use crate::llm::http::NativeHttpLlm;
use crate::llm::proxy::HostLlmProxy;
use crate::rpc::types::NativeLlmConfig;
use futures_util::future::select_all;
use kimi_core::types::*;
use std::collections::HashMap;
use std::sync::Arc;

pub struct LlmProvider {
    pub name: String,
    pub system_prompt: String,
    pub model: String,
    pub callbacks: Arc<dyn HostCallbacks>,

    pub native: Option<NativeLlmConfig>,

    pub cache_key: Option<String>,
}

impl LlmProvider {
    pub fn host(
        name: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        callbacks: Arc<dyn HostCallbacks>,
    ) -> Self {
        Self {
            name: name.into(),
            system_prompt: system_prompt.into(),
            model: model.into(),
            callbacks,
            native: None,
            cache_key: None,
        }
    }

    pub fn native(
        name: impl Into<String>,
        system_prompt: impl Into<String>,
        config: NativeLlmConfig,
        callbacks: Arc<dyn HostCallbacks>,
    ) -> Self {
        Self {
            name: name.into(),
            system_prompt: system_prompt.into(),
            model: config.model.clone(),
            callbacks,
            native: Some(config),
            cache_key: None,
        }
    }

    pub fn with_cache_key(mut self, key: Option<String>) -> Self {
        self.cache_key = key.filter(|value| !value.is_empty());
        self
    }

    fn build_transport(
        &self,
        request_id: Option<String>,
        ledger: Option<Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>>,
    ) -> Arc<dyn LLM> {
        let (llm, native): (Arc<dyn LLM>, bool) = match &self.native {
            Some(config) if config.protocol == "antigravity" => (
                Arc::new(crate::llm::antigravity::AntigravityLlm::new(
                    config.clone(),
                    self.system_prompt.clone(),
                )),
                true,
            ),
            Some(config) => (
                Arc::new(
                    NativeHttpLlm::new(config.clone(), self.system_prompt.clone())
                        .with_cache_key(self.cache_key.clone()),
                ),
                true,
            ),
            None => {
                let proxy = HostLlmProxy::new(self.system_prompt.clone(), self.model.clone())
                    .with_callbacks(self.callbacks.clone());
                let proxy = match request_id {
                    Some(id) => proxy.with_request_id(id),
                    None => proxy,
                };
                (Arc::new(proxy), false)
            }
        };
        if native && let Some(ledger) = ledger {
            llm.set_tool_call_ids(ledger);
        }
        llm
    }
}

static NEXT_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug)]
pub struct ProviderResult {
    pub provider_name: String,
    pub result: Result<LLMChatResponse, String>,
    pub elapsed_ms: u64,
}

pub struct MultiLLM {
    providers: Vec<LlmProvider>,
    label: String,

    tool_call_ids: std::sync::Mutex<Option<Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>>>,
    last_trace_id: std::sync::Mutex<Option<String>>,
    interrupted_response: Arc<std::sync::Mutex<Option<LLMChatResponse>>>,
}

impl MultiLLM {
    pub fn new(providers: Vec<LlmProvider>) -> Self {
        let label = if providers.len() <= 1 {
            providers
                .first()
                .map(|p| p.model.clone())
                .unwrap_or_default()
        } else {
            format!("{} + {} others", providers[0].model, providers.len() - 1)
        };
        Self {
            providers,
            label,
            tool_call_ids: std::sync::Mutex::new(None),
            last_trace_id: std::sync::Mutex::new(None),
            interrupted_response: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    #[tracing::instrument(name = "MultiLLM::first_past_the_post", skip_all, fields(providers = self.providers.len()))]
    pub async fn first_past_the_post(
        &self,
        params: LLMChatParams,
    ) -> Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>> {
        self.first_past_the_post_with_winner(params)
            .await
            .map(|(response, _native_winner)| response)
    }

    #[tracing::instrument(name = "MultiLLM::first_past_the_post", skip_all, fields(providers = self.providers.len()))]
    async fn first_past_the_post_with_winner(
        &self,
        params: LLMChatParams,
    ) -> Result<(LLMChatResponse, bool), Box<dyn std::error::Error + Send + Sync>> {
        self.interrupted_response
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if self.providers.is_empty() {
            return Err("No LLM providers configured".into());
        }
        let ledger = self
            .tool_call_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if self.providers.len() == 1 {
            let native = self.providers[0].native.is_some();
            let transport = self.providers[0].build_transport(None, ledger);
            let result = transport.chat(params).await;
            if let Some(partial) = transport.take_interrupted_response() {
                *self
                    .interrupted_response
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(partial);
            }
            let response = result?;
            *self.last_trace_id.lock().unwrap_or_else(|e| e.into_inner()) =
                transport.last_trace_id();
            return Ok((response, native));
        }

        let mut handles = Vec::with_capacity(self.providers.len());
        let interrupted_response = self.interrupted_response.clone();
        let mut cancellers: HashMap<String, Arc<dyn HostCallbacks>> = HashMap::new();
        for provider in &self.providers {
            let mut params = params.clone();
            if let Some(parent) = params.cancel.as_ref() {
                params.cancel = Some(parent.child_token());
            }

            let is_native = provider.native.is_some();
            let request_id = if is_native {
                None
            } else {
                let id = format!(
                    "llm-{}-{}",
                    provider.name,
                    NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                );
                cancellers.insert(id.clone(), provider.callbacks.clone());
                Some(id)
            };
            let llm = provider.build_transport(request_id.clone(), ledger.clone());
            let name = provider.name.clone();
            let racer_id = request_id.unwrap_or_else(|| name.clone());

            let interrupted = interrupted_response.clone();
            handles.push(tokio::spawn(async move {
                let start = std::time::Instant::now();
                let result = llm.chat(params).await;
                if let Some(partial) = llm.take_interrupted_response() {
                    let mut slot = interrupted.lock().unwrap_or_else(|e| e.into_inner());
                    if slot.is_none() {
                        *slot = Some(partial);
                    }
                }
                let elapsed = start.elapsed().as_millis() as u64;
                let trace_id = llm.last_trace_id();
                (
                    racer_id,
                    is_native,
                    ProviderResult {
                        provider_name: name,
                        result: result.map_err(|e| e.to_string()),
                        elapsed_ms: elapsed,
                    },
                    trace_id,
                )
            }));
        }

        let (response, native, trace_id) = race_first_success(handles, &cancellers).await?;
        *self.last_trace_id.lock().unwrap_or_else(|e| e.into_inner()) = trace_id;
        Ok((response, native))
    }

    #[tracing::instrument(name = "MultiLLM::all_results", skip_all, fields(providers = self.providers.len()))]
    pub async fn all_results(&self, params: LLMChatParams) -> Vec<ProviderResult> {
        if self.providers.is_empty() {
            return vec![];
        }

        let mut handles = Vec::with_capacity(self.providers.len());
        for provider in &self.providers {
            let params = params.clone();

            let llm = provider.build_transport(None, None);
            let name = provider.name.clone();

            handles.push(tokio::spawn(async move {
                let start = std::time::Instant::now();
                let result = llm.chat(params).await;
                let elapsed = start.elapsed().as_millis() as u64;
                ProviderResult {
                    provider_name: name,
                    result: result.map_err(|e| e.to_string()),
                    elapsed_ms: elapsed,
                }
            }));
        }

        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(r) = handle.await {
                results.push(r);
            }
        }
        results
    }
}

fn is_empty_completion(response: &LLMChatResponse) -> bool {
    response.content.is_empty() && response.tool_calls.is_empty()
}

#[tracing::instrument(name = "race_first_success", skip_all, fields(handles = handles.len()))]
async fn race_first_success(
    mut handles: Vec<tokio::task::JoinHandle<(String, bool, ProviderResult, Option<String>)>>,
    cancellers: &HashMap<String, Arc<dyn HostCallbacks>>,
) -> Result<(LLMChatResponse, bool, Option<String>), Box<dyn std::error::Error + Send + Sync>> {
    let mut errors: Vec<String> = Vec::new();

    let mut empty_fallback: Option<(LLMChatResponse, bool, Option<String>)> = None;
    while !handles.is_empty() {
        let (joined, _index, rest) = select_all(handles).await;
        handles = rest;
        match joined {
            Ok((winner_id, is_native, pr, trace_id)) => match pr.result {
                Ok(response) => {
                    if is_empty_completion(&response) {
                        eprintln!(
                            "MultiLLM: {} returned an empty completion ({}ms); holding it as a fallback",
                            pr.provider_name, pr.elapsed_ms
                        );
                        if empty_fallback.is_none() {
                            empty_fallback = Some((response, is_native, trace_id));
                        }
                        continue;
                    }
                    eprintln!("MultiLLM: {} won ({}ms)", pr.provider_name, pr.elapsed_ms);
                    for handle in &handles {
                        handle.abort();
                    }
                    for (id, callbacks) in cancellers {
                        if id != &winner_id {
                            callbacks.cancel_llm_chat(id);
                        }
                    }
                    return Ok((response, is_native, trace_id));
                }
                Err(e) => errors.push(format!("{}: {e}", pr.provider_name)),
            },
            Err(e) => errors.push(format!("join error: {e}")),
        }
    }
    if let Some(fallback) = empty_fallback {
        return Ok(fallback);
    }
    Err(errors.join("; ").into())
}

impl LLM for MultiLLM {
    fn system_prompt(&self) -> &str {
        self.providers
            .first()
            .map(|p| p.system_prompt.as_str())
            .unwrap_or("")
    }

    fn model_name(&self) -> &str {
        &self.label
    }

    fn model_alias(&self) -> Option<&str> {
        self.providers
            .first()
            .and_then(|entry| entry.native.as_ref()?.alias.as_deref())
    }

    fn provider_type(&self) -> Option<&str> {
        self.providers
            .first()
            .and_then(|entry| entry.native.as_ref()?.provider_type.as_deref())
    }

    fn protocol(&self) -> Option<&str> {
        self.providers
            .first()
            .and_then(|entry| entry.native.as_ref().map(|native| native.protocol.as_str()))
    }

    fn thinking_effort(&self) -> Option<&str> {
        self.providers
            .first()
            .and_then(|entry| entry.native.as_ref()?.reasoning_effort.as_deref())
    }

    fn last_trace_id(&self) -> Option<String> {
        self.last_trace_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn transport(&self) -> &'static str {
        "multi"
    }

    fn take_interrupted_response(&self) -> Option<LLMChatResponse> {
        self.interrupted_response
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn is_retryable_error(&self, error: &str) -> bool {
        self.providers
            .first()
            .map(|p| p.build_transport(None, None).is_retryable_error(error))
            .unwrap_or(false)
    }

    fn set_tool_call_ids(&self, ledger: Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>) {
        *self.tool_call_ids.lock().unwrap_or_else(|e| e.into_inner()) = Some(ledger);
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async move {
            let (mut response, winner_is_native) =
                self.first_past_the_post_with_winner(params).await?;
            if winner_is_native {
                return Ok(response);
            }
            let ledger = self
                .tool_call_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(ledger) = ledger {
                kimi_core::tool_call_id::remap_response_tool_calls(
                    &mut response,
                    &ledger.begin_response(),
                );
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::types::{
        BoxFuture, LlmChatRequest, LlmChatResponse, PermissionCheckRequest, PermissionDecision,
        TokenUsage, ToolExecuteRequest, ToolExecuteResponse,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockTestLlm {
        name: String,
        delay_ms: u64,
        should_fail: bool,
        call_count: Arc<AtomicU32>,
    }

    impl MockTestLlm {
        fn new(name: &str, delay_ms: u64, should_fail: bool) -> Self {
            Self {
                name: name.to_string(),
                delay_ms,
                should_fail,
                call_count: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl LLM for MockTestLlm {
        fn system_prompt(&self) -> &str {
            "mock"
        }
        fn model_name(&self) -> &str {
            &self.name
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }

        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> crate::rpc::types::BoxFuture<
            '_,
            Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
        > {
            let delay = self.delay_ms;
            let fail = self.should_fail;
            let name = self.name.clone();
            let cc = self.call_count.clone();

            Box::pin(async move {
                cc.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                if fail {
                    Err(format!("{name} failed").into())
                } else {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some(name),
                        usage: kimi_core::types::TokenUsage {
                            input_other: 10,
                            output: 5,
                            ..Default::default()
                        },
                        timing: None,
                    })
                }
            })
        }
    }

    #[tokio::test]
    async fn test_mock_llm_ok() {
        let mock = MockTestLlm::new("fast", 5, false);
        let params = LLMChatParams {
            cancel: None,
            messages: Arc::from(Vec::new()),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        };
        let result = mock.chat(params).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().finish_reason.unwrap(), "fast");
    }

    #[tokio::test]
    async fn test_mock_llm_fail() {
        let mock = MockTestLlm::new("failing", 5, true);
        let params = LLMChatParams {
            cancel: None,
            messages: Arc::from(Vec::new()),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        };
        let result = mock.chat(params).await;
        assert!(result.is_err());
    }

    fn mk_response(tag: &str) -> LLMChatResponse {
        LLMChatResponse {
            content: String::new(),
            thinking: vec![],
            tool_calls: vec![],
            finish_reason: Some(tag.to_string()),
            usage: kimi_core::types::TokenUsage {
                input_other: 1,
                output: 1,
                ..Default::default()
            },
            timing: None,
        }
    }

    #[tokio::test]
    async fn test_race_returns_fastest_success() {
        let slow = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            (
                "slow-id".to_string(),
                false,
                ProviderResult {
                    provider_name: "slow".into(),
                    result: Ok(mk_response("slow")),
                    elapsed_ms: 80,
                },
                None,
            )
        });
        let fast = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            (
                "fast-id".to_string(),
                false,
                ProviderResult {
                    provider_name: "fast".into(),
                    result: Ok(mk_response("fast")),
                    elapsed_ms: 5,
                },
                None,
            )
        });
        let (winner, _native, _trace) = race_first_success(vec![slow, fast], &HashMap::new())
            .await
            .unwrap();
        assert_eq!(winner.finish_reason.as_deref(), Some("fast"));
    }

    #[tokio::test]
    async fn test_race_prefers_success_over_fast_failure() {
        let fast_fail = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            (
                "failer-id".to_string(),
                false,
                ProviderResult {
                    provider_name: "failer".into(),
                    result: Err("boom".into()),
                    elapsed_ms: 5,
                },
                None,
            )
        });
        let slow_ok = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            (
                "ok-id".to_string(),
                false,
                ProviderResult {
                    provider_name: "ok".into(),
                    result: Ok(mk_response("ok")),
                    elapsed_ms: 30,
                },
                None,
            )
        });
        let (winner, _native, _trace) =
            race_first_success(vec![fast_fail, slow_ok], &HashMap::new())
                .await
                .unwrap();
        assert_eq!(winner.finish_reason.as_deref(), Some("ok"));
    }

    struct CancellationSpy {
        cancelled: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CancellationSpy {
        fn new() -> (Arc<CancellationSpy>, Arc<std::sync::Mutex<Vec<String>>>) {
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    cancelled: log.clone(),
                }),
                log,
            )
        }
    }

    impl HostCallbacks for CancellationSpy {
        fn llm_chat(
            &self,
            request: crate::rpc::types::LlmChatRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
        {
            let slow = request.model_name == "slow";
            let request_id = request.request_id.clone();
            Box::pin(async move {
                if slow {
                    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                }
                Ok(crate::rpc::types::LlmChatResponse {
                    content: request_id.unwrap_or_default(),
                    tool_calls: vec![],
                    thinking: vec![],
                    finish_reason: Some(if slow { "slow" } else { "fast" }.to_string()),
                    usage: crate::rpc::types::TokenUsage::default(),
                })
            })
        }

        fn execute_tool(
            &self,
            _request: crate::rpc::types::ToolExecuteRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async { Err("not used by this test".into()) })
        }

        fn check_permission(
            &self,
            _request: crate::rpc::types::PermissionCheckRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Err("not used by this test".into()) })
        }

        fn cancel_llm_chat(&self, request_id: &str) {
            self.cancelled.lock().unwrap().push(request_id.to_string());
        }
    }

    #[tokio::test]
    async fn test_losing_providers_are_cancelled_at_the_host() {
        let (slow_spy, slow_log) = CancellationSpy::new();
        let (fast_spy, fast_log) = CancellationSpy::new();
        let slow_callbacks: Arc<dyn HostCallbacks> = slow_spy;
        let fast_callbacks: Arc<dyn HostCallbacks> = fast_spy;
        let multi = MultiLLM::new(vec![
            LlmProvider::host("slow", "slow", "", slow_callbacks.clone()),
            LlmProvider::host("fast", "fast", "", fast_callbacks.clone()),
        ]);

        let winner = multi
            .first_past_the_post(LLMChatParams {
                cancel: None,
                messages: Arc::from(Vec::new()),
                tools: Arc::from(Vec::new()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(winner.finish_reason.as_deref(), Some("fast"));

        let cancelled = slow_log.lock().unwrap();
        assert_eq!(
            cancelled.len(),
            1,
            "the losing provider must be cancelled once"
        );
        assert!(
            cancelled[0].contains("slow"),
            "unexpected id: {}",
            cancelled[0]
        );
        drop(cancelled);

        assert!(
            fast_log.lock().unwrap().is_empty(),
            "the winning provider must not be cancelled"
        );
    }

    #[tokio::test]
    async fn a_native_racer_races_without_the_host_llm_proxy() {
        struct RefusingHost;
        impl HostCallbacks for RefusingHost {
            fn llm_chat(
                &self,
                _request: LlmChatRequest,
            ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
                Box::pin(async { Err("this host has no LLM proxy".to_string()) })
            }
            fn execute_tool(
                &self,
                _request: ToolExecuteRequest,
            ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
                Box::pin(async { Err("not used".to_string()) })
            }
            fn check_permission(
                &self,
                _request: PermissionCheckRequest,
            ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
                Box::pin(async { Ok(PermissionDecision::allow()) })
            }
        }

        let callbacks: Arc<dyn HostCallbacks> = Arc::new(RefusingHost);
        let mut config = NativeLlmConfig {
            protocol: "openai".into(),
            alias: None,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test-key".into(),
            model: "racer-model".into(),
            ..Default::default()
        };
        config.custom_headers.clear();

        let multi = MultiLLM::new(vec![
            LlmProvider::native("racer-a", "", config.clone(), callbacks.clone()),
            LlmProvider::native("racer-b", "", config, callbacks),
        ]);

        let error = multi
            .first_past_the_post(empty_params())
            .await
            .expect_err("an unreachable base_url cannot win");
        let message = error.to_string();
        assert!(
            !message.contains("no LLM proxy"),
            "the race must not fall back to the host proxy: {message}"
        );
    }

    #[tokio::test]
    async fn a_single_native_racer_runs_without_spawning_a_race() {
        struct RefusingHost;
        impl HostCallbacks for RefusingHost {
            fn llm_chat(
                &self,
                _request: LlmChatRequest,
            ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
                Box::pin(async { Err("this host has no LLM proxy".to_string()) })
            }
            fn execute_tool(
                &self,
                _request: ToolExecuteRequest,
            ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
                Box::pin(async { Err("not used".to_string()) })
            }
            fn check_permission(
                &self,
                _request: PermissionCheckRequest,
            ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
                Box::pin(async { Ok(PermissionDecision::allow()) })
            }
        }
        let callbacks: Arc<dyn HostCallbacks> = Arc::new(RefusingHost);
        let config = NativeLlmConfig {
            protocol: "openai".into(),
            alias: None,
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "test-key".into(),
            model: "solo".into(),
            ..Default::default()
        };
        let multi = MultiLLM::new(vec![LlmProvider::native("solo", "", config, callbacks)]);
        let error = multi
            .first_past_the_post(empty_params())
            .await
            .expect_err("unreachable endpoint");
        assert!(
            !error.to_string().contains("no LLM proxy"),
            "a lone native racer must still use its own transport: {error}"
        );
    }

    async fn spawn_openai_tool_call_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Read\",\"arguments\":\"{}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}/v1")
    }

    fn native_provider(name: &str, base_url: &str) -> LlmProvider {
        struct NoHost;
        impl HostCallbacks for NoHost {
            fn llm_chat(
                &self,
                _request: crate::rpc::types::LlmChatRequest,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<crate::rpc::types::LlmChatResponse, String>,
            > {
                Box::pin(async { Err("this host has no LLM proxy".to_string()) })
            }

            fn execute_tool(
                &self,
                _request: crate::rpc::types::ToolExecuteRequest,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<crate::rpc::types::ToolExecuteResponse, String>,
            > {
                Box::pin(async { Err("not used by this test".into()) })
            }

            fn check_permission(
                &self,
                _request: crate::rpc::types::PermissionCheckRequest,
            ) -> crate::rpc::types::BoxFuture<
                'static,
                Result<crate::rpc::types::PermissionDecision, String>,
            > {
                Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
            }
        }
        LlmProvider::native(
            name,
            "",
            NativeLlmConfig {
                protocol: "openai".into(),
                alias: None,
                base_url: base_url.into(),
                api_key: "test-key".into(),
                model: "native-model".into(),
                ..Default::default()
            },
            Arc::new(NoHost),
        )
    }

    async fn spawn_openai_partial_server(content: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0u8; 64 * 1024];
            let _ = socket.read(&mut request).await;
            let body =
                format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_race_exposes_a_native_partial_response() {
        let multi = Arc::new(MultiLLM::new(vec![
            native_provider("a", &spawn_openai_partial_server("partial-a").await),
            native_provider("b", &spawn_openai_partial_server("partial-b").await),
        ]));
        let token = tokio_util::sync::CancellationToken::new();
        let mut request = empty_params();
        request.cancel = Some(token.clone());
        let chat_multi = multi.clone();
        let chat = tokio::spawn(async move { chat_multi.chat(request).await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        token.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), chat)
            .await
            .expect("the cancelled race resolves promptly")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("llm cancelled"), "{error}");
        let partial = multi
            .take_interrupted_response()
            .expect("the race exposes a cancelled native attempt");
        assert!(matches!(
            partial.content.as_str(),
            "partial-a" | "partial-b"
        ));
        assert!(multi.take_interrupted_response().is_none());
    }

    struct BlockedHost;

    impl HostCallbacks for BlockedHost {
        fn llm_chat(
            &self,
            _request: crate::rpc::types::LlmChatRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
        {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Err("blocked".to_string())
            })
        }

        fn execute_tool(
            &self,
            _request: crate::rpc::types::ToolExecuteRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async { Err("not used by this test".into()) })
        }

        fn check_permission(
            &self,
            _request: crate::rpc::types::PermissionCheckRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
        }
    }

    struct ToolCallHost;

    impl HostCallbacks for ToolCallHost {
        fn llm_chat(
            &self,
            _request: crate::rpc::types::LlmChatRequest,
        ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
        {
            Box::pin(async {
                Ok(crate::rpc::types::LlmChatResponse {
                    content: String::new(),
                    tool_calls: vec![crate::rpc::types::LlmToolCall {
                        id: "call_1".into(),
                        name: "Read".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                    }],
                    thinking: vec![],
                    finish_reason: Some("tool_calls".into()),
                    usage: crate::rpc::types::TokenUsage::default(),
                })
            })
        }

        fn execute_tool(
            &self,
            _request: crate::rpc::types::ToolExecuteRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async { Err("not used by this test".into()) })
        }

        fn check_permission(
            &self,
            _request: crate::rpc::types::PermissionCheckRequest,
        ) -> crate::rpc::types::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
        }
    }

    fn ledger_with_call_1_taken() -> Arc<kimi_core::tool_call_id::ToolCallIdNormalizer> {
        let ledger = Arc::new(kimi_core::tool_call_id::ToolCallIdNormalizer::new());
        let previous = ledger.begin_response();
        assert_eq!(
            previous.remap_finalized_ids(&["call_1".to_string()]),
            vec!["call_1"]
        );
        ledger
    }

    #[tokio::test]
    async fn test_a_native_only_path_normalizes_tool_call_ids() {
        let base_url = spawn_openai_tool_call_server().await;
        let multi = MultiLLM::new(vec![native_provider("native", &base_url)]);
        multi.set_tool_call_ids(ledger_with_call_1_taken());

        let response = multi.chat(empty_params()).await.unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            response.tool_calls[0].id, "call_1__2",
            "the native transport must receive the ledger and rewrite the replayed id"
        );
    }

    #[tokio::test]
    async fn test_a_mixed_race_normalizes_once_when_the_native_racer_wins() {
        let base_url = spawn_openai_tool_call_server().await;
        let multi = MultiLLM::new(vec![
            native_provider("native", &base_url),
            LlmProvider::host("blocked-proxy", "proxy-model", "", Arc::new(BlockedHost)),
        ]);
        multi.set_tool_call_ids(ledger_with_call_1_taken());

        let response = multi.chat(empty_params()).await.unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            response.tool_calls[0].id, "call_1__2",
            "a native winner must be normalized exactly once, not suffix-nested"
        );
    }

    #[tokio::test]
    async fn test_a_mixed_race_normalizes_the_proxy_winner() {
        let multi = MultiLLM::new(vec![
            native_provider("native", "http://127.0.0.1:1/v1"),
            LlmProvider::host("proxy", "proxy-model", "", Arc::new(ToolCallHost)),
        ]);
        multi.set_tool_call_ids(ledger_with_call_1_taken());

        let response = multi.chat(empty_params()).await.unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            response.tool_calls[0].id, "call_1__2",
            "a proxy winner must be normalized by the racing layer"
        );
    }

    #[tokio::test]
    async fn test_race_all_fail_reports_every_error() {
        let f1 = tokio::spawn(async {
            (
                "f1-id".to_string(),
                false,
                ProviderResult {
                    provider_name: "f1".into(),
                    result: Err("e1".into()),
                    elapsed_ms: 1,
                },
                None,
            )
        });
        let f2 = tokio::spawn(async {
            (
                "f2-id".to_string(),
                false,
                ProviderResult {
                    provider_name: "f2".into(),
                    result: Err("e2".into()),
                    elapsed_ms: 1,
                },
                None,
            )
        });
        let err = race_first_success(vec![f1, f2], &HashMap::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("f1: e1"), "missing f1 error: {err}");
        assert!(err.contains("f2: e2"), "missing f2 error: {err}");
    }

    #[tokio::test]
    async fn test_call_count() {
        let p1 = MockTestLlm::new("p1", 10, false);
        let p2 = MockTestLlm::new("p2", 10, false);

        let params = LLMChatParams {
            cancel: None,
            messages: Arc::from(Vec::new()),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        };

        let (r1, r2) = tokio::join!(p1.chat(params.clone()), p2.chat(params));
        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }

    #[test]
    fn test_multi_llm_label_construction() {
        let callbacks = recording_callbacks();
        let p1 = LlmProvider::host("p1", "kimi-k2", "sys", callbacks.clone());
        let m1 = MultiLLM::new(vec![p1]);
        assert_eq!(m1.label, "kimi-k2");
        assert_eq!(m1.provider_count(), 1);

        let p1_again = LlmProvider::host("p1", "kimi-k2", "sys", callbacks.clone());
        let p2 = LlmProvider::host("p2", "claude-3-5", "sys", callbacks);
        let m2 = MultiLLM::new(vec![p1_again, p2]);
        assert_eq!(m2.label, "kimi-k2 + 1 others");
        assert_eq!(m2.provider_count(), 2);
    }

    fn recording_callbacks() -> Arc<dyn HostCallbacks> {
        struct RecordingHost {
            events: std::sync::Mutex<Vec<serde_json::Value>>,
        }
        impl HostCallbacks for RecordingHost {
            fn llm_chat(
                &self,
                _request: LlmChatRequest,
            ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
                Box::pin(async move {
                    Ok(LlmChatResponse {
                        content: String::new(),
                        tool_calls: vec![],
                        thinking: vec![],
                        finish_reason: Some("recording".into()),
                        usage: TokenUsage::default(),
                    })
                })
            }
            fn execute_tool(
                &self,
                _request: ToolExecuteRequest,
            ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
                Box::pin(async {
                    Ok(ToolExecuteResponse {
                        delivery: None,
                        stop_turn: false,
                        content: String::new(),
                        is_error: false,
                        note: None,
                    })
                })
            }
            fn check_permission(
                &self,
                _request: PermissionCheckRequest,
            ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
                Box::pin(async { Ok(PermissionDecision::allow()) })
            }
            fn emit_event(&self, event: serde_json::Value) {
                self.events.lock().unwrap().push(event);
            }
        }
        Arc::new(RecordingHost {
            events: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn test_provider(name: &str) -> LlmProvider {
        LlmProvider::host(
            name,
            format!("{name}-model"),
            "system",
            recording_callbacks(),
        )
    }

    fn empty_params() -> LLMChatParams {
        LLMChatParams {
            cancel: None,
            messages: Arc::from(Vec::new()),
            tools: Arc::from(Vec::new()),
            ..Default::default()
        }
    }

    #[test]
    fn multi_provider_count_reflects_input() {
        assert_eq!(MultiLLM::new(vec![]).provider_count(), 0);
        assert_eq!(MultiLLM::new(vec![test_provider("a")]).provider_count(), 1);
        assert_eq!(
            MultiLLM::new(vec![test_provider("a"), test_provider("b")]).provider_count(),
            2
        );
    }

    #[test]
    fn multi_label_for_zero_one_and_many_providers() {
        let zero = MultiLLM::new(vec![]);
        assert_eq!(zero.model_name(), "");

        let one = MultiLLM::new(vec![test_provider("a")]);
        assert_eq!(one.model_name(), "a-model");

        let many = MultiLLM::new(vec![
            test_provider("a"),
            test_provider("b"),
            test_provider("c"),
        ]);
        assert_eq!(many.model_name(), "a-model + 2 others");
    }

    #[test]
    fn multi_system_prompt_uses_first_provider() {
        let mut p1 = test_provider("a");
        p1.system_prompt = "first-prompt".to_string();
        let p2 = test_provider("b");
        let m = MultiLLM::new(vec![p1, p2]);
        assert_eq!(m.system_prompt(), "first-prompt");
    }

    #[test]
    fn multi_system_prompt_empty_when_no_providers() {
        let m = MultiLLM::new(vec![]);
        assert_eq!(m.system_prompt(), "");
    }

    #[test]
    fn multi_is_retryable_error_consults_first_provider() {
        let p1 = test_provider("a");
        let p2 = test_provider("b");
        let m = MultiLLM::new(vec![p1, p2]);
        assert!(!m.is_retryable_error("anything"));

        let empty = MultiLLM::new(vec![]);
        assert!(!empty.is_retryable_error("anything"));
    }

    #[tokio::test]
    async fn multi_empty_providers_first_past_the_post_errors() {
        let m = MultiLLM::new(vec![]);
        let err = m.first_past_the_post(empty_params()).await.unwrap_err();
        assert!(
            err.to_string().contains("No LLM providers"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn multi_empty_providers_all_results_returns_empty() {
        let m = MultiLLM::new(vec![]);
        let results = m.all_results(empty_params()).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn multi_single_provider_short_circuits_race() {
        let p1 = test_provider("only");
        let m = MultiLLM::new(vec![p1]);
        let result = m.first_past_the_post(empty_params()).await.unwrap();
        assert_eq!(result.finish_reason.as_deref(), Some("recording"));
    }

    #[tokio::test]
    async fn multi_all_results_returns_every_provider_outcome() {
        let p1 = test_provider("a");
        let p2 = test_provider("b");
        let m = MultiLLM::new(vec![p1, p2]);
        let results = m.all_results(empty_params()).await;
        assert_eq!(results.len(), 2);
        let names: std::collections::HashSet<_> =
            results.iter().map(|r| r.provider_name.as_str()).collect();
        assert!(names.contains("a"));
        assert!(names.contains("b"));
        for r in &results {
            assert!(
                r.result.is_ok(),
                "recording host should always succeed: {r:?}"
            );
        }
    }

    #[tokio::test]
    async fn multi_race_sends_cancel_to_loser_callbacks() {
        struct CancelRecordingHost {
            cancelled: Arc<std::sync::Mutex<Vec<String>>>,
            delay_ms: u64,
            succeed: bool,
        }
        impl HostCallbacks for CancelRecordingHost {
            fn llm_chat(
                &self,
                _request: LlmChatRequest,
            ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
                let delay = self.delay_ms;
                let succeed = self.succeed;
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    if succeed {
                        Ok(LlmChatResponse {
                            content: String::new(),
                            tool_calls: vec![],
                            thinking: vec![],
                            finish_reason: Some("winner".into()),
                            usage: TokenUsage::default(),
                        })
                    } else {
                        Err("loser error".into())
                    }
                })
            }
            fn execute_tool(
                &self,
                _request: ToolExecuteRequest,
            ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
                Box::pin(async {
                    Ok(ToolExecuteResponse {
                        delivery: None,
                        stop_turn: false,
                        content: String::new(),
                        is_error: false,
                        note: None,
                    })
                })
            }
            fn check_permission(
                &self,
                _request: PermissionCheckRequest,
            ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
                Box::pin(async { Ok(PermissionDecision::allow()) })
            }
            fn emit_event(&self, _event: serde_json::Value) {}
            fn cancel_llm_chat(&self, request_id: &str) {
                self.cancelled.lock().unwrap().push(request_id.to_string());
            }
        }
        let cancel_log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let host_a = Arc::new(CancelRecordingHost {
            cancelled: cancel_log.clone(),
            delay_ms: 5,
            succeed: false,
        });
        let host_b = Arc::new(CancelRecordingHost {
            cancelled: cancel_log.clone(),
            delay_ms: 30,
            succeed: false,
        });
        let p1 = LlmProvider::host("a", "a-m", "", host_a);
        let p2 = LlmProvider::host("b", "b-m", "", host_b);
        let m = MultiLLM::new(vec![p1, p2]);
        let err = m
            .first_past_the_post(empty_params())
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("a"), "missing a in: {err}");
        assert!(err.contains("b"), "missing b in: {err}");

        let cancelled = cancel_log.lock().unwrap();
        assert!(
            cancelled.is_empty(),
            "no provider should be cancelled when all error: {cancelled:?}"
        );
    }
}
