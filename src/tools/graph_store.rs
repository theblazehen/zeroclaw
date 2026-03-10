//! Agent tool for storing entities and relationships in the knowledge graph.
//!
//! Supports: adding entities, adding relationships, deleting entities/relationships.

use super::traits::{Tool, ToolResult};
use crate::memory::graph::KnowledgeGraph;
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct GraphStoreTool {
    graph: Arc<KnowledgeGraph>,
    security: Arc<SecurityPolicy>,
}

impl GraphStoreTool {
    pub fn new(graph: Arc<KnowledgeGraph>, security: Arc<SecurityPolicy>) -> Self {
        Self { graph, security }
    }
}

#[async_trait]
impl Tool for GraphStoreTool {
    fn name(&self) -> &str {
        "graph_store"
    }

    fn description(&self) -> &str {
        "Store entities and relationships in the knowledge graph. \
         Use 'add_entity' to create/update an entity, 'add_relationship' to \
         connect two entities, or 'delete_entity'/'delete_relationship' to remove."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add_entity", "add_relationship", "delete_entity", "delete_relationship"],
                    "description": "Store action type"
                },
                "name": {
                    "type": "string",
                    "description": "Entity name (for 'add_entity', 'delete_entity')"
                },
                "label": {
                    "type": "string",
                    "description": "Entity label/type: person, place, project, technology, organization, concept (default: 'entity')"
                },
                "from": {
                    "type": "string",
                    "description": "Source entity name (for 'add_relationship')"
                },
                "to": {
                    "type": "string",
                    "description": "Target entity name (for 'add_relationship')"
                },
                "rel_type": {
                    "type": "string",
                    "description": "Relationship type / verb phrase (for 'add_relationship'). Will be auto-normalized."
                },
                "id": {
                    "type": "integer",
                    "description": "Entity or relationship ID (for delete operations)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Enforce security policy (write operation)
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "graph_store")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;

        match action {
            "add_entity" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'add_entity' requires 'name'"))?;
                let label = args
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("entity");

                let id = self.graph.upsert_entity(name, label, None).await?;
                Ok(ToolResult {
                    success: true,
                    output: format!("Entity '{name}' [{label}] stored (id: {id})"),
                    error: None,
                })
            }

            "add_relationship" => {
                let from = args
                    .get("from")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'add_relationship' requires 'from'"))?;
                let to = args
                    .get("to")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'add_relationship' requires 'to'"))?;
                let rel_type = args
                    .get("rel_type")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'add_relationship' requires 'rel_type'"))?;
                let from_label = args
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("entity");

                // Use the transactional batch store from graph_extract types
                let entities = vec![
                    crate::memory::graph_extract::ExtractedEntity {
                        name: from.to_string(),
                        label: from_label.to_string(),
                    },
                    crate::memory::graph_extract::ExtractedEntity {
                        name: to.to_string(),
                        label: "entity".to_string(),
                    },
                ];
                let relationships = vec![crate::memory::graph_extract::ExtractedRelationship {
                    from: from.to_string(),
                    to: to.to_string(),
                    rel_type: rel_type.to_string(),
                    from_label: Some(from_label.to_string()),
                    to_label: None,
                }];

                let graph = self.graph.clone();
                tokio::task::spawn_blocking(move || {
                    graph.store_extraction_sync(&entities, &relationships)
                })
                .await??;

                // Resolve the normalized verb for display
                let graph = self.graph.clone();
                let verb = rel_type.to_string();
                let normalized = tokio::task::spawn_blocking(move || {
                    graph.normalize_verb_sync(&verb)
                })
                .await??;

                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Relationship stored: {from} → {normalized} → {to}"
                    ),
                    error: None,
                })
            }

            "delete_entity" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("'delete_entity' requires 'id'"))?;

                if self.graph.delete_entity(id).await? {
                    Ok(ToolResult {
                        success: true,
                        output: format!("Entity {id} deleted (and all its relationships)"),
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: true,
                        output: format!("Entity {id} not found"),
                        error: None,
                    })
                }
            }

            "delete_relationship" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow::anyhow!("'delete_relationship' requires 'id'"))?;

                if self.graph.delete_relationship(id).await? {
                    Ok(ToolResult {
                        success: true,
                        output: format!("Relationship {id} deleted"),
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: true,
                        output: format!("Relationship {id} not found"),
                        error: None,
                    })
                }
            }

            other => Ok(ToolResult {
                success: true,
                output: format!(
                    "Unknown action '{other}'. Use: add_entity, add_relationship, delete_entity, delete_relationship"
                ),
                error: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;
    use tempfile::TempDir;

    fn test_graph() -> (TempDir, Arc<KnowledgeGraph>) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test_brain.db");
        let graph = Arc::new(KnowledgeGraph::new(&db_path).unwrap());
        (tmp, graph)
    }

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, graph) = test_graph();
        let tool = GraphStoreTool::new(graph, test_security());
        assert_eq!(tool.name(), "graph_store");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn add_entity() {
        let (_tmp, graph) = test_graph();
        let tool = GraphStoreTool::new(graph.clone(), test_security());
        let result = tool
            .execute(json!({"action": "add_entity", "name": "Jasmin", "label": "person"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Jasmin"));
        assert!(result.output.contains("[person]"));
    }

    #[tokio::test]
    async fn add_relationship() {
        let (_tmp, graph) = test_graph();
        let tool = GraphStoreTool::new(graph.clone(), test_security());

        let result = tool
            .execute(json!({
                "action": "add_relationship",
                "from": "Jasmin",
                "to": "Niko",
                "rel_type": "dating"
            }))
            .await
            .unwrap();
        assert!(result.success);
        // "dating" normalizes to "partner_of"
        assert!(result.output.contains("partner_of"));
    }

    #[tokio::test]
    async fn add_relationship_auto_creates_entities() {
        let (_tmp, graph) = test_graph();
        let tool = GraphStoreTool::new(graph.clone(), test_security());

        tool.execute(json!({
            "action": "add_relationship",
            "from": "Alpha",
            "to": "Beta",
            "rel_type": "uses"
        }))
        .await
        .unwrap();

        // Both entities should now exist
        let alpha = graph.find_entity("Alpha", None).await.unwrap();
        let beta = graph.find_entity("Beta", None).await.unwrap();
        assert!(alpha.is_some());
        assert!(beta.is_some());
    }

    #[tokio::test]
    async fn missing_action_errors() {
        let (_tmp, graph) = test_graph();
        let tool = GraphStoreTool::new(graph, test_security());
        let result = tool.execute(json!({"name": "test"})).await;
        assert!(result.is_err());
    }
}
