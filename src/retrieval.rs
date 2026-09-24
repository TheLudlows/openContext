use crate::{
    db,
    error::{AppError, Result},
    parsing,
    service::Service,
    types::*,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

const SELECT: &str = "SELECT a.id asset_id,v.version,c.id chunk_id,a.kind,v.title,c.content,c.locator,v.source_event_id";
const FROM: &str = " FROM oc.chunks c JOIN oc.assets a ON a.id=c.asset_id AND a.tenant_id=c.tenant_id AND a.workspace_id=c.workspace_id AND a.current_version=c.version JOIN oc.versions v ON v.asset_id=c.asset_id AND v.version=c.version AND v.tenant_id=c.tenant_id AND v.workspace_id=c.workspace_id JOIN oc.events e ON e.id=v.source_event_id AND e.tenant_id=v.tenant_id AND e.workspace_id=v.workspace_id WHERE NOT a.deleted AND e.state='active'";

impl Service {
    pub async fn search(&self, a: &AuthContext, input: SearchInput) -> Result<Value> {
        if input.query.trim().is_empty()
            || input.query.len() > 4000
            || input.query.contains('\0')
            || !(1..=100).contains(&input.limit)
            || !["keyword", "vector", "hybrid"].contains(&input.mode.as_str())
        {
            return Err(AppError::Invalid("query, mode or limit invalid".into()));
        }
        db::authorized(&self.pool, a, "read", false)
            .await?
            .rollback()
            .await?;
        let mut warnings = Vec::new();
        let mut effective = input.mode.clone();
        let vector = if input.mode != "keyword" {
            match self.models.embed(&input.query).await {
                Ok(v) => Some(v),
                Err(_) if input.allow_partial => {
                    warnings.push("embedding unavailable; used keyword retrieval");
                    effective = "keyword".into();
                    None
                }
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let mut tx = db::authorized(&self.pool, a, "read", false).await?;
        let mut branches: Vec<Vec<SearchHit>> = Vec::new();
        if effective != "vector" {
            let query = parsing::lexical(&input.query);
            let sql = format!(
                "{SELECT},ts_rank_cd(c.search_vector,plainto_tsquery('simple',$1))::float8 score {FROM} AND c.search_vector @@ plainto_tsquery('simple',$1) ORDER BY score DESC,c.id LIMIT 100"
            );
            branches.push(sqlx::query_as(&sql).bind(query).fetch_all(&mut *tx).await?);
        }
        if let Some(vector) = vector {
            // Exact scoped vector scan: no global ANN candidates or mixed model profiles.
            let sql = format!(
                "{SELECT},(1-(c.embedding <=> $1))::float8 score {FROM} AND c.embedding_profile=$2 AND c.embedding IS NOT NULL ORDER BY c.embedding <=> $1,c.id LIMIT 100"
            );
            branches.push(
                sqlx::query_as(&sql)
                    .bind(vector)
                    .bind(&self.models.profile)
                    .fetch_all(&mut *tx)
                    .await?,
            );
        }
        // Summary recall: a third RRF branch in hybrid mode, mapping summary hits back to their source chunk.
        if effective == "hybrid" {
            let summaries = crate::graph::search_summaries(&mut tx, &input.query, 20).await?;
            let chunk_ids: Vec<Uuid> = summaries
                .iter()
                .filter_map(|s| s["chunk_id"].as_str())
                .filter_map(|x| Uuid::parse_str(x).ok())
                .collect();
            if !chunk_ids.is_empty() {
                let sql = format!(
                    "{SELECT},0.0::float8 score {FROM} AND c.id=ANY($1) ORDER BY c.id LIMIT 100"
                );
                branches.push(
                    sqlx::query_as(&sql)
                        .bind(&chunk_ids)
                        .fetch_all(&mut *tx)
                        .await?,
                );
            }
        }
        let mut hits = fuse(branches, effective == "hybrid");
        // Final visibility check after all branches; revocation and deletion never rely on cached authorization.
        let valid: Vec<Uuid> = sqlx::query_scalar(&format!("SELECT c.id {FROM} AND c.id=ANY($1)"))
            .bind(hits.iter().map(|h| h.chunk_id).collect::<Vec<_>>())
            .fetch_all(&mut *tx)
            .await?;
        hits.retain(|h| valid.contains(&h.chunk_id));
        hits.truncate(input.limit);
        // Graph recall (entities + 1-hop relations) is a hybrid-only side band; it is reported
        // alongside hits but does not enter the chunk RRF ranking.
        let (graph_entities, graph_relations) = if effective == "hybrid" {
            let entities = crate::graph::search_entities(&mut tx, &input.query, 20).await?;
            let ids: Vec<Uuid> = entities
                .iter()
                .filter_map(|e| e["id"].as_str())
                .filter_map(|s| Uuid::parse_str(s).ok())
                .collect();
            let relations = crate::graph::relations_of(&mut tx, &ids).await?;
            (entities, relations)
        } else {
            (Vec::new(), Vec::new())
        };
        tx.commit().await?;
        db::authorized(&self.pool, a, "read", false)
            .await?
            .rollback()
            .await?;
        Ok(
            json!({"hits":hits,"requested_mode":input.mode,"effective_mode":effective,"warnings":warnings,"embedding_profile":self.models.profile,"retrieval_policy":"scoped-exact-rrf60-v1","graph":{"entities":graph_entities,"relations":graph_relations}}),
        )
    }
    pub async fn resolve(&self, a: &AuthContext, input: ResolveInput) -> Result<Value> {
        if input.budget_tokens > 32000 {
            return Err(AppError::Invalid("budget_tokens must be <=32000".into()));
        }
        let search = self
            .search(
                a,
                SearchInput {
                    query: input.query,
                    limit: 100,
                    mode: input.mode,
                    allow_partial: input.allow_partial,
                },
            )
            .await?;
        let hits: Vec<SearchHit> =
            serde_json::from_value(search["hits"].clone()).map_err(anyhow::Error::from)?;
        let (rendered, sources) = render(&hits, input.budget_tokens);
        // Graph fragments share the same conservative byte budget; cited as a side band.
        let graph_entities: Vec<Value> =
            serde_json::from_value(search["graph"]["entities"].clone()).unwrap_or_default();
        let graph_relations: Vec<Value> =
            serde_json::from_value(search["graph"]["relations"].clone()).unwrap_or_default();
        // Remaining budget after chunk rendering; whole graph blocks are kept or dropped, never partial.
        let remaining = input.budget_tokens.saturating_sub(rendered.len());
        let (graph_text, graph_sources) =
            crate::graph::render_graph(&graph_entities, &graph_relations, remaining);
        let rendered = format!("{rendered}{graph_text}");
        let mut all_sources = sources;
        all_sources.extend(graph_sources);
        Ok(
            json!({"rendered_context":rendered,"sources":all_sources,"budget_tokens":input.budget_tokens,"count":rendered.len(),"tokenizer":"utf8-bytes-upper-bound-v1","count_is_estimate":true,"effective_mode":search["effective_mode"],"warnings":search["warnings"],"graph":search["graph"]}),
        )
    }
}

fn fuse(branches: Vec<Vec<SearchHit>>, hybrid: bool) -> Vec<SearchHit> {
    let mut merged: HashMap<Uuid, SearchHit> = HashMap::new();
    for branch in branches {
        for (rank, mut hit) in branch.into_iter().enumerate() {
            if hybrid {
                hit.score = 1.0 / (60.0 + rank as f64 + 1.0);
            }
            merged
                .entry(hit.chunk_id)
                .and_modify(|old| old.score += hit.score)
                .or_insert(hit);
        }
    }
    let mut hits: Vec<_> = merged.into_values().collect();
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    hits
}
pub fn render(hits: &[SearchHit], budget: usize) -> (String, Vec<Value>) {
    // Explicit conservative byte budget, not a claim to model-specific tokenization.
    // Byte count includes all titles, separators and citation markers. No partial citation records.
    let mut rendered = String::new();
    let mut sources = Vec::new();
    for hit in hits {
        let marker = format!("[{}:{}:{}]", hit.asset_id, hit.version, hit.chunk_id);
        let block = format!("{marker} {}\n{}\n\n", hit.title, hit.content);
        if rendered.len() + block.len() > budget {
            continue;
        }
        rendered.push_str(&block);
        sources.push(json!({"citation":marker,"asset_id":hit.asset_id,"version":hit.version,"chunk_id":hit.chunk_id,"source_event_id":hit.source_event_id,"locator":hit.locator}));
    }
    (rendered, sources)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn budget_includes_citations_and_utf8() {
        let hit = SearchHit {
            asset_id: Uuid::new_v4(),
            version: 1,
            chunk_id: Uuid::new_v4(),
            kind: "memory".into(),
            title: "标题".into(),
            content: "中文内容".repeat(30),
            locator: json!({}),
            source_event_id: Uuid::new_v4(),
            score: 1.0,
        };
        for budget in [0, 10, 80, 500] {
            let (text, sources) = render(std::slice::from_ref(&hit), budget);
            assert!(text.len() <= budget);
            assert_eq!(text.is_empty(), sources.is_empty());
        }
    }
}
