//! Embedded SQLite knowledge graph.
//!
//! Provides entity/relationship storage in the same `brain.db` used by
//! `SqliteMemory`. All writes go through verb normalization via the
//! `graph_rel_aliases` lookup table. Queries leverage FTS5 for fuzzy
//! entity search and a pre-joined view for efficient neighbor traversal.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// ── Public types ────────────────────────────────────────────────────

/// A node in the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub id: i64,
    pub name: String,
    pub canonical_name: String,
    pub label: String,
    pub properties: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// An edge in the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub id: i64,
    pub source_id: i64,
    pub target_id: i64,
    pub rel_type: String,
    pub properties: Option<serde_json::Value>,
    pub created_at: String,
}

/// A fully-resolved connection (entity → relationship → entity).
///
/// Named `GraphConnection` to avoid collision with `rusqlite::Connection`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphConnection {
    pub rel_id: i64,
    pub rel_type: String,
    pub from_name: String,
    pub from_canonical: String,
    pub from_label: String,
    pub to_name: String,
    pub to_canonical: String,
    pub to_label: String,
    pub rel_properties: Option<serde_json::Value>,
    pub created_at: String,
}

impl std::fmt::Display for GraphConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} → {} → {}",
            self.from_name, self.rel_type, self.to_name
        )
    }
}

/// Graph statistics summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStats {
    pub entities: usize,
    pub relationships: usize,
    pub relationship_types: usize,
}

impl std::fmt::Display for GraphStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} entities, {} relationships, {} types",
            self.entities, self.relationships, self.relationship_types
        )
    }
}

// ── Knowledge Graph ─────────────────────────────────────────────────

/// Thread-safe handle to the embedded knowledge graph.
///
/// Shares the same `Arc<Mutex<Connection>>` pattern as `SqliteMemory`.
/// Can be constructed from the same `brain.db` or a separate connection.
#[derive(Clone)]
pub struct KnowledgeGraph {
    conn: Arc<Mutex<Connection>>,
}

impl KnowledgeGraph {
    /// Open (or create) graph tables in the given database file.
    ///
    /// Typically called with the same `brain.db` path that `SqliteMemory` uses.
    /// Schema initialization is idempotent (CREATE IF NOT EXISTS).
    pub fn new(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .context("failed to create directory for graph database")?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("failed to open graph database at {}", db_path.display()))?;

        // Match SqliteMemory PRAGMA tuning
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA mmap_size    = 8388608;
             PRAGMA cache_size   = -2000;
             PRAGMA temp_store   = MEMORY;
             PRAGMA foreign_keys = ON;",
        )?;

        Self::init_schema(&conn)?;
        Self::seed_default_rel_types(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Construct from an existing shared connection.
    ///
    /// Useful when the caller already holds an open connection to `brain.db`
    /// (e.g., from `SqliteMemory`). The caller is responsible for having run
    /// `init_schema` on this connection.
    pub fn from_connection(conn: Arc<Mutex<Connection>>) -> Result<Self> {
        {
            let c = conn.lock();
            c.execute_batch("PRAGMA foreign_keys = ON;")?;
            Self::init_schema(&c)?;
            Self::seed_default_rel_types(&c)?;
        }
        Ok(Self { conn })
    }

    // ── Schema ──────────────────────────────────────────────────

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "-- Relationship type registry with optional directionality
            CREATE TABLE IF NOT EXISTS graph_rel_types (
                id INTEGER PRIMARY KEY,
                canonical TEXT NOT NULL UNIQUE,
                category TEXT,
                directed INTEGER NOT NULL DEFAULT 1
            );

            -- Verb alias → canonical relationship mapping (write-time normalization)
            CREATE TABLE IF NOT EXISTS graph_rel_aliases (
                raw_verb TEXT PRIMARY KEY,
                canonical TEXT NOT NULL REFERENCES graph_rel_types(canonical)
            );

            -- Entity nodes
            CREATE TABLE IF NOT EXISTS graph_entities (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                canonical_name TEXT NOT NULL,
                label TEXT NOT NULL DEFAULT 'entity',
                properties TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_entities_canonical
                ON graph_entities(canonical_name, label);

            -- FTS5 full-text index for fuzzy entity search
            CREATE VIRTUAL TABLE IF NOT EXISTS graph_entities_fts USING fts5(
                name, canonical_name, content=graph_entities, content_rowid=id
            );

            -- FTS5 sync triggers (same pattern as memories_fts)
            CREATE TRIGGER IF NOT EXISTS graph_entities_ai AFTER INSERT ON graph_entities BEGIN
                INSERT INTO graph_entities_fts(rowid, name, canonical_name)
                VALUES (new.id, new.name, new.canonical_name);
            END;
            CREATE TRIGGER IF NOT EXISTS graph_entities_ad AFTER DELETE ON graph_entities BEGIN
                INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, canonical_name)
                VALUES ('delete', old.id, old.name, old.canonical_name);
            END;
            CREATE TRIGGER IF NOT EXISTS graph_entities_au AFTER UPDATE ON graph_entities BEGIN
                INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, canonical_name)
                VALUES ('delete', old.id, old.name, old.canonical_name);
                INSERT INTO graph_entities_fts(rowid, name, canonical_name)
                VALUES (new.id, new.name, new.canonical_name);
            END;

            -- Relationship edges
            CREATE TABLE IF NOT EXISTS graph_relationships (
                id INTEGER PRIMARY KEY,
                source_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
                target_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
                rel_type TEXT NOT NULL REFERENCES graph_rel_types(canonical),
                properties TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(source_id, target_id, rel_type)
            );
            CREATE INDEX IF NOT EXISTS idx_rel_source ON graph_relationships(source_id);
            CREATE INDEX IF NOT EXISTS idx_rel_target ON graph_relationships(target_id);

            -- Pre-joined view for efficient connection queries
            CREATE VIEW IF NOT EXISTS graph_connections AS
            SELECT r.id AS rel_id, r.rel_type,
                   s.id AS from_id, s.name AS from_name, s.canonical_name AS from_canonical,
                   s.label AS from_label,
                   t.id AS to_id, t.name AS to_name, t.canonical_name AS to_canonical,
                   t.label AS to_label,
                   r.properties AS rel_properties, r.created_at
            FROM graph_relationships r
            JOIN graph_entities s ON s.id = r.source_id
            JOIN graph_entities t ON t.id = r.target_id;",
        )
        .context("failed to initialize knowledge graph schema")?;

        Ok(())
    }

    /// Seed commonly-used relationship types and verb aliases.
    fn seed_default_rel_types(conn: &Connection) -> Result<()> {
        let defaults: &[(&str, &str, bool, &[&str])] = &[
            (
                "partner_of",
                "social",
                false,
                &[
                    "partner of",
                    "dating",
                    "in a relationship with",
                    "married to",
                ],
            ),
            (
                "friend_of",
                "social",
                false,
                &["friends with", "friend of", "knows"],
            ),
            (
                "lives_in",
                "location",
                true,
                &["lives in", "resides in", "based in", "located in"],
            ),
            (
                "works_at",
                "professional",
                true,
                &["works at", "employed by", "works for"],
            ),
            ("owns", "possession", true, &["owns", "has", "possesses"]),
            (
                "likes",
                "preference",
                true,
                &["likes", "enjoys", "prefers", "loves"],
            ),
            (
                "dislikes",
                "preference",
                true,
                &["dislikes", "hates", "avoids"],
            ),
            (
                "created",
                "action",
                true,
                &["created", "built", "made", "wrote", "authored"],
            ),
            ("uses", "tool", true, &["uses", "utilizes", "works with"]),
            (
                "member_of",
                "membership",
                true,
                &["member of", "part of", "belongs to"],
            ),
            (
                "parent_of",
                "family",
                true,
                &["parent of", "father of", "mother of"],
            ),
            (
                "child_of",
                "family",
                true,
                &["child of", "son of", "daughter of"],
            ),
            (
                "sibling_of",
                "family",
                false,
                &["sibling of", "brother of", "sister of"],
            ),
            (
                "interested_in",
                "preference",
                true,
                &["interested in", "curious about", "fascinated by"],
            ),
            (
                "skilled_in",
                "competency",
                true,
                &["skilled in", "good at", "proficient in", "expert in"],
            ),
            (
                "studies",
                "education",
                true,
                &["studies", "learning", "studying"],
            ),
            (
                "related_to",
                "general",
                false,
                &["related to", "associated with", "connected to"],
            ),
        ];

        for &(canonical, category, directed, aliases) in defaults {
            conn.execute(
                "INSERT OR IGNORE INTO graph_rel_types (canonical, category, directed)
                 VALUES (?1, ?2, ?3)",
                params![canonical, category, directed as i32],
            )?;
            for alias in aliases {
                conn.execute(
                    "INSERT OR IGNORE INTO graph_rel_aliases (raw_verb, canonical)
                     VALUES (?1, ?2)",
                    params![alias, canonical],
                )?;
            }
        }

        Ok(())
    }

    // ── Entity CRUD ─────────────────────────────────────────────

    /// Canonicalize an entity name: lowercase, trim, collapse whitespace to underscores.
    pub fn canonicalize(name: &str) -> String {
        name.trim()
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("_")
    }

    /// Upsert an entity (synchronous). Returns the entity ID.
    ///
    /// If an entity with the same `(canonical_name, label)` exists, updates
    /// its `name` (preserving the user's preferred casing) and `updated_at`.
    pub fn upsert_entity_sync(
        &self,
        name: &str,
        label: &str,
        properties: Option<&serde_json::Value>,
    ) -> Result<i64> {
        let canonical = Self::canonicalize(name);
        let label = if label.is_empty() { "entity" } else { label };
        let props_json = properties.map(|p| p.to_string());
        let conn = self.conn.lock();

        conn.execute(
            "INSERT INTO graph_entities (name, canonical_name, label, properties)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(canonical_name, label) DO UPDATE SET
                 name = excluded.name,
                 properties = COALESCE(excluded.properties, graph_entities.properties),
                 updated_at = datetime('now')",
            params![name, canonical, label, props_json],
        )?;

        let id: i64 = conn.query_row(
            "SELECT id FROM graph_entities WHERE canonical_name = ?1 AND label = ?2",
            params![canonical, label],
            |row| row.get(0),
        )?;

        Ok(id)
    }

    /// Async wrapper: upsert an entity on the blocking threadpool.
    pub async fn upsert_entity(
        &self,
        name: &str,
        label: &str,
        properties: Option<&serde_json::Value>,
    ) -> Result<i64> {
        let this = self.clone();
        let name = name.to_string();
        let label = label.to_string();
        let properties = properties.cloned();
        tokio::task::spawn_blocking(move || {
            this.upsert_entity_sync(&name, &label, properties.as_ref())
        })
        .await?
    }

    /// Look up an entity by canonical name and optional label (synchronous).
    pub fn find_entity_sync(&self, name: &str, label: Option<&str>) -> Result<Option<Entity>> {
        let canonical = Self::canonicalize(name);
        let conn = self.conn.lock();

        let result = if let Some(label) = label {
            conn.query_row(
                "SELECT id, name, canonical_name, label, properties, created_at, updated_at
                 FROM graph_entities WHERE canonical_name = ?1 AND label = ?2",
                params![canonical, label],
                Self::row_to_entity,
            )
            .optional()?
        } else {
            conn.query_row(
                "SELECT id, name, canonical_name, label, properties, created_at, updated_at
                 FROM graph_entities WHERE canonical_name = ?1",
                params![canonical],
                Self::row_to_entity,
            )
            .optional()?
        };

        Ok(result)
    }

    /// Async wrapper: find entity on the blocking threadpool.
    pub async fn find_entity(&self, name: &str, label: Option<&str>) -> Result<Option<Entity>> {
        let this = self.clone();
        let name = name.to_string();
        let label = label.map(str::to_string);
        tokio::task::spawn_blocking(move || this.find_entity_sync(&name, label.as_deref())).await?
    }

    /// Fuzzy-search entities via FTS5 (synchronous). Returns up to `limit` matches.
    pub fn search_entities_sync(&self, query: &str, limit: usize) -> Result<Vec<Entity>> {
        let conn = self.conn.lock();
        // FTS5 query: use * suffix for prefix matching
        let fts_query = query
            .split_whitespace()
            .map(|w| format!("{w}*"))
            .collect::<Vec<_>>()
            .join(" ");

        let mut stmt = conn.prepare(
            "SELECT e.id, e.name, e.canonical_name, e.label, e.properties,
                    e.created_at, e.updated_at
             FROM graph_entities_fts f
             JOIN graph_entities e ON e.id = f.rowid
             WHERE graph_entities_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let entities = stmt
            .query_map(params![fts_query, limit as i64], Self::row_to_entity)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(entities)
    }

    /// Async wrapper: search entities on the blocking threadpool.
    pub async fn search_entities(&self, query: &str, limit: usize) -> Result<Vec<Entity>> {
        let this = self.clone();
        let query = query.to_string();
        tokio::task::spawn_blocking(move || this.search_entities_sync(&query, limit)).await?
    }

    /// Delete an entity and all its relationships (CASCADE, synchronous).
    pub fn delete_entity_sync(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let affected = conn.execute("DELETE FROM graph_entities WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    /// Async wrapper: delete entity on the blocking threadpool.
    pub async fn delete_entity(&self, id: i64) -> Result<bool> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.delete_entity_sync(id)).await?
    }

    // ── Relationship CRUD ───────────────────────────────────────

    /// Normalize a verb to its canonical relationship type (synchronous).
    ///
    /// First checks `graph_rel_aliases` for an exact match. If not found,
    /// canonicalizes the verb (lowercase, underscores) and checks if it's
    /// a known `graph_rel_types.canonical`. Falls back to `"related_to"`.
    pub fn normalize_verb_sync(&self, raw_verb: &str) -> Result<String> {
        let trimmed = raw_verb.trim().to_lowercase();
        let conn = self.conn.lock();

        // Check alias table first
        if let Some(canonical) = conn
            .query_row(
                "SELECT canonical FROM graph_rel_aliases WHERE raw_verb = ?1",
                params![trimmed],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(canonical);
        }

        // Check if it's already a canonical type
        let underscored = trimmed.replace(' ', "_");
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM graph_rel_types WHERE canonical = ?1)",
            params![underscored],
            |row| row.get(0),
        )?;

        if exists {
            return Ok(underscored);
        }

        // Fall back to "related_to" — the catch-all
        Ok("related_to".into())
    }

    /// Insert a relationship between two entities (synchronous).
    ///
    /// The verb is normalized via `normalize_verb_sync` before storage.
    /// Returns the relationship ID, or `None` if it was a duplicate
    /// (same source, target, and rel_type — INSERT OR IGNORE).
    pub fn insert_relationship_sync(
        &self,
        source_id: i64,
        target_id: i64,
        raw_verb: &str,
        properties: Option<&serde_json::Value>,
    ) -> Result<Option<i64>> {
        let rel_type = self.normalize_verb_sync(raw_verb)?;
        let props_json = properties.map(|p| p.to_string());
        let conn = self.conn.lock();

        // Ensure the rel_type exists in graph_rel_types (auto-register if novel)
        conn.execute(
            "INSERT OR IGNORE INTO graph_rel_types (canonical, category, directed)
             VALUES (?1, 'auto', 1)",
            params![rel_type],
        )?;

        let affected = conn.execute(
            "INSERT OR IGNORE INTO graph_relationships (source_id, target_id, rel_type, properties)
             VALUES (?1, ?2, ?3, ?4)",
            params![source_id, target_id, rel_type, props_json],
        )?;

        if affected == 0 {
            return Ok(None); // Duplicate — already exists
        }

        let id = conn.last_insert_rowid();
        Ok(Some(id))
    }

    /// Async wrapper: insert relationship on the blocking threadpool.
    pub async fn insert_relationship(
        &self,
        source_id: i64,
        target_id: i64,
        raw_verb: &str,
        properties: Option<&serde_json::Value>,
    ) -> Result<Option<i64>> {
        let this = self.clone();
        let raw_verb = raw_verb.to_string();
        let properties = properties.cloned();
        tokio::task::spawn_blocking(move || {
            this.insert_relationship_sync(source_id, target_id, &raw_verb, properties.as_ref())
        })
        .await?
    }

    /// Delete a relationship by ID (synchronous).
    pub fn delete_relationship_sync(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let affected =
            conn.execute("DELETE FROM graph_relationships WHERE id = ?1", params![id])?;
        Ok(affected > 0)
    }

    /// Async wrapper: delete relationship on the blocking threadpool.
    pub async fn delete_relationship(&self, id: i64) -> Result<bool> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.delete_relationship_sync(id)).await?
    }

    // ── Query helpers ───────────────────────────────────────────

    /// Get all connections for an entity (both as source and target, synchronous).
    pub fn neighbors_sync(&self, entity_id: i64, limit: usize) -> Result<Vec<GraphConnection>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT rel_id, rel_type, from_name, from_canonical, from_label,
                    to_name, to_canonical, to_label, rel_properties, created_at
             FROM graph_connections
             WHERE from_id = ?1 OR to_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;

        let connections = stmt
            .query_map(params![entity_id, limit as i64], Self::row_to_connection)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(connections)
    }

    /// Async wrapper: neighbors on the blocking threadpool.
    pub async fn neighbors(&self, entity_id: i64, limit: usize) -> Result<Vec<GraphConnection>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.neighbors_sync(entity_id, limit)).await?
    }

    /// Get connections for multiple entity IDs at once (synchronous, batch query).
    pub fn neighbors_batch_sync(
        &self,
        entity_ids: &[i64],
        limit_per_entity: usize,
    ) -> Result<Vec<GraphConnection>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock();
        let mut all_connections = Vec::new();

        let mut stmt = conn.prepare(
            "SELECT rel_id, rel_type, from_name, from_canonical, from_label,
                    to_name, to_canonical, to_label, rel_properties, created_at
             FROM graph_connections
             WHERE from_id = ?1 OR to_id = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;

        for &eid in entity_ids {
            let connections = stmt
                .query_map(
                    params![eid, limit_per_entity as i64],
                    Self::row_to_connection,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            all_connections.extend(connections);
        }

        // Deduplicate by rel_id
        all_connections.sort_by_key(|c| c.rel_id);
        all_connections.dedup_by_key(|c| c.rel_id);

        Ok(all_connections)
    }

    /// Async wrapper: batch neighbors on the blocking threadpool.
    pub async fn neighbors_batch(
        &self,
        entity_ids: &[i64],
        limit_per_entity: usize,
    ) -> Result<Vec<GraphConnection>> {
        let this = self.clone();
        let ids = entity_ids.to_vec();
        tokio::task::spawn_blocking(move || this.neighbors_batch_sync(&ids, limit_per_entity))
            .await?
    }

    /// Find shortest path between two entities via BFS (synchronous, up to `max_hops`).
    ///
    /// Returns the chain of connections forming the path, or an empty vec
    /// if no path exists within the hop limit.
    pub fn find_path_sync(
        &self,
        from_id: i64,
        to_id: i64,
        max_hops: usize,
    ) -> Result<Vec<GraphConnection>> {
        if from_id == to_id {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock();

        // BFS in Rust over the SQLite data
        let mut visited: HashMap<i64, (i64, i64)> = HashMap::new(); // node → (prev_node, rel_id)
        visited.insert(from_id, (-1, -1));
        let mut frontier = vec![from_id];

        for _hop in 0..max_hops {
            if frontier.is_empty() {
                break;
            }

            let mut next_frontier = Vec::new();
            for &current in &frontier {
                // Query outgoing
                let mut stmt = conn.prepare_cached(
                    "SELECT id, target_id FROM graph_relationships WHERE source_id = ?1",
                )?;
                let outgoing: Vec<(i64, i64)> = stmt
                    .query_map(params![current], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;

                for (rel_id, neighbor) in outgoing {
                    if !visited.contains_key(&neighbor) {
                        visited.insert(neighbor, (current, rel_id));
                        if neighbor == to_id {
                            return Self::reconstruct_path(&conn, &visited, from_id, to_id);
                        }
                        next_frontier.push(neighbor);
                    }
                }

                // Query incoming (for undirected traversal)
                let mut stmt = conn.prepare_cached(
                    "SELECT id, source_id FROM graph_relationships WHERE target_id = ?1",
                )?;
                let incoming: Vec<(i64, i64)> = stmt
                    .query_map(params![current], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;

                for (rel_id, neighbor) in incoming {
                    if !visited.contains_key(&neighbor) {
                        visited.insert(neighbor, (current, rel_id));
                        if neighbor == to_id {
                            return Self::reconstruct_path(&conn, &visited, from_id, to_id);
                        }
                        next_frontier.push(neighbor);
                    }
                }
            }

            frontier = next_frontier;
        }

        Ok(Vec::new()) // No path found within max_hops
    }

    /// Async wrapper: find path on the blocking threadpool.
    pub async fn find_path(
        &self,
        from_id: i64,
        to_id: i64,
        max_hops: usize,
    ) -> Result<Vec<GraphConnection>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.find_path_sync(from_id, to_id, max_hops)).await?
    }

    /// Get graph statistics (synchronous).
    pub fn stats_sync(&self) -> Result<GraphStats> {
        let conn = self.conn.lock();
        let entity_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM graph_entities", [], |row| row.get(0))?;
        let relationship_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM graph_relationships", [], |row| {
                row.get(0)
            })?;
        let rel_type_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM graph_rel_types", [], |row| row.get(0))?;

        Ok(GraphStats {
            entities: entity_count as usize,
            relationships: relationship_count as usize,
            relationship_types: rel_type_count as usize,
        })
    }

    /// Async wrapper: stats on the blocking threadpool.
    pub async fn stats(&self) -> Result<GraphStats> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.stats_sync()).await?
    }

    /// Store a batch of entities and relationships in a single transaction (synchronous).
    ///
    /// Returns a map of canonical entity name → entity ID for the upserted entities.
    pub fn store_extraction_sync(
        &self,
        entities: &[super::graph_extract::ExtractedEntity],
        relationships: &[super::graph_extract::ExtractedRelationship],
    ) -> Result<HashMap<String, i64>> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        let mut entity_ids: HashMap<String, i64> = HashMap::new();

        // Phase 1: Upsert all entities
        for entity in entities {
            let canonical = Self::canonicalize(&entity.name);
            let label = if entity.label.is_empty() {
                "entity"
            } else {
                &entity.label
            };

            tx.execute(
                "INSERT INTO graph_entities (name, canonical_name, label)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(canonical_name, label) DO UPDATE SET
                     name = excluded.name,
                     updated_at = datetime('now')",
                params![entity.name, canonical, label],
            )?;

            let id: i64 = tx.query_row(
                "SELECT id FROM graph_entities WHERE canonical_name = ?1 AND label = ?2",
                params![canonical, label],
                |row| row.get(0),
            )?;

            entity_ids.insert(canonical, id);
        }

        // Phase 2: Insert relationships
        for rel in relationships {
            let from_canonical = Self::canonicalize(&rel.from);
            let to_canonical = Self::canonicalize(&rel.to);

            // Resolve or create source entity
            let source_id = if let Some(&id) = entity_ids.get(&from_canonical) {
                id
            } else {
                let from_label = rel.from_label.as_deref().unwrap_or("entity");
                tx.execute(
                    "INSERT INTO graph_entities (name, canonical_name, label)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(canonical_name, label) DO UPDATE SET
                         name = excluded.name,
                         updated_at = datetime('now')",
                    params![rel.from, from_canonical, from_label],
                )?;
                let id: i64 = tx.query_row(
                    "SELECT id FROM graph_entities WHERE canonical_name = ?1 AND label = ?2",
                    params![from_canonical, from_label],
                    |row| row.get(0),
                )?;
                entity_ids.insert(from_canonical, id);
                id
            };

            // Resolve or create target entity
            let target_id = if let Some(&id) = entity_ids.get(&to_canonical) {
                id
            } else {
                let to_label = rel.to_label.as_deref().unwrap_or("entity");
                tx.execute(
                    "INSERT INTO graph_entities (name, canonical_name, label)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(canonical_name, label) DO UPDATE SET
                         name = excluded.name,
                         updated_at = datetime('now')",
                    params![rel.to, to_canonical, to_label],
                )?;
                let id: i64 = tx.query_row(
                    "SELECT id FROM graph_entities WHERE canonical_name = ?1 AND label = ?2",
                    params![to_canonical, to_label],
                    |row| row.get(0),
                )?;
                entity_ids.insert(to_canonical, id);
                id
            };

            // Normalize the verb (need to drop the lock to call normalize_verb_sync,
            // but we're inside a transaction on the same connection, so we inline the logic)
            let trimmed = rel.rel_type.trim().to_lowercase();
            let rel_type = tx
                .query_row(
                    "SELECT canonical FROM graph_rel_aliases WHERE raw_verb = ?1",
                    params![trimmed],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| {
                    let underscored = trimmed.replace(' ', "_");
                    // Check if it's a known canonical type (best effort, default to related_to)
                    let exists: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM graph_rel_types WHERE canonical = ?1)",
                            params![underscored],
                            |row| row.get(0),
                        )
                        .unwrap_or(false);
                    if exists {
                        underscored
                    } else {
                        "related_to".into()
                    }
                });

            // Ensure the rel_type exists in graph_rel_types
            tx.execute(
                "INSERT OR IGNORE INTO graph_rel_types (canonical, category, directed)
                 VALUES (?1, 'auto', 1)",
                params![rel_type],
            )?;

            tx.execute(
                "INSERT OR IGNORE INTO graph_relationships (source_id, target_id, rel_type)
                 VALUES (?1, ?2, ?3)",
                params![source_id, target_id, rel_type],
            )?;
        }

        tx.commit()?;
        Ok(entity_ids)
    }

    // ── Private helpers ─────────────────────────────────────────

    fn row_to_entity(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entity> {
        let props_str: Option<String> = row.get(4)?;
        let properties = props_str.and_then(|s| serde_json::from_str(&s).ok());
        Ok(Entity {
            id: row.get(0)?,
            name: row.get(1)?,
            canonical_name: row.get(2)?,
            label: row.get(3)?,
            properties,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    }

    fn row_to_connection(row: &rusqlite::Row<'_>) -> rusqlite::Result<GraphConnection> {
        let props_str: Option<String> = row.get(8)?;
        let rel_properties = props_str.and_then(|s| serde_json::from_str(&s).ok());
        Ok(GraphConnection {
            rel_id: row.get(0)?,
            rel_type: row.get(1)?,
            from_name: row.get(2)?,
            from_canonical: row.get(3)?,
            from_label: row.get(4)?,
            to_name: row.get(5)?,
            to_canonical: row.get(6)?,
            to_label: row.get(7)?,
            rel_properties,
            created_at: row.get(9)?,
        })
    }

    fn reconstruct_path(
        conn: &Connection,
        visited: &HashMap<i64, (i64, i64)>,
        from_id: i64,
        to_id: i64,
    ) -> Result<Vec<GraphConnection>> {
        let mut path_rels = Vec::new();
        let mut current = to_id;

        while current != from_id {
            let &(prev, rel_id) = visited
                .get(&current)
                .context("BFS path reconstruction failed: node not in visited set")?;

            let connection = conn.query_row(
                "SELECT rel_id, rel_type, from_name, from_canonical, from_label,
                        to_name, to_canonical, to_label, rel_properties, created_at
                 FROM graph_connections WHERE rel_id = ?1",
                params![rel_id],
                Self::row_to_connection,
            )?;
            path_rels.push(connection);
            current = prev;
        }

        path_rels.reverse();
        Ok(path_rels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_graph() -> (TempDir, KnowledgeGraph) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test_brain.db");
        let graph = KnowledgeGraph::new(&db_path).unwrap();
        (tmp, graph)
    }

    #[test]
    fn canonicalize_normalizes_names() {
        assert_eq!(KnowledgeGraph::canonicalize("  John Doe  "), "john_doe");
        assert_eq!(KnowledgeGraph::canonicalize("Rust"), "rust");
        assert_eq!(
            KnowledgeGraph::canonicalize("  multiple   spaces  "),
            "multiple_spaces"
        );
    }

    #[test]
    fn upsert_entity_creates_and_updates() {
        let (_tmp, graph) = test_graph();

        let id1 = graph.upsert_entity_sync("Jasmin", "person", None).unwrap();
        let id2 = graph.upsert_entity_sync("jasmin", "person", None).unwrap();
        assert_eq!(
            id1, id2,
            "upsert should return same ID for same canonical name+label"
        );

        let entity = graph
            .find_entity_sync("jasmin", Some("person"))
            .unwrap()
            .unwrap();
        assert_eq!(entity.name, "jasmin");
    }

    #[test]
    fn insert_relationship_normalizes_verb() {
        let (_tmp, graph) = test_graph();

        let jasmin_id = graph.upsert_entity_sync("Jasmin", "person", None).unwrap();
        let niko_id = graph.upsert_entity_sync("Niko", "person", None).unwrap();

        // "dating" should normalize to "partner_of"
        let rel = graph
            .insert_relationship_sync(jasmin_id, niko_id, "dating", None)
            .unwrap();
        assert!(rel.is_some());

        let connections = graph.neighbors_sync(jasmin_id, 10).unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].rel_type, "partner_of");
    }

    #[test]
    fn duplicate_relationship_is_ignored() {
        let (_tmp, graph) = test_graph();

        let a = graph.upsert_entity_sync("A", "entity", None).unwrap();
        let b = graph.upsert_entity_sync("B", "entity", None).unwrap();

        let first = graph.insert_relationship_sync(a, b, "likes", None).unwrap();
        let second = graph.insert_relationship_sync(a, b, "likes", None).unwrap();
        assert!(first.is_some());
        assert!(second.is_none(), "duplicate should return None");
    }

    #[test]
    fn search_entities_fts() {
        let (_tmp, graph) = test_graph();
        graph.upsert_entity_sync("Jasmin", "person", None).unwrap();
        graph
            .upsert_entity_sync("JavaScript", "language", None)
            .unwrap();
        graph.upsert_entity_sync("Rust", "language", None).unwrap();

        let results = graph.search_entities_sync("jas", 10).unwrap();
        // FTS5 prefix match: "jas*" matches "jasmin" in both name and canonical_name columns
        assert!(!results.is_empty(), "should find at least one entity");
        assert!(
            results.iter().any(|e| e.name == "Jasmin"),
            "should find Jasmin"
        );

        // Search for "rust" should find only Rust
        let rust_results = graph.search_entities_sync("rust", 10).unwrap();
        assert_eq!(rust_results.len(), 1);
        assert_eq!(rust_results[0].name, "Rust");
    }

    #[test]
    fn neighbors_returns_both_directions() {
        let (_tmp, graph) = test_graph();
        let a = graph.upsert_entity_sync("A", "entity", None).unwrap();
        let b = graph.upsert_entity_sync("B", "entity", None).unwrap();
        let c = graph.upsert_entity_sync("C", "entity", None).unwrap();

        graph.insert_relationship_sync(a, b, "likes", None).unwrap();
        graph.insert_relationship_sync(c, a, "uses", None).unwrap();

        let connections = graph.neighbors_sync(a, 10).unwrap();
        assert_eq!(connections.len(), 2);
    }

    #[test]
    fn find_path_simple() {
        let (_tmp, graph) = test_graph();
        let a = graph.upsert_entity_sync("A", "entity", None).unwrap();
        let b = graph.upsert_entity_sync("B", "entity", None).unwrap();
        let c = graph.upsert_entity_sync("C", "entity", None).unwrap();

        graph.insert_relationship_sync(a, b, "likes", None).unwrap();
        graph.insert_relationship_sync(b, c, "uses", None).unwrap();

        let path = graph.find_path_sync(a, c, 3).unwrap();
        assert_eq!(path.len(), 2);
    }

    #[test]
    fn stats_reflects_data() {
        let (_tmp, graph) = test_graph();
        let initial = graph.stats_sync().unwrap();
        assert_eq!(initial.entities, 0);
        assert_eq!(initial.relationships, 0);

        let a = graph.upsert_entity_sync("A", "entity", None).unwrap();
        let b = graph.upsert_entity_sync("B", "entity", None).unwrap();
        graph.insert_relationship_sync(a, b, "likes", None).unwrap();

        let after = graph.stats_sync().unwrap();
        assert_eq!(after.entities, 2);
        assert_eq!(after.relationships, 1);
    }

    #[test]
    fn delete_entity_cascades_relationships() {
        let (_tmp, graph) = test_graph();
        let a = graph.upsert_entity_sync("A", "entity", None).unwrap();
        let b = graph.upsert_entity_sync("B", "entity", None).unwrap();
        graph.insert_relationship_sync(a, b, "likes", None).unwrap();

        graph.delete_entity_sync(a).unwrap();

        let stats = graph.stats_sync().unwrap();
        assert_eq!(stats.entities, 1);
        assert_eq!(stats.relationships, 0);
    }

    #[test]
    fn store_extraction_sync_batches_in_transaction() {
        use super::super::graph_extract::{ExtractedEntity, ExtractedRelationship};

        let (_tmp, graph) = test_graph();

        let entities = vec![
            ExtractedEntity {
                name: "Jasmin".into(),
                label: "person".into(),
            },
            ExtractedEntity {
                name: "Rust".into(),
                label: "technology".into(),
            },
        ];
        let relationships = vec![ExtractedRelationship {
            from: "Jasmin".into(),
            to: "Rust".into(),
            rel_type: "likes".into(),
            from_label: Some("person".into()),
            to_label: Some("technology".into()),
        }];

        let ids = graph
            .store_extraction_sync(&entities, &relationships)
            .unwrap();
        assert_eq!(ids.len(), 2);

        let stats = graph.stats_sync().unwrap();
        assert_eq!(stats.entities, 2);
        assert_eq!(stats.relationships, 1);
    }
}
