# llama-server Backend Support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Herd route requests to llama-server (llama.cpp) backends alongside Ollama, with backend-aware health checks, model discovery, and node registration.

**Architecture:** Add a `BackendType` enum (`Ollama` / `LlamaServer`) threaded through config, node registration, health checking, model discovery, and model warming. Both static YAML backends and registered SQLite nodes gain backend-type awareness. The router itself stays unchanged — it routes to OpenAI-compatible HTTP endpoints regardless of backend type.

**Tech Stack:** Rust, serde, rusqlite, reqwest, axum, tokio

**Scope:** Herd router changes only (Part A). herd-tune script changes (GPU detection, binary download) are deferred to a follow-up.

---

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `src/config.rs` | Add `BackendType` enum + `backend` field to `Backend` struct |
| Modify | `src/nodes/types.rs` | Add backend fields to `NodeRegistration` and `Node` structs |
| Modify | `src/nodes/db.rs` | SQLite schema migration + read/write new columns |
| Modify | `src/nodes/health.rs` | Backend-aware health polling (Ollama vs llama-server paths) |
| Modify | `src/backend/health.rs` | Smart defaults for health check path based on backend type |
| Modify | `src/backend/discovery.rs` | Backend-aware model discovery (`/api/tags` vs `/v1/models`) |
| Modify | `src/backend/warmer.rs` | Skip warming for llama-server backends |
| Modify | `src/server.rs` | `inject_keep_alive` only for Ollama backends |
| Modify | `herd.yaml.example` | Document `backend` field on static backends |

---

## Task 1: Add `BackendType` enum and `backend` field to config

**Files:**
- Modify: `src/config.rs:101-178` (after `RoutingStrategy` enum, and in `Backend` struct)

- [ ] **Step 1: Write failing test — BackendType deserialization**

Add to the `#[cfg(test)] mod tests` block at bottom of `src/config.rs`:

```rust
#[test]
fn backend_type_defaults_to_ollama() {
    let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n";
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.backends[0].backend, BackendType::Ollama);
}

#[test]
fn backend_type_llama_server_parses() {
    let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n    backend: llama-server\n";
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.backends[0].backend, BackendType::LlamaServer);
}

#[test]
fn backend_type_ollama_explicit() {
    let yaml = "backends:\n  - name: x\n    url: http://x\n    priority: 50\n    backend: ollama\n";
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.backends[0].backend, BackendType::Ollama);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::tests::backend_type -v`
Expected: FAIL — `BackendType` not found

- [ ] **Step 3: Implement BackendType enum and add to Backend struct**

Add the `BackendType` enum after the `RoutingStrategy` impl block (after line 125 in `src/config.rs`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BackendType {
    #[default]
    #[serde(rename = "ollama")]
    Ollama,

    #[serde(rename = "llama-server")]
    LlamaServer,
}

impl std::fmt::Display for BackendType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendType::Ollama => write!(f, "ollama"),
            BackendType::LlamaServer => write!(f, "llama-server"),
        }
    }
}
```

Add the `backend` field to the `Backend` struct (after the `url` field):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    pub name: String,
    pub url: String,

    #[serde(default)]
    pub backend: BackendType,

    pub priority: u32,
    // ... rest unchanged
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config::tests -v`
Expected: All config tests PASS (including existing ones — `backend` defaults to `Ollama`)

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add BackendType enum to config (ollama | llama-server)"
```

---

## Task 2: Add backend fields to node registration types

**Files:**
- Modify: `src/nodes/types.rs`

- [ ] **Step 1: Write failing test — NodeRegistration deserializes new fields**

Create test at the bottom of `src/nodes/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_registration_defaults_to_ollama_backend() {
        let json = r#"{
            "hostname": "test",
            "ollama_url": "http://test:11434"
        }"#;
        let reg: NodeRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.backend, crate::config::BackendType::Ollama);
        assert!(reg.backend_url.is_none());
    }

    #[test]
    fn node_registration_llama_server_fields() {
        let json = r#"{
            "hostname": "citadel",
            "backend": "llama-server",
            "backend_url": "http://citadel:8090",
            "gpu_vendor": "nvidia",
            "gpu_model": "NVIDIA GeForce RTX 5090",
            "gpu_backend": "cuda",
            "cuda_version": "13.1",
            "vram_mb": 32768,
            "ram_mb": 131072,
            "models_loaded": ["gemma-4-26B.gguf"],
            "capabilities": ["cuda", "flash_attn"]
        }"#;
        let reg: NodeRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.backend, crate::config::BackendType::LlamaServer);
        assert_eq!(reg.backend_url.as_deref(), Some("http://citadel:8090"));
        assert_eq!(reg.gpu_vendor.as_deref(), Some("nvidia"));
        assert_eq!(reg.capabilities, vec!["cuda", "flash_attn"]);
    }

    #[test]
    fn node_registration_backward_compat_ollama() {
        // Old herd-tune scripts only send ollama_url, no backend field
        let json = r#"{
            "hostname": "minipc",
            "ollama_url": "http://minipc:11434",
            "ollama_version": "0.16.1",
            "models_available": 42,
            "models_loaded": ["qwen3:32b"]
        }"#;
        let reg: NodeRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.backend, crate::config::BackendType::Ollama);
        assert_eq!(reg.ollama_url, "http://minipc:11434");
        assert!(reg.backend_url.is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib nodes::types::tests -v`
Expected: FAIL — missing fields on `NodeRegistration`

- [ ] **Step 3: Add new fields to NodeRegistration and Node**

Update `NodeRegistration` in `src/nodes/types.rs`:

```rust
use crate::config::BackendType;

/// Registration payload from herd-tune scripts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistration {
    pub hostname: String,

    /// Legacy field — used by Ollama backends. For llama-server, use backend_url.
    #[serde(default)]
    pub ollama_url: String,

    /// Preferred URL field for all backend types. Falls back to ollama_url if not set.
    #[serde(default)]
    pub backend_url: Option<String>,

    /// Backend type: "ollama" (default) or "llama-server"
    #[serde(default)]
    pub backend: BackendType,

    /// Stable machine identifier (preferred over hostname for upsert).
    #[serde(default)]
    pub node_id: Option<String>,

    #[serde(default)]
    pub gpu: Option<String>,

    /// GPU vendor: "nvidia", "amd", "intel", "none"
    #[serde(default)]
    pub gpu_vendor: Option<String>,

    /// GPU model name (e.g., "NVIDIA GeForce RTX 5090")
    #[serde(default)]
    pub gpu_model: Option<String>,

    /// GPU compute backend: "cuda", "rocm", "sycl", "vulkan", "cpu"
    #[serde(default)]
    pub gpu_backend: Option<String>,

    /// CUDA version (NVIDIA only, e.g., "13.1")
    #[serde(default)]
    pub cuda_version: Option<String>,

    #[serde(default)]
    pub vram_mb: u32,
    #[serde(default)]
    pub ram_mb: u32,
    #[serde(default)]
    pub ollama_version: Option<String>,

    /// llama.cpp build version (e.g., "b8678")
    #[serde(default)]
    pub backend_version: Option<String>,

    #[serde(default)]
    pub models_available: u32,
    #[serde(default)]
    pub models_loaded: Vec<String>,

    /// GGUF file paths (llama-server only)
    #[serde(default)]
    pub model_paths: Vec<String>,

    /// Capabilities: ["cuda", "flash_attn", "moe", etc.]
    #[serde(default)]
    pub capabilities: Vec<String>,

    #[serde(default)]
    pub recommended_config: serde_json::Value,
    #[serde(default)]
    pub config_applied: bool,
    #[serde(default)]
    pub herd_tune_version: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub registered_at: Option<String>,
}

impl NodeRegistration {
    /// Returns the effective backend URL, preferring backend_url over ollama_url.
    pub fn effective_url(&self) -> &str {
        self.backend_url.as_deref().unwrap_or(&self.ollama_url)
    }
}
```

Update `Node` struct similarly:

```rust
/// Stored node record from SQLite
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub node_id: Option<String>,
    pub hostname: String,
    /// Effective backend URL (unified field replacing ollama_url)
    pub backend_url: String,
    pub backend: BackendType,
    pub backend_version: Option<String>,
    pub gpu: Option<String>,
    pub gpu_vendor: Option<String>,
    pub gpu_model: Option<String>,
    pub gpu_backend: Option<String>,
    pub cuda_version: Option<String>,
    pub vram_mb: u32,
    pub ram_mb: u32,
    pub max_concurrent: u32,
    pub ollama_version: Option<String>,
    pub os: Option<String>,
    pub status: String,
    pub priority: u32,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub models_available: u32,
    pub models_loaded: Vec<String>,
    pub model_paths: Vec<String>,
    pub capabilities: Vec<String>,
    pub recommended_config: serde_json::Value,
    pub config_applied: bool,
    pub last_health_check: Option<String>,
    pub registered_at: String,
    pub updated_at: String,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib nodes::types::tests -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/nodes/types.rs
git commit -m "feat: add backend type and GPU fields to node registration types"
```

---

## Task 3: SQLite schema migration

**Files:**
- Modify: `src/nodes/db.rs`

- [ ] **Step 1: Write failing test — new columns readable after migration**

Add a test module at the bottom of `src/nodes/db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BackendType;

    fn test_db() -> NodeDb {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").unwrap();
        let db = NodeDb { conn: Mutex::new(conn) };
        db.migrate().unwrap();
        db
    }

    #[test]
    fn migrate_creates_new_columns() {
        let db = test_db();
        // Register an Ollama node (old-style)
        let reg = NodeRegistration {
            hostname: "test-node".to_string(),
            ollama_url: "http://test:11434".to_string(),
            backend: BackendType::Ollama,
            ..Default::default()
        };
        let (id, is_new) = db.upsert_node(&reg).unwrap();
        assert!(is_new);

        let node = db.get_node(&id).unwrap().unwrap();
        assert_eq!(node.backend, BackendType::Ollama);
        assert_eq!(node.backend_url, "http://test:11434");
    }

    #[test]
    fn upsert_llama_server_node() {
        let db = test_db();
        let reg = NodeRegistration {
            hostname: "citadel".to_string(),
            ollama_url: String::new(),
            backend_url: Some("http://citadel:8090".to_string()),
            backend: BackendType::LlamaServer,
            gpu_vendor: Some("nvidia".to_string()),
            gpu_model: Some("RTX 5090".to_string()),
            gpu_backend: Some("cuda".to_string()),
            cuda_version: Some("13.1".to_string()),
            vram_mb: 32768,
            models_loaded: vec!["gemma-4.gguf".to_string()],
            model_paths: vec!["/models/gemma-4.gguf".to_string()],
            capabilities: vec!["cuda".to_string(), "flash_attn".to_string()],
            ..Default::default()
        };
        let (id, is_new) = db.upsert_node(&reg).unwrap();
        assert!(is_new);

        let node = db.get_node(&id).unwrap().unwrap();
        assert_eq!(node.backend, BackendType::LlamaServer);
        assert_eq!(node.backend_url, "http://citadel:8090");
        assert_eq!(node.gpu_vendor.as_deref(), Some("nvidia"));
        assert_eq!(node.gpu_backend.as_deref(), Some("cuda"));
        assert_eq!(node.cuda_version.as_deref(), Some("13.1"));
        assert_eq!(node.capabilities, vec!["cuda", "flash_attn"]);
        assert_eq!(node.model_paths, vec!["/models/gemma-4.gguf"]);
    }

    #[test]
    fn upsert_idempotent_re_registration() {
        let db = test_db();
        let reg = NodeRegistration {
            hostname: "node1".to_string(),
            ollama_url: "http://node1:11434".to_string(),
            backend: BackendType::Ollama,
            ..Default::default()
        };
        let (id1, new1) = db.upsert_node(&reg).unwrap();
        assert!(new1);

        // Re-register same hostname — should update, not insert
        let (id2, new2) = db.upsert_node(&reg).unwrap();
        assert!(!new2);
        assert_eq!(id1, id2);
    }

    #[test]
    fn routable_nodes_filters_correctly() {
        let db = test_db();
        let reg = NodeRegistration {
            hostname: "healthy-node".to_string(),
            ollama_url: "http://healthy:11434".to_string(),
            ..Default::default()
        };
        db.upsert_node(&reg).unwrap();

        let routable = db.get_routable_nodes().unwrap();
        assert_eq!(routable.len(), 1);
        assert_eq!(routable[0].hostname, "healthy-node");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib nodes::db::tests -v`
Expected: FAIL — `NodeRegistration` has no `Default`, new columns missing from schema/queries

- [ ] **Step 3: Add Default impl for NodeRegistration**

Add to `src/nodes/types.rs`:

```rust
impl Default for NodeRegistration {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            ollama_url: String::new(),
            backend_url: None,
            backend: BackendType::default(),
            node_id: None,
            gpu: None,
            gpu_vendor: None,
            gpu_model: None,
            gpu_backend: None,
            cuda_version: None,
            vram_mb: 0,
            ram_mb: 0,
            ollama_version: None,
            backend_version: None,
            models_available: 0,
            models_loaded: Vec::new(),
            model_paths: Vec::new(),
            capabilities: Vec::new(),
            recommended_config: serde_json::Value::Object(Default::default()),
            config_applied: false,
            herd_tune_version: None,
            os: None,
            registered_at: None,
        }
    }
}
```

- [ ] **Step 4: Update the `migrate()` method with new columns**

In `src/nodes/db.rs`, update the `migrate` method. Keep the original `CREATE TABLE IF NOT EXISTS` as-is (for new installs), then add `ALTER TABLE` statements for each new column (for existing installs):

```rust
fn migrate(&self) -> Result<()> {
    let conn = self
        .conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;

    // Original table (for fresh installs)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS nodes (
            id TEXT PRIMARY KEY,
            hostname TEXT NOT NULL UNIQUE,
            ollama_url TEXT NOT NULL DEFAULT '',
            gpu TEXT,
            vram_mb INTEGER DEFAULT 0,
            ram_mb INTEGER DEFAULT 0,
            max_concurrent INTEGER DEFAULT 1,
            ollama_version TEXT,
            os TEXT,
            status TEXT DEFAULT 'healthy',
            priority INTEGER DEFAULT 10,
            enabled INTEGER DEFAULT 1,
            tags TEXT DEFAULT '[]',
            models_available INTEGER DEFAULT 0,
            models_loaded TEXT DEFAULT '[]',
            recommended_config TEXT DEFAULT '{}',
            config_applied INTEGER DEFAULT 0,
            last_health_check TEXT,
            registered_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    )?;

    // Migration v1: stable machine identity
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN node_id TEXT;").ok();
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_node_id ON nodes(node_id) WHERE node_id IS NOT NULL;",
    )?;

    // Migration v2: llama-server backend support
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN backend TEXT NOT NULL DEFAULT 'ollama';").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN backend_url TEXT NOT NULL DEFAULT '';").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN backend_version TEXT;").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN gpu_vendor TEXT;").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN gpu_model TEXT;").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN gpu_backend TEXT;").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN cuda_version TEXT;").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN model_paths TEXT DEFAULT '[]';").ok();
    conn.execute_batch("ALTER TABLE nodes ADD COLUMN capabilities TEXT DEFAULT '[]';").ok();

    // Backfill: copy ollama_url → backend_url for existing rows where backend_url is empty
    conn.execute_batch(
        "UPDATE nodes SET backend_url = ollama_url WHERE backend_url = '' AND ollama_url != '';",
    )?;

    Ok(())
}
```

- [ ] **Step 5: Update `NODE_COLUMNS`, `row_to_node`, `upsert_node` queries**

Update `NODE_COLUMNS` to include new columns:

```rust
const NODE_COLUMNS: &'static str =
    "id, node_id, hostname, backend_url, backend, backend_version,
     gpu, gpu_vendor, gpu_model, gpu_backend, cuda_version,
     vram_mb, ram_mb, max_concurrent,
     ollama_version, os, status, priority, enabled, tags, models_available,
     models_loaded, model_paths, capabilities,
     recommended_config, config_applied, last_health_check,
     registered_at, updated_at";
```

Update `row_to_node` to map the new column order (29 columns):

```rust
fn row_to_node(row: &rusqlite::Row) -> rusqlite::Result<Node> {
    let backend_str: String = row.get(4)?;
    let backend = match backend_str.as_str() {
        "llama-server" => crate::config::BackendType::LlamaServer,
        _ => crate::config::BackendType::Ollama,
    };
    Ok(Node {
        id: row.get(0)?,
        node_id: row.get(1)?,
        hostname: row.get(2)?,
        backend_url: row.get(3)?,
        backend,
        backend_version: row.get(5)?,
        gpu: row.get(6)?,
        gpu_vendor: row.get(7)?,
        gpu_model: row.get(8)?,
        gpu_backend: row.get(9)?,
        cuda_version: row.get(10)?,
        vram_mb: row.get::<_, i32>(11)? as u32,
        ram_mb: row.get::<_, i32>(12)? as u32,
        max_concurrent: row.get::<_, i32>(13)? as u32,
        ollama_version: row.get(14)?,
        os: row.get(15)?,
        status: row.get(16)?,
        priority: row.get::<_, i32>(17)? as u32,
        enabled: row.get::<_, i32>(18)? != 0,
        tags: serde_json::from_str(&row.get::<_, String>(19)?).unwrap_or_default(),
        models_available: row.get::<_, i32>(20)? as u32,
        models_loaded: serde_json::from_str(&row.get::<_, String>(21)?)
            .unwrap_or_default(),
        model_paths: serde_json::from_str(&row.get::<_, String>(22)?)
            .unwrap_or_default(),
        capabilities: serde_json::from_str(&row.get::<_, String>(23)?)
            .unwrap_or_default(),
        recommended_config: serde_json::from_str(&row.get::<_, String>(24)?)
            .unwrap_or_default(),
        config_applied: row.get::<_, i32>(25)? != 0,
        last_health_check: row.get(26)?,
        registered_at: row.get(27)?,
        updated_at: row.get(28)?,
    })
}
```

Update `upsert_node` to write all new fields. The `effective_url()` helper determines `backend_url`:

```rust
pub fn upsert_node(&self, reg: &NodeRegistration) -> Result<(String, bool)> {
    let conn = self
        .conn
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {}", e))?;
    let now = chrono::Utc::now().to_rfc3339();
    let registered_at = reg.registered_at.clone().unwrap_or_else(|| now.clone());
    let backend_url = reg.effective_url().to_string();
    let backend_str = reg.backend.to_string();

    let existing_id: Option<String> = reg
        .node_id
        .as_ref()
        .and_then(|nid| {
            conn.query_row(
                "SELECT id FROM nodes WHERE node_id = ?1",
                rusqlite::params![nid],
                |row| row.get(0),
            )
            .ok()
        })
        .or_else(|| {
            conn.query_row(
                "SELECT id FROM nodes WHERE hostname = ?1",
                rusqlite::params![reg.hostname],
                |row| row.get(0),
            )
            .ok()
        });

    let models_loaded_json = serde_json::to_string(&reg.models_loaded)?;
    let model_paths_json = serde_json::to_string(&reg.model_paths)?;
    let capabilities_json = serde_json::to_string(&reg.capabilities)?;
    let recommended_config_json = serde_json::to_string(&reg.recommended_config)?;

    let max_concurrent = reg
        .recommended_config
        .get("num_parallel")
        .and_then(|v| v.as_u64())
        .unwrap_or(1) as u32;

    if let Some(id) = existing_id {
        conn.execute(
            "UPDATE nodes SET
                backend_url = ?1, backend = ?2, backend_version = ?3,
                ollama_url = ?4, gpu = ?5,
                gpu_vendor = ?6, gpu_model = ?7, gpu_backend = ?8, cuda_version = ?9,
                vram_mb = ?10, ram_mb = ?11, max_concurrent = ?12,
                ollama_version = ?13, os = ?14,
                status = 'healthy', models_available = ?15, models_loaded = ?16,
                model_paths = ?17, capabilities = ?18,
                recommended_config = ?19, config_applied = ?20, updated_at = ?21,
                node_id = COALESCE(?23, node_id), hostname = ?24
            WHERE id = ?22",
            rusqlite::params![
                backend_url,
                backend_str,
                reg.backend_version,
                reg.ollama_url,
                reg.gpu,
                reg.gpu_vendor,
                reg.gpu_model,
                reg.gpu_backend,
                reg.cuda_version,
                reg.vram_mb,
                reg.ram_mb,
                max_concurrent,
                reg.ollama_version,
                reg.os,
                reg.models_available,
                models_loaded_json,
                model_paths_json,
                capabilities_json,
                recommended_config_json,
                reg.config_applied as i32,
                now,
                id,
                reg.node_id,
                reg.hostname
            ],
        )?;
        Ok((id, false))
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO nodes (id, node_id, hostname, backend_url, backend, backend_version,
                ollama_url, gpu, gpu_vendor, gpu_model, gpu_backend, cuda_version,
                vram_mb, ram_mb, max_concurrent, ollama_version, os, status,
                models_available, models_loaded, model_paths, capabilities,
                recommended_config, config_applied, registered_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, 'healthy', ?18, ?19, ?20, ?21,
                    ?22, ?23, ?24, ?25)",
            rusqlite::params![
                id,
                reg.node_id,
                reg.hostname,
                backend_url,
                backend_str,
                reg.backend_version,
                reg.ollama_url,
                reg.gpu,
                reg.gpu_vendor,
                reg.gpu_model,
                reg.gpu_backend,
                reg.cuda_version,
                reg.vram_mb,
                reg.ram_mb,
                max_concurrent,
                reg.ollama_version,
                reg.os,
                reg.models_available,
                models_loaded_json,
                model_paths_json,
                capabilities_json,
                recommended_config_json,
                reg.config_applied as i32,
                registered_at,
                now
            ],
        )?;
        Ok((id, true))
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib nodes -v`
Expected: All node tests PASS

- [ ] **Step 7: Commit**

```bash
git add src/nodes/db.rs src/nodes/types.rs
git commit -m "feat: SQLite schema migration for llama-server backend fields"
```

---

## Task 4: Backend-aware node health polling

**Files:**
- Modify: `src/nodes/health.rs`

- [ ] **Step 1: Write failing test — llama-server health check response parsing**

Add at bottom of `src/nodes/health.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_llama_server_models_response() {
        let json = r#"{"object":"list","data":[{"id":"gemma-4-26B","object":"model","owned_by":"llamacpp"}]}"#;
        let resp: LlamaServerModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].id, "gemma-4-26B");
    }

    #[test]
    fn parse_llama_server_health_response() {
        // llama-server /health returns {"status":"ok"} when healthy
        let json = r#"{"status":"ok"}"#;
        let resp: LlamaServerHealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, "ok");
    }

    #[test]
    fn parse_llama_server_health_loading() {
        let json = r#"{"status":"loading model"}"#;
        let resp: LlamaServerHealthResponse = serde_json::from_str(json).unwrap();
        assert_ne!(resp.status, "ok");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib nodes::health::tests -v`
Expected: FAIL — `LlamaServerModelsResponse` and `LlamaServerHealthResponse` not found

- [ ] **Step 3: Add llama-server response types and backend-aware polling**

Add response types near the top of `src/nodes/health.rs` (after Ollama types):

```rust
/// llama-server /v1/models response (OpenAI format)
#[derive(Debug, Deserialize)]
struct LlamaServerModelsResponse {
    #[serde(default)]
    data: Vec<LlamaServerModel>,
}

#[derive(Debug, Deserialize)]
struct LlamaServerModel {
    id: String,
}

/// llama-server /health response
#[derive(Debug, Deserialize)]
struct LlamaServerHealthResponse {
    status: String,
}
```

Update `poll_node` to dispatch based on backend type. Replace the existing `poll_node` method:

```rust
async fn poll_node(&self, node_db: &NodeDb, node: &crate::nodes::Node, check_tags: bool) {
    let base_url = node.backend_url.trim_end_matches('/');

    match node.backend {
        crate::config::BackendType::LlamaServer => {
            self.poll_llama_server(node_db, node, base_url).await;
        }
        crate::config::BackendType::Ollama => {
            self.poll_ollama(node_db, node, base_url, check_tags).await;
        }
    }
}

async fn poll_llama_server(&self, node_db: &NodeDb, node: &crate::nodes::Node, base_url: &str) {
    // GET /health — server health
    let health_url = format!("{}/health", base_url);
    let health_result = self.client.get(&health_url).send().await;

    match health_result {
        Ok(resp) if resp.status().is_success() => {
            let is_ok = match resp.json::<LlamaServerHealthResponse>().await {
                Ok(h) => h.status == "ok",
                Err(_) => true, // 200 is good enough
            };

            if !is_ok {
                // Server is loading — mark degraded
                if let Err(e) = node_db.update_health(&node.id, "degraded", &node.models_loaded, None) {
                    tracing::error!("Failed to update health for node {}: {}", node.hostname, e);
                }
                return;
            }

            // GET /v1/models — loaded models
            let models_url = format!("{}/v1/models", base_url);
            let models_loaded: Vec<String> = match self.client.get(&models_url).send().await {
                Ok(r) if r.status().is_success() => {
                    match r.json::<LlamaServerModelsResponse>().await {
                        Ok(m) => m.data.into_iter().map(|d| d.id).collect(),
                        Err(_) => node.models_loaded.clone(), // keep existing
                    }
                }
                _ => node.models_loaded.clone(),
            };

            let models_available = Some(models_loaded.len() as u32);
            if let Err(e) = node_db.update_health(&node.id, "healthy", &models_loaded, models_available) {
                tracing::error!("Failed to update health for node {}: {}", node.hostname, e);
            }
        }
        Ok(resp) => {
            tracing::warn!(
                "Node {} ({}) returned status {} from /health",
                node.hostname, base_url, resp.status()
            );
            let new_status = if node.status == "healthy" || node.status == "degraded" {
                "degraded"
            } else {
                "unreachable"
            };
            if let Err(e) = node_db.update_health(&node.id, new_status, &[], None) {
                tracing::error!("Failed to update health for node {}: {}", node.hostname, e);
            }
        }
        Err(e) => {
            tracing::warn!("Node {} ({}) health check failed: {}", node.hostname, base_url, e);
            let new_status = match node.status.as_str() {
                "healthy" => "degraded",
                "degraded" => "unreachable",
                _ => "unreachable",
            };
            if let Err(e) = node_db.update_health(&node.id, new_status, &[], None) {
                tracing::error!("Failed to update health for node {}: {}", node.hostname, e);
            }
        }
    }
}
```

Rename the existing Ollama health check logic to `poll_ollama`:

```rust
async fn poll_ollama(&self, node_db: &NodeDb, node: &crate::nodes::Node, base_url: &str, check_tags: bool) {
    // ... (existing poll_node body, replacing node.ollama_url references with base_url)
}
```

- [ ] **Step 4: Update `sync_to_pool` to use `backend_url`**

In the `sync_to_pool` method, change `node.ollama_url.clone()` to `node.backend_url.clone()`:

```rust
let backend = crate::config::Backend {
    name: backend_name.clone(),
    url: node.backend_url.clone(),
    backend: node.backend,
    priority: node.priority,
    tags: node.tags.clone(),
    ..Default::default()
};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib nodes -v`
Expected: All PASS

- [ ] **Step 6: Commit**

```bash
git add src/nodes/health.rs
git commit -m "feat: backend-aware health polling for llama-server nodes"
```

---

## Task 5: Backend-aware model discovery for static backends

**Files:**
- Modify: `src/backend/discovery.rs`

- [ ] **Step 1: Write failing test — llama-server model discovery response**

Add test at bottom of `src/backend/discovery.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_models_response() {
        let json = r#"{"object":"list","data":[
            {"id":"gemma-4-26B","object":"model","owned_by":"llamacpp","created":1234}
        ]}"#;
        let resp: OpenAIModelsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].id, "gemma-4-26B");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib backend::discovery::tests -v`
Expected: FAIL — `OpenAIModelsResponse` not found

- [ ] **Step 3: Add OpenAI models response type and backend-aware discovery**

Add response type:

```rust
/// OpenAI-compatible /v1/models response (used by llama-server)
#[derive(Debug, Deserialize)]
struct OpenAIModelsResponse {
    #[serde(default)]
    data: Vec<OpenAIModel>,
}

#[derive(Debug, Deserialize)]
struct OpenAIModel {
    id: String,
}
```

Update `discover_models` to branch on backend type:

```rust
async fn discover_models(&self, pool: &BackendPool, backend: &Backend) -> Result<()> {
    let mut model_names: Vec<String> = match backend.backend {
        crate::config::BackendType::LlamaServer => {
            let url = format!("{}/v1/models", backend.url);
            let resp = self.client.get(&url).send().await?;
            let models: OpenAIModelsResponse = resp.json().await?;
            models.data.into_iter().map(|m| m.id).collect()
        }
        crate::config::BackendType::Ollama => {
            let url = format!("{}/api/tags", backend.url);
            let resp = self.client.get(&url).send().await?;
            let models: OllamaModels = resp.json().await?;
            models.models.into_iter().map(|m| m.name).collect()
        }
    };

    // Apply model_filter regex if configured
    if let Some(ref filter) = backend.model_filter {
        match regex::Regex::new(filter) {
            Ok(re) => {
                let before = model_names.len();
                model_names.retain(|name| re.is_match(name));
                if model_names.len() < before {
                    tracing::debug!(
                        "model_filter '{}' on {}: kept {}/{} models",
                        filter, backend.name, model_names.len(), before
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Invalid model_filter '{}' on {}: {}", filter, backend.name, e);
            }
        }
    }

    pool.update_models(&backend.name, model_names).await;
    Ok(())
}
```

Update `discover_running` to branch on backend type:

```rust
async fn discover_running(&self, pool: &BackendPool, backend: &Backend) -> Result<()> {
    let current = match backend.backend {
        crate::config::BackendType::LlamaServer => {
            // llama-server always has its model loaded — use /v1/models
            let url = format!("{}/v1/models", backend.url);
            let resp = self.client.get(&url).send().await?;
            let models: OpenAIModelsResponse = resp.json().await?;
            models.data.first().map(|m| m.id.clone())
        }
        crate::config::BackendType::Ollama => {
            let url = format!("{}/api/ps", backend.url);
            let resp = self.client.get(&url).send().await?;
            let running: OllamaRunning = resp.json().await?;
            running.models.first().map(|m| {
                if m.model.is_empty() {
                    m.name.clone()
                } else {
                    m.model.clone()
                }
            })
        }
    };

    pool.update_current_model(&backend.name, current).await;
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib backend::discovery -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/backend/discovery.rs
git commit -m "feat: backend-aware model discovery (Ollama /api/tags vs llama-server /v1/models)"
```

---

## Task 6: Skip model warming for llama-server backends

**Files:**
- Modify: `src/backend/warmer.rs`

- [ ] **Step 1: Write failing test — warmer skips llama-server backends**

Add to the existing test module in `src/backend/warmer.rs`:

```rust
#[test]
fn llama_server_backend_has_no_hot_models_by_default() {
    // llama-server backends shouldn't need warming — models are loaded at server start.
    // The warmer naturally skips backends with empty hot_models, but we verify
    // that Backend::default() (which is Ollama) has empty hot_models.
    let b = Backend::default();
    assert!(b.hot_models.is_empty());
}

#[test]
fn warm_url_not_used_for_llama_server() {
    // warm_url produces Ollama-specific /api/generate paths.
    // This test documents that warm_url is Ollama-only.
    let url = warm_url("http://citadel:8090");
    assert!(url.contains("/api/generate"));
}
```

- [ ] **Step 2: Run tests to verify they pass** (these are documentation tests)

Run: `cargo test --lib backend::warmer -v`
Expected: PASS

- [ ] **Step 3: Add backend type check to warmer loop**

In `warm_all`, skip llama-server backends:

```rust
async fn warm_all(&self, pool: &BackendPool) {
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_WARMUPS));
    let backends = pool.all().await;
    for name in backends {
        if let Some(state) = pool.get(&name).await {
            // Skip llama-server backends — models are loaded at server start,
            // and /api/generate is Ollama-specific.
            if state.config.backend == crate::config::BackendType::LlamaServer {
                continue;
            }

            for model in &state.config.hot_models {
                let url = warm_url(&state.config.url);
                let payload = warm_payload(model);
                let client = self.client.clone();
                let model = model.clone();
                let name = name.clone();
                let permit = semaphore.clone();
                tokio::spawn(async move {
                    let _permit = permit.acquire().await.expect("semaphore closed");
                    if let Err(e) = client.post(&url).json(&payload).send().await {
                        tracing::warn!("Warmer failed for {} on {}: {}", model, name, e);
                    } else {
                        tracing::debug!("Warmed {} on {}", model, name);
                    }
                });
            }
        }
    }
}
```

- [ ] **Step 4: Run all backend tests**

Run: `cargo test --lib backend -v`
Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/backend/warmer.rs
git commit -m "feat: skip model warming for llama-server backends"
```

---

## Task 7: Scope keep_alive injection to Ollama backends only

**Files:**
- Modify: `src/server.rs`

- [ ] **Step 1: Write failing test — keep_alive not injected for llama-server backend URL**

The `inject_keep_alive` function operates on bytes and path, not backend type. The fix is at the call site in `proxy_handler` — we need to know the backend type of the selected backend. This requires checking the `BackendState` after routing.

Rather than adding a test for `inject_keep_alive` (which works correctly), we add logic to skip injection when the routed backend is a llama-server. This is a behavior change best verified by integration testing.

- [ ] **Step 2: Update proxy_handler to skip keep_alive injection for llama-server**

In `proxy_handler` in `src/server.rs`, move the `inject_keep_alive` call inside the retry loop where we know which backend was selected, and gate it on backend type:

Find the line (approx line 787):
```rust
let forward_bytes = inject_keep_alive(&body_bytes, &path, &keep_alive_value);
```

Replace with:
```rust
// keep_alive injection is deferred to the retry loop — it's Ollama-specific
// and we need to know the selected backend's type first.
let forward_bytes_base = body_bytes.clone();
```

Then inside the retry loop, before building the request:
```rust
// Only inject keep_alive for Ollama backends (llama-server doesn't use it)
let forward_bytes = if state.pool.get(&backend.name).await
    .map(|s| s.config.backend == crate::config::BackendType::Ollama)
    .unwrap_or(true)
{
    inject_keep_alive(&forward_bytes_base, &path, &keep_alive_value)
} else {
    forward_bytes_base.clone().into()
};
```

- [ ] **Step 3: Run existing proxy tests**

Run: `cargo test --lib server -v`
Expected: All existing proxy tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/server.rs
git commit -m "feat: scope keep_alive injection to Ollama backends only"
```

---

## Task 8: Smart health check defaults for static backends

**Files:**
- Modify: `src/backend/health.rs`

- [ ] **Step 1: Write failing test — llama-server uses /health by default**

Add to existing test module in `src/backend/health.rs`:

```rust
#[test]
fn llama_server_default_health_check_path() {
    let b = Backend {
        name: "llama1".into(),
        url: "http://localhost:8090".into(),
        backend: crate::config::BackendType::LlamaServer,
        priority: 50,
        ..Default::default()
    };
    // When no explicit health_check_path is set, llama-server should use /health
    let path = b.health_check_path.as_deref()
        .unwrap_or(b.default_health_check_path());
    assert_eq!(path, "/health");
}

#[test]
fn ollama_default_health_check_path() {
    let b = Backend::default();
    let path = b.health_check_path.as_deref()
        .unwrap_or(b.default_health_check_path());
    assert_eq!(path, "/");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib backend::health::tests -v`
Expected: FAIL — `default_health_check_path` not found

- [ ] **Step 3: Add `default_health_check_path` to Backend**

In `src/config.rs`, add a method to `Backend`:

```rust
impl Backend {
    /// Default health check path based on backend type.
    pub fn default_health_check_path(&self) -> &str {
        match self.backend {
            BackendType::LlamaServer => "/health",
            BackendType::Ollama => "/",
        }
    }
}
```

Update `src/backend/health.rs` `check_all` to use it:

```rust
let path = state.config.health_check_path.as_deref()
    .unwrap_or(state.config.default_health_check_path());
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib backend::health -v`
Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/backend/health.rs
git commit -m "feat: smart health check path defaults by backend type"
```

---

## Task 9: Update herd.yaml.example with backend field documentation

**Files:**
- Modify: `herd.yaml.example`

- [ ] **Step 1: Add llama-server backend example**

Add a new backend entry to `herd.yaml.example` showing the llama-server configuration:

```yaml
  # llama-server backend example (llama.cpp):
  # - name: "citadel-llama"
  #   url: "http://citadel:8090"   # llama-server listen address
  #   backend: "llama-server"      # Backend type: "ollama" (default) or "llama-server"
  #   priority: 100
  #   tags:
  #     - "gpu"
  #     - "high-vram"
  #   # health_check_path defaults to /health for llama-server (vs / for ollama)
  #   # hot_models is ignored for llama-server (model is set at server start)
```

- [ ] **Step 2: Commit**

```bash
git add herd.yaml.example
git commit -m "docs: add llama-server backend example to herd.yaml.example"
```

---

## Task 10: Full build and test verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass (existing 111+ plus new tests from this plan)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: No warnings

- [ ] **Step 3: Build release**

Run: `cargo build --release`
Expected: Clean build

- [ ] **Step 4: Verify backward compatibility**

Confirm that an empty `herd.yaml` (no `backend` field on any backend) still works:

Run: `cargo test --lib config::tests -v`
Expected: All existing config tests PASS — `backend` defaults to `Ollama`

- [ ] **Step 5: Commit any fixes**

If clippy or tests revealed issues, fix and commit:

```bash
git add -A
git commit -m "fix: address clippy warnings and test failures"
```

---

## Summary of Changes

| File | What Changes | Lines Added (est.) |
|------|-------------|-------------------|
| `src/config.rs` | `BackendType` enum, `backend` field on `Backend`, `default_health_check_path()` | ~35 |
| `src/nodes/types.rs` | New fields on `NodeRegistration` + `Node`, `effective_url()`, `Default` impl | ~80 |
| `src/nodes/db.rs` | Schema migration v2, updated queries, 29-column mapping | ~120 |
| `src/nodes/health.rs` | llama-server response types, `poll_llama_server`, backend dispatch | ~90 |
| `src/backend/discovery.rs` | `OpenAIModelsResponse`, backend-aware `discover_models` + `discover_running` | ~50 |
| `src/backend/warmer.rs` | Skip llama-server backends in `warm_all` | ~5 |
| `src/backend/health.rs` | Use `default_health_check_path()` | ~3 |
| `src/server.rs` | Gate `inject_keep_alive` on Ollama backend type | ~10 |
| `herd.yaml.example` | Commented llama-server example | ~10 |
| **Total** | | **~400** |
