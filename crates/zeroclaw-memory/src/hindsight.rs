use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroclaw_api::attribution::{Attributable, MemoryKind, Role};
use zeroclaw_api::memory_traits::{ExportFilter, MemoryCategory, MemoryEntry};

use crate::traits::Memory;

#[derive(Debug, Clone)]
pub struct HindsightMemory {
    name: String,
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    tenant: String,
    bank_id: String,
    synchronous_retain: bool,
}

#[derive(Debug, Serialize)]
struct RetainRequest<'a> {
    items: Vec<RetainItem<'a>>,
    #[serde(rename = "async")]
    async_retain: bool,
}

#[derive(Debug, Serialize)]
struct RetainItem<'a> {
    content: &'a str,
    context: String,
    document_id: String,
    metadata: Value,
    tags: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RecallRequest<'a> {
    query: &'a str,
    budget: &'static str,
    max_tokens: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags_match: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
struct RecallResponse {
    #[serde(default)]
    results: Vec<HindsightResult>,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    items: Vec<HindsightResult>,
    total: usize,
}

#[derive(Debug, Deserialize)]
struct StatsResponse {
    total_nodes: usize,
}

#[derive(Debug, Deserialize)]
struct HindsightResult {
    id: String,
    #[serde(default)]
    text: String,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    #[serde(default)]
    mentioned_at: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    metadata: Value,
}

impl HindsightMemory {
    pub fn new(
        name: &str,
        base_url: &str,
        api_key: &str,
        tenant: &str,
        bank_id: &str,
        synchronous_retain: bool,
    ) -> anyhow::Result<Self> {
        let base_url = base_url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            anyhow::bail!(
                "Hindsight memory backend requires `url` in [storage.hindsight.<alias>] or HINDSIGHT_API_URL"
            );
        }
        let api_key = api_key.trim();
        if api_key.is_empty() {
            anyhow::bail!(
                "Hindsight memory backend requires `api_key` in [storage.hindsight.<alias>] or HINDSIGHT_API_KEY"
            );
        }
        let tenant = tenant.trim();
        if tenant.is_empty() {
            anyhow::bail!("Hindsight memory backend requires a non-empty tenant");
        }
        let bank_id = bank_id.trim();
        if bank_id.is_empty() {
            anyhow::bail!("Hindsight memory backend requires a non-empty bank_id");
        }

        Ok(Self {
            name: name.to_string(),
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            tenant: tenant.to_string(),
            bank_id: bank_id.to_string(),
            synchronous_retain,
        })
    }

    fn bank_url(&self) -> String {
        format!(
            "{}/v1/{}/banks/{}",
            self.base_url, self.tenant, self.bank_id
        )
    }

    fn metadata_key<'a>(metadata: &'a Value, key: &str) -> Option<&'a str> {
        metadata.get(key).and_then(Value::as_str)
    }

    fn category_to_hindsight(category: &MemoryCategory) -> String {
        match category {
            MemoryCategory::Core => "world".to_string(),
            MemoryCategory::Daily => "experience".to_string(),
            MemoryCategory::Conversation => "observation".to_string(),
            MemoryCategory::Custom(value) => value.clone(),
        }
    }

    fn category_from_hindsight(value: Option<&str>) -> MemoryCategory {
        match value.unwrap_or("world") {
            "world" => MemoryCategory::Core,
            "experience" => MemoryCategory::Daily,
            "observation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    fn tags(
        key: Option<&str>,
        category: Option<&MemoryCategory>,
        namespace: Option<&str>,
        session_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Vec<String> {
        let mut tags = vec!["zeroclaw".to_string()];
        if let Some(key) = key.filter(|value| !value.is_empty()) {
            tags.push(format!("key:{key}"));
        }
        if let Some(category) = category {
            tags.push(format!("category:{}", category));
        }
        if let Some(namespace) = namespace.filter(|value| !value.is_empty()) {
            tags.push(format!("namespace:{namespace}"));
        }
        if let Some(session_id) = session_id.filter(|value| !value.is_empty()) {
            tags.push(format!("session:{session_id}"));
        }
        if let Some(agent_id) = agent_id.filter(|value| !value.is_empty()) {
            tags.push(format!("agent:{agent_id}"));
        }
        tags
    }

    fn entry_from_result(result: HindsightResult) -> MemoryEntry {
        let key = Self::metadata_key(&result.metadata, "key")
            .map(ToString::to_string)
            .unwrap_or_else(|| result.id.clone());
        let namespace = Self::metadata_key(&result.metadata, "namespace")
            .unwrap_or("default")
            .to_string();
        let session_id =
            Self::metadata_key(&result.metadata, "session_id").map(ToString::to_string);
        let agent_id = Self::metadata_key(&result.metadata, "agent_id").map(ToString::to_string);
        let timestamp = result
            .mentioned_at
            .or(result.date)
            .unwrap_or_else(|| Utc::now().to_rfc3339());

        MemoryEntry {
            id: result.id,
            key,
            content: result.text,
            category: Self::category_from_hindsight(result.memory_type.as_deref()),
            timestamp,
            session_id,
            score: None,
            namespace,
            importance: result.metadata.get("importance").and_then(Value::as_f64),
            superseded_by: None,
            agent_alias: agent_id.clone(),
            agent_id,
        }
    }

    async fn send_recall(
        &self,
        request: &RecallRequest<'_>,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let response = self
            .client
            .post(format!("{}/memories/recall", self.bank_url()))
            .bearer_auth(&self.api_key)
            .json(request)
            .send()
            .await
            .context("failed to call Hindsight recall API")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Hindsight recall response")?;
        if !status.is_success() {
            anyhow::bail!("Hindsight recall failed with HTTP {status}: {body}");
        }
        let parsed: RecallResponse = serde_json::from_str(&body)
            .with_context(|| format!("failed to parse Hindsight recall response: {body}"))?;
        Ok(parsed
            .results
            .into_iter()
            .map(Self::entry_from_result)
            .take(limit)
            .collect())
    }

    async fn send_list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut request = self
            .client
            .get(format!("{}/memories/list", self.bank_url()))
            .bearer_auth(&self.api_key)
            .query(&[("limit", limit.to_string())]);
        if let Some(category) = category {
            request = request.query(&[("type", Self::category_to_hindsight(category))]);
        }
        let response = request
            .send()
            .await
            .context("failed to call Hindsight list API")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Hindsight list response")?;
        if !status.is_success() {
            anyhow::bail!("Hindsight list failed with HTTP {status}: {body}");
        }
        let parsed: ListResponse = serde_json::from_str(&body)
            .with_context(|| format!("failed to parse Hindsight list response: {body}"))?;
        let _ = parsed.total;
        let mut entries: Vec<_> = parsed
            .items
            .into_iter()
            .map(Self::entry_from_result)
            .collect();
        if let Some(session_id) = session_id {
            entries.retain(|entry| entry.session_id.as_deref() == Some(session_id));
        }
        entries.truncate(limit);
        Ok(entries)
    }

    async fn send_count(&self) -> anyhow::Result<usize> {
        let response = self
            .client
            .get(format!("{}/stats", self.bank_url()))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .context("failed to call Hindsight stats API")?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Hindsight stats response")?;
        if !status.is_success() {
            anyhow::bail!("Hindsight stats failed with HTTP {status}: {body}");
        }
        let parsed: StatsResponse = serde_json::from_str(&body)
            .with_context(|| format!("failed to parse Hindsight stats response: {body}"))?;
        Ok(parsed.total_nodes)
    }

    async fn ensure_bank(&self) -> anyhow::Result<()> {
        let response = self
            .client
            .put(self.bank_url())
            .bearer_auth(&self.api_key)
            .json(&json!({"retain_mission": "Retain ZeroClaw agent memories."}))
            .send()
            .await
            .context("failed to create or verify Hindsight bank")?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Hindsight bank setup failed with HTTP {status}: {body}")
        }
    }
}

impl Attributable for HindsightMemory {
    fn role(&self) -> Role {
        Role::Memory(MemoryKind::Hindsight)
    }

    fn alias(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl Memory for HindsightMemory {
    fn name(&self) -> &str {
        &self.name
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.store_with_agent(
            key,
            content,
            category,
            session_id,
            Some("default"),
            None,
            None,
        )
        .await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let tags = Self::tags(None, None, None, session_id, None);
        let request = RecallRequest {
            query,
            budget: "low",
            max_tokens: 2048,
            tags,
            tags_match: Some("all_strict"),
        };
        self.send_recall(&request, limit).await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let request = RecallRequest {
            query: key,
            budget: "low",
            max_tokens: 512,
            tags: Self::tags(Some(key), None, None, None, None),
            tags_match: Some("all_strict"),
        };
        Ok(self.send_recall(&request, 1).await?.into_iter().next())
    }

    async fn get_for_agent(
        &self,
        key: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let request = RecallRequest {
            query: key,
            budget: "low",
            max_tokens: 512,
            tags: Self::tags(Some(key), None, None, None, Some(agent_id)),
            tags_match: Some("all_strict"),
        };
        Ok(self.send_recall(&request, 1).await?.into_iter().next())
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.send_list(category, session_id, 100).await
    }

    async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
        anyhow::bail!("forget is not supported by the Hindsight memory backend")
    }

    async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
        anyhow::bail!("forget_for_agent is not supported by the Hindsight memory backend")
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.send_count().await
    }

    async fn health_check(&self) -> bool {
        let Ok(response) = self
            .client
            .get(format!("{}/version", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
        else {
            return false;
        };
        matches!(response.status(), StatusCode::OK)
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
    ) -> anyhow::Result<()> {
        self.store_with_agent(
            key, content, category, session_id, namespace, importance, None,
        )
        .await
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.ensure_bank().await?;
        let namespace = namespace.unwrap_or("default");
        let memory_type = Self::category_to_hindsight(&category);
        let document_id = format!("zeroclaw-{key}");
        let mut metadata = json!({
            "key": key,
            "category": category.to_string(),
            "namespace": namespace,
        });
        if let Some(session_id) = session_id {
            metadata["session_id"] = json!(session_id);
        }
        if let Some(importance) = importance {
            metadata["importance"] = json!(importance);
        }
        if let Some(agent_id) = agent_id {
            metadata["agent_id"] = json!(agent_id);
        }
        let item = RetainItem {
            content,
            context: format!("ZeroClaw {memory_type} memory"),
            document_id,
            metadata,
            tags: Self::tags(
                Some(key),
                Some(&category),
                Some(namespace),
                session_id,
                agent_id,
            ),
        };
        let request = RetainRequest {
            items: vec![item],
            async_retain: !self.synchronous_retain,
        };
        let response = self
            .client
            .post(format!("{}/memories", self.bank_url()))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .context("failed to call Hindsight retain API")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(())
        } else {
            anyhow::bail!("Hindsight retain failed with HTTP {status}: {body}")
        }
    }

    async fn recall_namespaced(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let request = RecallRequest {
            query,
            budget: "low",
            max_tokens: 2048,
            tags: Self::tags(None, None, Some(namespace), session_id, None),
            tags_match: Some("all_strict"),
        };
        self.send_recall(&request, limit).await
    }

    async fn export(&self, filter: &ExportFilter) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self
            .send_list(filter.category.as_ref(), filter.session_id.as_deref(), 1000)
            .await?;
        if let Some(namespace) = filter.namespace.as_deref() {
            entries.retain(|entry| entry.namespace == namespace);
        }
        entries.retain(|entry| {
            if let Some(since) = filter.since.as_deref()
                && entry.timestamp.as_str() < since
            {
                return false;
            }
            if let Some(until) = filter.until.as_deref()
                && entry.timestamp.as_str() > until
            {
                return false;
            }
            true
        });
        Ok(entries)
    }

    async fn recall_for_agents(
        &self,
        allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        if allowed_agent_ids.is_empty() {
            return self.recall(query, limit, session_id, None, None).await;
        }
        let mut entries = Vec::new();
        for agent_id in allowed_agent_ids {
            let request = RecallRequest {
                query,
                budget: "low",
                max_tokens: 2048,
                tags: Self::tags(None, None, None, session_id, Some(agent_id)),
                tags_match: Some("all_strict"),
            };
            entries.extend(self.send_recall(&request, limit).await?);
            if entries.len() >= limit {
                break;
            }
        }
        entries.truncate(limit);
        Ok(entries)
    }
}
