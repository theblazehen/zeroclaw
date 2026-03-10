//! LLM-driven entity and relationship extraction for the knowledge graph.
//!
//! Two extraction modes:
//! 1. **Recall extraction** — extract entity names from a user message for graph lookup
//! 2. **Fact extraction** — extract entities + relationships from a conversation for storage

use crate::memory::graph::{GraphConnection, KnowledgeGraph};
use crate::providers::Provider;
use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── Extraction types ────────────────────────────────────────────────

/// Entity names extracted from a user message (for recall queries).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallEntities {
    pub entities: Vec<String>,
}

/// An entity extracted from a conversation (for storage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    #[serde(default = "default_entity_label")]
    pub label: String,
}

fn default_entity_label() -> String {
    "entity".into()
}

/// A relationship extracted from a conversation (for storage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelationship {
    pub from: String,
    pub to: String,
    pub rel_type: String,
    #[serde(default)]
    pub from_label: Option<String>,
    #[serde(default)]
    pub to_label: Option<String>,
}

/// Full extraction result from a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    #[serde(default)]
    pub relationships: Vec<ExtractedRelationship>,
}

// ── Recall: extract entity names from user message ──────────────────

const RECALL_EXTRACTION_PROMPT: &str = r#"Extract the key entity names (people, places, projects, technologies, organizations, concepts) mentioned or implied in this message. Return ONLY a JSON object with an "entities" array of strings.

Rules:
- Extract proper nouns and specific named things
- Include implied entities (e.g., "my partner" → include "partner" if you don't know the name)
- Do NOT include generic words (the, a, is, etc.)
- Do NOT include verbs or adjectives unless they're part of a name
- Return 1-5 entities maximum, ordered by relevance
- If no entities are found, return {"entities": []}

Example input: "What does Jasmin think about Rust?"
Example output: {"entities": ["Jasmin", "Rust"]}

Example input: "Tell me about the kubernetes project"
Example output: {"entities": ["kubernetes"]}

Message: "#;

/// Extract entity names from a user message for graph recall.
///
/// Calls a cheap LLM model to identify entity names, which are then
/// used to query the graph for relevant context.
pub async fn extract_recall_entities(
    provider: &dyn Provider,
    model: &str,
    user_message: &str,
    max_entities: usize,
) -> Result<Vec<String>> {
    let prompt = format!("{RECALL_EXTRACTION_PROMPT}{user_message}");

    let response = provider
        .simple_chat(&prompt, model, 0.0)
        .await
        .map_err(|e| {
            tracing::warn!("Graph recall extraction LLM call failed: {e}");
            e
        })?;

    let parsed = parse_json_from_response::<RecallEntities>(&response)?;

    Ok(parsed.entities.into_iter().take(max_entities).collect())
}

// ── Fact extraction: extract entities + relationships from conversation ─

const FACT_EXTRACTION_PROMPT: &str = r#"Extract entities and relationships from this conversation. Return ONLY a JSON object matching this schema:

{
  "entities": [{"name": "string", "label": "person|place|project|technology|organization|concept|entity"}],
  "relationships": [{"from": "entity_name", "to": "entity_name", "rel_type": "verb_phrase"}]
}

Rules:
- Extract FACTUAL statements only (things that are true, preferences, relationships)
- Do NOT extract opinions about hypotheticals or questions
- Use natural verb phrases for rel_type (e.g., "lives in", "likes", "works at", "partner of")
- Entity names should be as specific as possible (prefer "Jasmin" over "the user")
- Label entities with the most specific label that fits
- If the user states a preference, use the user's name (or "user" if unknown) as an entity
- Do NOT fabricate relationships not stated or strongly implied in the conversation
- Return {"entities": [], "relationships": []} if nothing factual is stated

Conversation:
"#;

/// Extract entities and relationships from a conversation, then store them.
///
/// This is the async post-turn extraction pipeline. It:
/// 1. Calls a cheap LLM to extract structured facts
/// 2. Upserts entities into the graph (in a single transaction)
/// 3. Normalizes verbs and inserts relationships
///
/// Designed to run in a `tokio::spawn` background task. Errors are
/// logged as warnings — extraction failures must never crash the main loop.
pub async fn extract_and_store(
    graph: &KnowledgeGraph,
    provider: &dyn Provider,
    model: &str,
    conversation: &[String],
) -> Result<ExtractionResult> {
    let conversation_text = conversation.join("\n");
    let prompt = format!("{FACT_EXTRACTION_PROMPT}{conversation_text}");

    let response = provider.simple_chat(&prompt, model, 0.0).await?;

    let extraction = parse_json_from_response::<ExtractionResult>(&response)?;

    // Use the transactional batch store
    let graph = graph.clone();
    let entities = extraction.entities.clone();
    let relationships = extraction.relationships.clone();
    tokio::task::spawn_blocking(move || graph.store_extraction_sync(&entities, &relationships))
        .await??;

    tracing::debug!(
        entities = extraction.entities.len(),
        relationships = extraction.relationships.len(),
        "Graph extraction complete"
    );

    Ok(extraction)
}

// ── JSON parsing helper ─────────────────────────────────────────────

/// Parse a JSON object from an LLM response, handling markdown code fences.
///
/// LLMs often wrap JSON in ```json ... ``` blocks. This strips those and
/// finds the first `{...}` in the response.
fn parse_json_from_response<T: serde::de::DeserializeOwned>(response: &str) -> Result<T> {
    let trimmed = response.trim();

    // Try direct parse first
    if let Ok(parsed) = serde_json::from_str(trimmed) {
        return Ok(parsed);
    }

    // Strip markdown code fences
    let stripped = if trimmed.starts_with("```") {
        let inner = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);
        inner.strip_suffix("```").unwrap_or(inner).trim()
    } else {
        trimmed
    };

    if let Ok(parsed) = serde_json::from_str(stripped) {
        return Ok(parsed);
    }

    // Last resort: find first { ... } block
    if let Some(start) = stripped.find('{') {
        if let Some(end) = stripped.rfind('}') {
            if end > start {
                let json_str = &stripped[start..=end];
                if let Ok(parsed) = serde_json::from_str(json_str) {
                    return Ok(parsed);
                }
            }
        }
    }

    anyhow::bail!(
        "Failed to parse JSON from LLM response: {}",
        &response[..response.len().min(200)]
    )
}

// ── Graph context formatting (for recall injection) ─────────────────

/// Build a formatted knowledge graph context string from connections.
///
/// Used by the pre-turn recall hook to inject graph knowledge into
/// the user message alongside `[Memory context]`.
pub fn format_graph_context(connections: &[GraphConnection]) -> String {
    if connections.is_empty() {
        return String::new();
    }

    let mut context = String::from("[Knowledge graph]\n");
    let mut seen = std::collections::HashSet::new();

    for conn in connections {
        // Deduplicate display lines
        let line = format!(
            "- {} → {} → {}\n",
            conn.from_name, conn.rel_type, conn.to_name
        );
        if seen.insert(line.clone()) {
            context.push_str(&line);
        }
    }

    context.push('\n');
    context
}

/// Build knowledge graph recall context for a user message.
///
/// Pipeline:
/// 1. Extract entity names from user message via LLM
/// 2. Search the graph for matching entities (FTS)
/// 3. Fetch neighbor connections for found entities
/// 4. Format as `[Knowledge graph]` context block
///
/// Returns empty string if graph has no relevant info or extraction fails.
pub async fn build_recall_context(
    graph: &KnowledgeGraph,
    provider: &dyn Provider,
    extraction_model: &str,
    user_message: &str,
    max_entities: usize,
    max_hops: usize,
) -> String {
    // Step 1: LLM entity extraction
    let entities =
        match extract_recall_entities(provider, extraction_model, user_message, max_entities).await
        {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return String::new(),
            Err(e) => {
                tracing::debug!("Graph recall entity extraction failed: {e}");
                return String::new();
            }
        };

    // Step 2: Resolve entity names to IDs via FTS search
    let mut entity_ids = Vec::new();
    for name in &entities {
        match graph.search_entities(name, 3).await {
            Ok(found) => {
                for entity in found {
                    entity_ids.push(entity.id);
                }
            }
            Err(e) => tracing::debug!("Graph entity search failed for '{name}': {e}"),
        }
    }

    if entity_ids.is_empty() {
        return String::new();
    }

    // Deduplicate
    entity_ids.sort_unstable();
    entity_ids.dedup();

    // Step 3: Get neighbor connections
    let limit_per_entity = (max_hops * 10).max(10);
    match graph.neighbors_batch(&entity_ids, limit_per_entity).await {
        Ok(connections) if !connections.is_empty() => format_graph_context(&connections),
        Ok(_) => String::new(),
        Err(e) => {
            tracing::debug!("Graph neighbor lookup failed: {e}");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_direct() {
        let input = r#"{"entities": ["Jasmin", "Rust"]}"#;
        let result: RecallEntities = parse_json_from_response(input).unwrap();
        assert_eq!(result.entities, vec!["Jasmin", "Rust"]);
    }

    #[test]
    fn parse_json_with_code_fence() {
        let input = "```json\n{\"entities\": [\"Jasmin\"]}\n```";
        let result: RecallEntities = parse_json_from_response(input).unwrap();
        assert_eq!(result.entities, vec!["Jasmin"]);
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let input = "Here are the entities:\n{\"entities\": [\"Rust\", \"ZeroClaw\"]}\nDone.";
        let result: RecallEntities = parse_json_from_response(input).unwrap();
        assert_eq!(result.entities, vec!["Rust", "ZeroClaw"]);
    }

    #[test]
    fn parse_json_extraction_result() {
        let input = r#"{
            "entities": [
                {"name": "Jasmin", "label": "person"},
                {"name": "Rust", "label": "technology"}
            ],
            "relationships": [
                {"from": "Jasmin", "to": "Rust", "rel_type": "likes"}
            ]
        }"#;
        let result: ExtractionResult = parse_json_from_response(input).unwrap();
        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.relationships.len(), 1);
        assert_eq!(result.relationships[0].rel_type, "likes");
    }

    #[test]
    fn parse_json_extraction_with_default_label() {
        let input = r#"{"entities": [{"name": "Foo"}], "relationships": []}"#;
        let result: ExtractionResult = parse_json_from_response(input).unwrap();
        assert_eq!(result.entities[0].label, "entity");
    }

    #[test]
    fn format_graph_context_empty() {
        assert_eq!(format_graph_context(&[]), "");
    }

    #[test]
    fn format_graph_context_deduplicates() {
        let connections = vec![
            GraphConnection {
                rel_id: 1,
                rel_type: "likes".into(),
                from_name: "Jasmin".into(),
                from_canonical: "jasmin".into(),
                from_label: "person".into(),
                to_name: "Rust".into(),
                to_canonical: "rust".into(),
                to_label: "technology".into(),
                rel_properties: None,
                created_at: "2026-01-01".into(),
            },
            GraphConnection {
                rel_id: 1,
                rel_type: "likes".into(),
                from_name: "Jasmin".into(),
                from_canonical: "jasmin".into(),
                from_label: "person".into(),
                to_name: "Rust".into(),
                to_canonical: "rust".into(),
                to_label: "technology".into(),
                rel_properties: None,
                created_at: "2026-01-01".into(),
            },
        ];

        let context = format_graph_context(&connections);
        assert!(context.contains("[Knowledge graph]"));
        assert_eq!(context.matches("Jasmin → likes → Rust").count(), 1);
    }
}
