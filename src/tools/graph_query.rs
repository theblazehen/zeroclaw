//! Agent tool for querying the knowledge graph.
//!
//! Supports: entity search (FTS5), neighbor traversal, path finding, and stats.

use super::traits::{Tool, ToolResult};
use crate::memory::graph::KnowledgeGraph;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct GraphQueryTool {
    graph: Arc<KnowledgeGraph>,
}

impl GraphQueryTool {
    pub fn new(graph: Arc<KnowledgeGraph>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl Tool for GraphQueryTool {
    fn name(&self) -> &str {
        "graph_query"
    }

    fn description(&self) -> &str {
        "Search the knowledge graph for entities and their relationships. \
         Use 'search' to find entities by name, 'neighbors' to get an entity's \
         connections, 'path' to find how two entities are related, or 'stats' \
         for graph statistics."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["search", "neighbors", "path", "stats"],
                    "description": "Query type: 'search' (find entities by name), 'neighbors' (get connections for an entity), 'path' (find path between two entities), 'stats' (graph statistics)"
                },
                "query": {
                    "type": "string",
                    "description": "Entity name or search query (for 'search' and 'neighbors')"
                },
                "from": {
                    "type": "string",
                    "description": "Source entity name (for 'path')"
                },
                "to": {
                    "type": "string",
                    "description": "Target entity name (for 'path')"
                },
                "label": {
                    "type": "string",
                    "description": "Optional entity label filter (e.g., 'person', 'technology')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return (default: 10)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        match action {
            "search" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'search' requires 'query' parameter"))?;

                let entities = self.graph.search_entities(query, limit).await?;
                if entities.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: format!("No entities found matching '{query}'"),
                        error: None,
                    });
                }

                let mut output = format!("Found {} entities:\n", entities.len());
                for entity in &entities {
                    output.push_str(&format!(
                        "  - {} [{}] (id: {})\n",
                        entity.name, entity.label, entity.id
                    ));
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }

            "neighbors" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'neighbors' requires 'query' parameter"))?;
                let label = args.get("label").and_then(|v| v.as_str());

                let entity = self.graph.find_entity(query, label).await?;
                let entity = match entity {
                    Some(e) => e,
                    None => {
                        return Ok(ToolResult {
                            success: true,
                            output: format!("Entity '{query}' not found in the graph"),
                            error: None,
                        })
                    }
                };

                let connections = self.graph.neighbors(entity.id, limit).await?;
                if connections.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Entity '{}' [{}] exists but has no connections",
                            entity.name, entity.label
                        ),
                        error: None,
                    });
                }

                let mut output = format!(
                    "Connections for '{}' [{}] ({} found):\n",
                    entity.name,
                    entity.label,
                    connections.len()
                );
                for conn in &connections {
                    output.push_str(&format!("  - {conn}\n"));
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }

            "path" => {
                let from = args
                    .get("from")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'path' requires 'from' parameter"))?;
                let to = args
                    .get("to")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'path' requires 'to' parameter"))?;

                let from_entity = self.graph.find_entity(from, None).await?;
                let to_entity = self.graph.find_entity(to, None).await?;

                let (from_entity, to_entity) = match (from_entity, to_entity) {
                    (Some(f), Some(t)) => (f, t),
                    (None, _) => {
                        return Ok(ToolResult {
                            success: true,
                            output: format!("Entity '{from}' not found"),
                            error: None,
                        })
                    }
                    (_, None) => {
                        return Ok(ToolResult {
                            success: true,
                            output: format!("Entity '{to}' not found"),
                            error: None,
                        })
                    }
                };

                let path = self
                    .graph
                    .find_path(from_entity.id, to_entity.id, 4)
                    .await?;
                if path.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: format!(
                            "No path found between '{}' and '{}' (within 4 hops)",
                            from_entity.name, to_entity.name
                        ),
                        error: None,
                    });
                }

                let mut output = format!(
                    "Path from '{}' to '{}' ({} hops):\n",
                    from_entity.name,
                    to_entity.name,
                    path.len()
                );
                for (i, conn) in path.iter().enumerate() {
                    output.push_str(&format!("  {}. {conn}\n", i + 1));
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }

            "stats" => {
                let stats = self.graph.stats().await?;
                Ok(ToolResult {
                    success: true,
                    output: format!("Knowledge graph: {stats}"),
                    error: None,
                })
            }

            other => Ok(ToolResult {
                success: true,
                output: format!("Unknown action '{other}'. Use: search, neighbors, path, stats"),
                error: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_graph() -> (TempDir, Arc<KnowledgeGraph>) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test_brain.db");
        let graph = Arc::new(KnowledgeGraph::new(&db_path).unwrap());
        (tmp, graph)
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, graph) = test_graph();
        let tool = GraphQueryTool::new(graph);
        assert_eq!(tool.name(), "graph_query");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn search_empty_graph() {
        let (_tmp, graph) = test_graph();
        let tool = GraphQueryTool::new(graph);
        let result = tool
            .execute(json!({"action": "search", "query": "nobody"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No entities found"));
    }

    #[tokio::test]
    async fn search_finds_entities() {
        let (_tmp, graph) = test_graph();
        graph.upsert_entity("Jasmin", "person", None).await.unwrap();
        graph
            .upsert_entity("JavaScript", "language", None)
            .await
            .unwrap();

        let tool = GraphQueryTool::new(graph);
        let result = tool
            .execute(json!({"action": "search", "query": "jas"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Jasmin"));
    }

    #[tokio::test]
    async fn stats_action() {
        let (_tmp, graph) = test_graph();
        let tool = GraphQueryTool::new(graph);
        let result = tool.execute(json!({"action": "stats"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Knowledge graph:"));
    }

    #[tokio::test]
    async fn neighbors_of_nonexistent_entity() {
        let (_tmp, graph) = test_graph();
        let tool = GraphQueryTool::new(graph);
        let result = tool
            .execute(json!({"action": "neighbors", "query": "ghost"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn missing_action_returns_error() {
        let (_tmp, graph) = test_graph();
        let tool = GraphQueryTool::new(graph);
        let result = tool.execute(json!({"query": "test"})).await;
        assert!(result.is_err());
    }
}
