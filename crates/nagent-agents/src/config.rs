//! Re-export of the plain per-agent `*Config` structs at the crate
//! root. The TOML / env parsing for each config lives in
//! `nagent-server`'s `config::agents.rs` because the toml-mirror
//! structs (`TomlWebFetchConfig`, …) are part of the server's
//! `ConfigFile`/`Config` surface, not of this crate.

pub use crate::agents::config_doc::{
    CalDavAgentConfig, CalculateAgentConfig, DateTimeAgentConfig, DictionaryAgentConfig,
    MemoryAgentConfig, ReadDocumentAgentConfig, StockAgentConfig, UnitConvertAgentConfig,
    WeatherAgentConfig, WebFetchAgentConfig, WikipediaAgentConfig, XTimelineAgentConfig,
};

/// Bag of every per-agent config. Plain data only — the server
/// wraps it with its `from_env_with_toml` parser.
#[derive(Debug, Clone, Default)]
pub struct AgentConfigs {
    pub web_fetch: WebFetchAgentConfig,
    pub weather: WeatherAgentConfig,
    pub unit_convert: UnitConvertAgentConfig,
    pub wikipedia: WikipediaAgentConfig,
    pub dictionary: DictionaryAgentConfig,
    pub stock: StockAgentConfig,
    pub calculate: CalculateAgentConfig,
    pub datetime: DateTimeAgentConfig,
    pub read_document: ReadDocumentAgentConfig,
    pub caldav: CalDavAgentConfig,
    pub x_timeline: XTimelineAgentConfig,
    pub memory: MemoryAgentConfig,
}
