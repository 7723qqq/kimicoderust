use std::sync::Arc;

use crate::agent::profile::profile::ProfileModelContext;
use crate::turn_loop::types::LLMMessage;

pub use crate::compaction::*;

use super::types::CompactionSource;

pub const DEFAULT_BLOCK_RATIO: f64 = 0.85;
pub const DEFAULT_MAX_COMPACTION_PER_TURN: u32 = u32::MAX;
pub const DEFAULT_MIN_OVERFLOW_REDUCTION_RATIO: f64 = 0.05;
pub const DEFAULT_COMPACTION_CONFIG: CompactionConfig = CompactionConfig {
    max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
    trigger_ratio: 0.85,
    reserved_context_size: 50_000,
    max_recent_messages: 4,
    max_recent_user_messages: u32::MAX,
    max_recent_size_ratio: 0.2,
    max_attempts: None,
    max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
};

pub trait CompactionStrategy: Send + Sync {
    fn should_compact(&self, used_size: u32) -> bool;
    fn should_block(&self, used_size: u32) -> bool;
    fn compute_compact_count(&self, messages: &[LLMMessage], source: CompactionSource) -> u32;
    fn reduce_compact_on_overflow(&self, messages: &[LLMMessage]) -> u32;
    fn check_after_step(&self) -> bool;
    fn max_compaction_per_turn(&self) -> u32;
    fn max_overflow_compaction_attempts(&self) -> u32;
}

type EstimateMessage = Arc<dyn Fn(&LLMMessage) -> u32 + Send + Sync>;
type MaxSizeProvider = Arc<dyn Fn() -> u32 + Send + Sync>;

pub struct DefaultCompactionStrategy {
    max_size_provider: MaxSizeProvider,
    config: CompactionConfig,
    estimate_message: EstimateMessage,
    block_ratio: f64,
    max_compaction_per_turn: u32,
    min_overflow_reduction_ratio: f64,
}

impl DefaultCompactionStrategy {
    pub fn new(max_size_provider: MaxSizeProvider, config: CompactionConfig) -> Self {
        Self {
            max_size_provider,
            config,
            estimate_message: Arc::new(estimate_message_tokens),
            block_ratio: DEFAULT_BLOCK_RATIO,
            max_compaction_per_turn: DEFAULT_MAX_COMPACTION_PER_TURN,
            min_overflow_reduction_ratio: DEFAULT_MIN_OVERFLOW_REDUCTION_RATIO,
        }
    }

    pub fn with_estimate_message(mut self, estimate_message: EstimateMessage) -> Self {
        self.estimate_message = estimate_message;
        self
    }

    pub fn with_block_ratio(mut self, block_ratio: f64) -> Self {
        self.block_ratio = block_ratio;
        self
    }

    pub fn with_max_compaction_per_turn(mut self, max_compaction_per_turn: u32) -> Self {
        self.max_compaction_per_turn = max_compaction_per_turn;
        self
    }

    pub fn with_min_overflow_reduction_ratio(mut self, ratio: f64) -> Self {
        self.min_overflow_reduction_ratio = ratio;
        self
    }

    fn max_size(&self) -> u32 {
        (self.max_size_provider)()
    }

    fn fit_compact_count_to_window(&self, messages: &[LLMMessage], compacted_count: u32) -> u32 {
        if self.max_size() == 0 || compacted_count == 0 {
            return compacted_count;
        }
        let mut compacted_size = messages
            .iter()
            .take(compacted_count as usize)
            .fold(0u32, |total, message| {
                total.saturating_add((self.estimate_message)(message))
            });
        if compacted_size <= self.max_size() {
            return compacted_count;
        }
        let mut best = None;
        for count in (1..compacted_count as usize).rev() {
            compacted_size = compacted_size
                .saturating_sub((self.estimate_message)(&messages[count]));
            if !crate::compaction::can_split_after(messages, count - 1) {
                continue;
            }
            best = Some(count as u32);
            if compacted_size <= self.max_size() {
                return count as u32;
            }
        }
        best.unwrap_or(compacted_count)
    }

    fn should_use_reserved_context(&self, used_size: u32) -> bool {
        let max_size = self.max_size();
        let reserved = self.config.reserved_context_size;
        reserved > 0 && reserved < max_size && used_size.saturating_add(reserved) >= max_size
    }
}

impl CompactionStrategy for DefaultCompactionStrategy {
    fn should_compact(&self, used_size: u32) -> bool {
        let max_size = self.max_size();
        if max_size == 0 {
            return false;
        }
        used_size as f64 >= max_size as f64 * self.config.trigger_ratio
            || self.should_use_reserved_context(used_size)
    }

    fn should_block(&self, used_size: u32) -> bool {
        let max_size = self.max_size();
        if max_size == 0 {
            return false;
        }
        used_size as f64 >= max_size as f64 * self.block_ratio
            || self.should_use_reserved_context(used_size)
    }

    fn compute_compact_count(&self, messages: &[LLMMessage], source: CompactionSource) -> u32 {
        if source == CompactionSource::Manual {
            for index in (1..messages.len()).rev() {
                if crate::compaction::can_split_after(messages, index) {
                    return self.fit_compact_count_to_window(messages, (index + 1) as u32);
                }
            }
            return 0;
        }
        let mut recent_messages = 1usize;
        let mut recent_user_messages = 0u32;
        let mut recent_size = 0u32;
        let mut best = None;
        while recent_messages < messages.len() {
            let split_index = messages.len() - recent_messages - 1;
            let message = &messages[messages.len() - recent_messages];
            if message.role == "user" {
                recent_user_messages += 1;
            }
            recent_size = recent_size.saturating_add((self.estimate_message)(message));
            if crate::compaction::can_split_after(messages, split_index) {
                best = Some((split_index + 1) as u32);
            }
            let reaches_max = recent_messages as u32 >= self.config.max_recent_messages
                || recent_user_messages >= self.config.max_recent_user_messages
                || recent_size as f64
                    >= self.max_size() as f64 * self.config.max_recent_size_ratio;
            if reaches_max && best.is_some() {
                break;
            }
            recent_messages += 1;
        }
        self.fit_compact_count_to_window(messages, best.unwrap_or(0))
    }

    fn reduce_compact_on_overflow(&self, messages: &[LLMMessage]) -> u32 {
        if messages.len() <= 2 {
            return messages.len() as u32;
        }
        let min_reduced_size = ((self.max_size() as f64 * self.min_overflow_reduction_ratio).ceil()
            as u32)
            .max(1);
        let mut reduced_size = 0u32;
        let mut best = None;
        for index in (1..messages.len() - 1).rev() {
            reduced_size = reduced_size.saturating_add((self.estimate_message)(&messages[index + 1]));
            if can_split_after(messages, index) {
                best = Some((index + 1) as u32);
                if reduced_size >= min_reduced_size {
                    return (index + 1) as u32;
                }
            }
        }
        best.unwrap_or(messages.len() as u32)
    }

    fn check_after_step(&self) -> bool {
        self.config.trigger_ratio != self.block_ratio
    }

    fn max_compaction_per_turn(&self) -> u32 {
        self.max_compaction_per_turn
    }

    fn max_overflow_compaction_attempts(&self) -> u32 {
        self.config.max_overflow_compaction_attempts
    }
}

pub struct RuntimeCompactionStrategy {
    context: Arc<dyn Fn() -> ProfileModelContext + Send + Sync>,
    estimate_message: EstimateMessage,
}

impl RuntimeCompactionStrategy {
    pub fn new(
        context: Arc<dyn Fn() -> ProfileModelContext + Send + Sync>,
        estimate_message: Option<EstimateMessage>,
    ) -> Self {
        Self {
            context,
            estimate_message: estimate_message.unwrap_or_else(|| Arc::new(estimate_message_tokens)),
        }
    }

    fn config(model: &ProfileModelContext) -> CompactionConfig {
        CompactionConfig {
            trigger_ratio: model
                .compaction_trigger_ratio
                .unwrap_or(DEFAULT_COMPACTION_CONFIG.trigger_ratio),
            reserved_context_size: model
                .reserved_context_size
                .unwrap_or(DEFAULT_COMPACTION_CONFIG.reserved_context_size),
            ..DEFAULT_COMPACTION_CONFIG
        }
    }

    fn delegate(&self) -> DefaultCompactionStrategy {
        let model = (self.context)();
        let max_size = model
            .model_capabilities
            .max_input_tokens
            .unwrap_or(model.model_capabilities.max_context_tokens);
        let config = Self::config(&model);
        DefaultCompactionStrategy::new(
            Arc::new(move || max_size),
            CompactionConfig {
                max_context_tokens: max_size,
                ..config
            },
        )
        .with_estimate_message(self.estimate_message.clone())
        .with_block_ratio(config.trigger_ratio.max(DEFAULT_BLOCK_RATIO))
    }

    fn window_delegate(&self) -> DefaultCompactionStrategy {
        let context = self.context.clone();
        let max_size = Arc::new(move || {
            let max_input_tokens = context().model_capabilities.max_input_tokens;
            max_input_tokens.unwrap_or_else(|| context().model_capabilities.max_context_tokens)
        });
        DefaultCompactionStrategy::new(max_size, DEFAULT_COMPACTION_CONFIG)
            .with_estimate_message(self.estimate_message.clone())
    }
}

impl CompactionStrategy for RuntimeCompactionStrategy {
    fn should_compact(&self, used_size: u32) -> bool {
        self.delegate().should_compact(used_size)
    }

    fn should_block(&self, used_size: u32) -> bool {
        self.delegate().should_block(used_size)
    }

    fn compute_compact_count(&self, messages: &[LLMMessage], source: CompactionSource) -> u32 {
        self.window_delegate().compute_compact_count(messages, source)
    }

    fn reduce_compact_on_overflow(&self, messages: &[LLMMessage]) -> u32 {
        self.window_delegate().reduce_compact_on_overflow(messages)
    }

    fn check_after_step(&self) -> bool {
        Self::config(&(self.context)()).trigger_ratio
            != Self::config(&(self.context)())
                .trigger_ratio
                .max(DEFAULT_BLOCK_RATIO)
    }

    fn max_compaction_per_turn(&self) -> u32 {
        DEFAULT_MAX_COMPACTION_PER_TURN
    }

    fn max_overflow_compaction_attempts(&self) -> u32 {
        DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS
    }
}
