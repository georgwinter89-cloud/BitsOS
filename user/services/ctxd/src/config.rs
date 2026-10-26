//! Settings under `context.*` (DESIGN §1, desk decision 6a). The prototype reads them from a TOML file
//! (`$XDG_CONFIG_HOME/ctxd/config.toml` or `--config`); on V2 they come from `settingsd`.
//! Without a config file the only root is the synthetic test set `host/cx-eval/docs`, so ctxd never
//! crawls Georg's home unless a root is configured.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub context: Context,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Context {
    pub roots: Vec<RootCfg>,
    /// Extra roots that are always refused (R8a adds the Windows disk rules on top).
    pub forbidden_roots: Vec<PathBuf>,
    /// Exclude patterns, matched against every path component (`*` and `?` wildcards).
    pub excludes: Vec<String>,
    pub data: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub index: IndexCfg,
    pub rank: RankCfg,
    pub claude: ClaudeCfg,
    pub embedder: EmbedderCfg,
    pub mcp: McpCfg,
    pub search: SearchCfg,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RootCfg {
    pub path: PathBuf,
    #[serde(default)]
    pub private: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IndexCfg {
    /// Extraction workers (child processes); the embedder has its own `embedder.threads`.
    pub threads: usize,
    pub max_file_mb: u64,
    pub extract_seconds: u64,
    pub extract_mem_mb: u64,
    /// Chunk window and overlap in bytes of UTF-8 text (about 256 / 48 tokens of the e5 tokenizer).
    pub chunk_bytes: usize,
    pub chunk_overlap: usize,
    /// Speed switch, not a limit: below this many chunks the vector search is an exact scan.
    pub hnsw_min_chunks: usize,
    pub rescan_seconds: u64,
    /// inotify on the roots wakes the indexer after a change (the rescan stays as the safety net).
    pub watch: bool,
    /// File events within this time are gathered into one indexer run.
    pub watch_debounce_ms: u64,
    /// Chunks per embedder request.
    pub embed_batch: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RankCfg {
    pub w_v: f32,
    pub w_b: f32,
    pub w_c: f32,
    pub w_r: f32,
    pub w_x: f32,
    pub tau: f32,
    pub t_half_days: f32,
    pub v_min: f32,
    pub b_min: f32,
    pub candidates: usize,
    /// "linear" (the DESIGN §7.1 formula) or "rrf" (reciprocal rank fusion, kept for comparison).
    pub fusion: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ClaudeCfg {
    /// Snippet length for MCP results in characters; 0 turns snippets off (Georg 15.10.: about two sentences).
    pub snippet_chars: usize,
    /// `context_forget` is `delete_overwrite` (B3 default `ask`); without a screen to ask on, MCP calls are
    /// refused unless Georg sets this.
    pub allow_forget: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbedderCfg {
    /// "llama" (llama-server child process), "fake" (hashing, for tests), "none" (keyword only).
    pub kind: String,
    pub model: Option<PathBuf>,
    pub server: Option<PathBuf>,
    pub threads: usize,
    /// Parallel sequences in llama-server; measured 15.10.: 2 slots are as fast as 8 and use half the memory.
    pub slots: usize,
    pub batch_timeout_ms: u64,
    pub startup_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct McpCfg {
    pub enabled: bool,
    pub port: u16,
    /// Optional bearer token; the listener is on 127.0.0.1 only either way.
    pub token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchCfg {
    pub deadline_ms: u64,
    pub mcp_deadline_ms: u64,
    pub desktop_snippet_chars: usize,
}

pub fn default_excludes() -> Vec<String> {
    [
        ".ssh",
        ".gnupg",
        "*.kdbx",
        "*.pem",
        "*.key",
        "node_modules",
        ".git",
        "*.tmp",
        ".mozilla",
        "BraveSoftware",
        "google-chrome",
        "chromium",
        ".cache",
        ".Trash-*",
        "lost+found",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The synthetic German test set (CX-0), the default root of the prototype.
pub fn eval_docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/cx-eval/docs")
}

pub fn eval_topics_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/cx-eval/topics.json")
}

/// Downloads live under `third_party/` (gitignored).
pub fn third_party_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party")
}

impl Default for Context {
    fn default() -> Self {
        Context {
            roots: vec![RootCfg { path: eval_docs_dir(), private: false }],
            forbidden_roots: Vec::new(),
            excludes: default_excludes(),
            data: None,
            socket: None,
            index: IndexCfg::default(),
            rank: RankCfg::default(),
            claude: ClaudeCfg::default(),
            embedder: EmbedderCfg::default(),
            mcp: McpCfg::default(),
            search: SearchCfg::default(),
        }
    }
}

impl Default for IndexCfg {
    fn default() -> Self {
        IndexCfg {
            threads: 8,
            max_file_mb: 64,
            extract_seconds: 20,
            extract_mem_mb: 2048,
            chunk_bytes: 1000,
            chunk_overlap: 190,
            hnsw_min_chunks: 20_000,
            rescan_seconds: 60,
            watch: true,
            watch_debounce_ms: 1000,
            embed_batch: 32,
        }
    }
}

impl Default for RankCfg {
    fn default() -> Self {
        RankCfg {
            w_v: 0.45,
            w_b: 0.30,
            w_c: 0.15,
            w_r: 0.10,
            w_x: 0.60,
            tau: 2.0,
            t_half_days: 180.0,
            v_min: 0.35,
            b_min: 0.20,
            candidates: 100,
            fusion: "linear".into(),
        }
    }
}

impl Default for ClaudeCfg {
    fn default() -> Self {
        ClaudeCfg { snippet_chars: 200, allow_forget: false }
    }
}

impl Default for EmbedderCfg {
    fn default() -> Self {
        EmbedderCfg {
            kind: "llama".into(),
            model: None,
            server: None,
            threads: 8,
            slots: 2,
            batch_timeout_ms: 60_000,
            startup_seconds: 120,
        }
    }
}

impl Default for McpCfg {
    fn default() -> Self {
        McpCfg { enabled: true, port: 29510, token: None }
    }
}

impl Default for SearchCfg {
    fn default() -> Self {
        SearchCfg { deadline_ms: 300, mcp_deadline_ms: 3000, desktop_snippet_chars: 400 }
    }
}

impl EmbedderCfg {
    pub fn model_path(&self) -> PathBuf {
        self.model.clone().unwrap_or_else(|| third_party_dir().join("models/multilingual-e5-small-q8_0.gguf"))
    }
    pub fn server_path(&self) -> PathBuf {
        self.server.clone().unwrap_or_else(|| third_party_dir().join("build/llama.cpp/bin/llama-server"))
    }
}

fn xdg(var: &str, fallback: &str) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback))
}

pub fn default_config_path() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config").map(|p| p.join("ctxd/config.toml"))
}

impl Config {
    /// Explicit path: must exist and parse. Default path: used when present, else defaults.
    pub fn load(explicit: Option<&Path>) -> Result<Config, String> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => match default_config_path() {
                Some(p) if p.exists() => p,
                _ => return Ok(Config::default()),
            },
        };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("config {}: {e}", path.display()))?;
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| format!("config: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        let c = &self.context;
        if c.index.chunk_bytes < 64 || c.index.chunk_overlap >= c.index.chunk_bytes {
            return Err("config: context.index.chunk_bytes must be >= 64 and > chunk_overlap".into());
        }
        if !["llama", "fake", "none"].contains(&c.embedder.kind.as_str()) {
            return Err(format!("config: context.embedder.kind {:?} unknown", c.embedder.kind));
        }
        if !["linear", "rrf"].contains(&c.rank.fusion.as_str()) {
            return Err(format!("config: context.rank.fusion {:?} unknown", c.rank.fusion));
        }
        Ok(())
    }

    pub fn data_dir(&self, explicit: Option<&Path>) -> PathBuf {
        explicit
            .map(Path::to_path_buf)
            .or_else(|| self.context.data.clone())
            .or_else(|| xdg("XDG_DATA_HOME", ".local/share").map(|p| p.join("ctxd")))
            .unwrap_or_else(|| PathBuf::from("ctxd-data"))
    }

    pub fn socket_path(&self, data: &Path) -> PathBuf {
        if let Some(s) = &self.context.socket {
            return s.clone();
        }
        match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(r) => PathBuf::from(r).join("ctxd.sock"),
            None => data.join("ctxd.sock"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_test_set_root() {
        let c = Config::default();
        assert_eq!(c.context.roots.len(), 1);
        assert!(c.context.roots[0].path.ends_with("host/cx-eval/docs"));
        assert_eq!(c.context.claude.snippet_chars, 200);
        assert_eq!(c.context.mcp.port, 29510);
    }

    #[test]
    fn parses_context_namespace() {
        let c = Config::parse(
            r#"
            [context]
            roots = [{ path = "/tmp/a" }, { path = "/tmp/b", private = true }]
            [context.claude]
            snippet_chars = 120
            [context.rank]
            w_v = 0.5
            "#,
        )
        .unwrap();
        assert_eq!(c.context.roots[1], RootCfg { path: "/tmp/b".into(), private: true });
        assert_eq!(c.context.claude.snippet_chars, 120);
        assert_eq!(c.context.rank.w_v, 0.5);
        assert_eq!(c.context.rank.w_b, 0.30);
    }

    #[test]
    fn example_config_parses() {
        let t = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml")).unwrap();
        let c = Config::parse(&t).unwrap();
        assert_eq!(c.context.roots.len(), 2);
        assert!(c.context.roots[1].private);
        assert_eq!(c.context.embedder.slots, 2);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("[context.embedder]\nkind = \"cloud\"").is_err());
        assert!(Config::parse("[context.index]\nchunk_bytes = 10").is_err());
        assert!(Config::parse("not toml = = =").is_err());
    }
}
