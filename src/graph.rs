use crate::{
    db::Tx,
    error::{AppError, Result},
    types::{AuthContext, Entity, GraphExtraction, Relation},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

pub fn entity_id(name: &str) -> Uuid {
    let digest = Sha256::digest(name.trim().to_lowercase().as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

pub fn relation_id(source: Uuid, predicate: &str, target: Uuid) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hasher.update(predicate.trim().to_lowercase().as_bytes());
    hasher.update(target.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

pub fn parse_graph_extraction(value: &Value) -> Result<GraphExtraction> {
    let entities: Vec<Entity> = serde_json::from_value(value["entities"].clone())
        .map_err(|_| AppError::Unavailable("invalid graph extraction: entities".into()))?;
    let relations: Vec<Relation> = serde_json::from_value(value["relations"].clone())
        .map_err(|_| AppError::Unavailable("invalid graph extraction: relations".into()))?;
    if entities.len() > 20 || relations.len() > 20 {
        return Err(AppError::Unavailable("too many graph items".into()));
    }
    let mut names = HashSet::new();
    for entity in &entities {
        if entity.name.trim().is_empty() || entity.name.len() > 256 || entity.name.contains('\0') {
            return Err(AppError::Unavailable("invalid entity name".into()));
        }
        if entity.entity_type.len() > 128 || entity.description.len() > 4000 {
            return Err(AppError::Unavailable("invalid entity fields".into()));
        }
        names.insert(entity.name.trim().to_lowercase());
    }
    for relation in &relations {
        if relation.predicate.trim().is_empty()
            || relation.predicate.len() > 256
            || relation.predicate.contains('\0')
        {
            return Err(AppError::Unavailable("invalid relation predicate".into()));
        }
        if !names.contains(&relation.source.trim().to_lowercase())
            || !names.contains(&relation.target.trim().to_lowercase())
        {
            return Err(AppError::Unavailable(
                "relation references unknown entity".into(),
            ));
        }
    }
    Ok(GraphExtraction {
        entities,
        relations,
    })
}

pub async fn upsert_entity(tx: &mut Tx, a: &AuthContext, e: &Entity) -> Result<Uuid> {
    let id = entity_id(&e.name);
    let lexical = crate::parsing::lexical(&format!("{} {}", e.name, e.description));
    let existing: Uuid = sqlx::query_scalar(
        "INSERT INTO oc.entities(tenant_id,workspace_id,id,canonical_name,type,description,search_vector) VALUES($1,$2,$3,$4,$5,$6,to_tsvector('simple',$7)) ON CONFLICT (tenant_id,workspace_id,id) DO UPDATE SET canonical_name=excluded.canonical_name RETURNING id",
    )
    .bind(a.tenant_id)
    .bind(a.workspace_id)
    .bind(id)
    .bind(&e.name)
    .bind(&e.entity_type)
    .bind(&e.description)
    .bind(lexical)
    .fetch_one(&mut **tx)
    .await?;
    Ok(existing)
}

pub async fn upsert_relation(
    tx: &mut Tx,
    a: &AuthContext,
    id: Uuid,
    source: Uuid,
    predicate: &str,
    target: Uuid,
    fact_text: &str,
) -> Result<Uuid> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO oc.relations(tenant_id,workspace_id,id,source_entity,predicate,target_entity,fact_text) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (tenant_id,workspace_id,id) DO NOTHING RETURNING id",
    )
    .bind(a.tenant_id)
    .bind(a.workspace_id)
    .bind(id)
    .bind(source)
    .bind(predicate)
    .bind(target)
    .bind(fact_text)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(existing.unwrap_or(id))
}

pub async fn add_owner(
    tx: &mut Tx,
    a: &AuthContext,
    source: Uuid,
    kind: &str,
    id: Uuid,
) -> Result<()> {
    match kind {
        "entity" => sqlx::query(
            "INSERT INTO oc.entity_owners(tenant_id,workspace_id,source_event_id,entity_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(a.tenant_id)
        .bind(a.workspace_id)
        .bind(source)
        .bind(id)
        .execute(&mut **tx)
        .await?,
        _ => sqlx::query(
            "INSERT INTO oc.relation_owners(tenant_id,workspace_id,source_event_id,relation_id) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(a.tenant_id)
        .bind(a.workspace_id)
        .bind(source)
        .bind(id)
        .execute(&mut **tx)
        .await?,
    };
    Ok(())
}

pub async fn write_graph(
    tx: &mut Tx,
    a: &AuthContext,
    source: Uuid,
    g: &GraphExtraction,
) -> Result<Value> {
    let mut ids = std::collections::HashMap::new();
    let mut entity_rows = Vec::new();
    for e in &g.entities {
        let id = upsert_entity(tx, a, e).await?;
        add_owner(tx, a, source, "entity", id).await?;
        ids.insert(e.name.trim().to_lowercase(), id);
        entity_rows.push(json!({"id": id, "name": e.name, "type": e.entity_type}));
    }
    let mut relation_rows = Vec::new();
    for r in &g.relations {
        let src = *ids
            .get(&r.source.trim().to_lowercase())
            .ok_or_else(|| AppError::Unavailable("relation source not found".into()))?;
        let dst = *ids
            .get(&r.target.trim().to_lowercase())
            .ok_or_else(|| AppError::Unavailable("relation target not found".into()))?;
        let fact = format!("{} {} {}", r.source, r.predicate, r.target);
        let rid = relation_id(src, &r.predicate, dst);
        upsert_relation(tx, a, rid, src, &r.predicate, dst, &fact).await?;
        add_owner(tx, a, source, "relation", rid).await?;
        relation_rows
            .push(json!({"id": rid, "source": src, "predicate": r.predicate, "target": dst}));
    }
    Ok(json!({"entities": entity_rows, "relations": relation_rows}))
}

pub async fn search_entities(tx: &mut Tx, query: &str, limit: usize) -> Result<Vec<Value>> {
    let lexical = crate::parsing::lexical(query);
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'name',canonical_name,'type',type,'description',description) FROM oc.entities WHERE search_vector @@ plainto_tsquery('simple',$1) ORDER BY id LIMIT $2",
    )
    .bind(lexical)
    .bind(limit as i64)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows)
}

pub async fn relations_of(tx: &mut Tx, ids: &[Uuid]) -> Result<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',id,'source',source_entity,'predicate',predicate,'target',target_entity,'fact_text',fact_text) FROM oc.relations WHERE source_entity=ANY($1) OR target_entity=ANY($1) ORDER BY id LIMIT 100",
    )
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows)
}

pub async fn search_summaries(tx: &mut Tx, query: &str, limit: usize) -> Result<Vec<Value>> {
    let lexical = crate::parsing::lexical(query);
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id',s.id,'chunk_id',s.chunk_id,'text',s.text) FROM oc.summaries s WHERE s.search_vector @@ plainto_tsquery('simple',$1) ORDER BY s.id LIMIT $2",
    )
    .bind(lexical)
    .bind(limit as i64)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows)
}

/// Render graph fragments under a conservative byte budget, mirroring `retrieval::render`:
/// whole blocks are kept or dropped, never partial citation records.
pub fn render_graph(
    entities: &[Value],
    relations: &[Value],
    budget: usize,
) -> (String, Vec<Value>) {
    let mut rendered = String::new();
    let mut sources = Vec::new();
    for e in entities {
        let name = e["name"].as_str().unwrap_or("");
        let block = format!("[entity] {name}\n");
        if rendered.len() + block.len() > budget {
            break;
        }
        rendered.push_str(&block);
        let id = e["id"].as_str().unwrap_or("");
        sources.push(json!({"citation": format!("[entity:{id}]"), "entity_id": id}));
    }
    for r in relations {
        let fact = r["fact_text"].as_str().unwrap_or("");
        let block = format!("[relation] {fact}\n");
        if rendered.len() + block.len() > budget {
            break;
        }
        rendered.push_str(&block);
        let id = r["id"].as_str().unwrap_or("");
        sources.push(json!({"citation": format!("[relation:{id}]"), "relation_id": id}));
    }
    (rendered, sources)
}

/// Provenance-aware orphan cleanup: retracting a source first detaches its owners,
/// then deletes relations/entities that no longer have any owner. Shared graph
/// objects (with owners from other still-active sources) are retained.
///
/// Runs under the active RLS scope; does not take `&AuthContext` because visibility
/// is enforced by `tenant_id`/`workspace_id` session config.
pub async fn cleanup_orphans(tx: &mut Tx) -> Result<Value> {
    // Detach owners for sources whose event has been retracted.
    sqlx::query(
        "DELETE FROM oc.entity_owners o WHERE EXISTS(SELECT 1 FROM oc.events e WHERE e.id=o.source_event_id AND e.tenant_id=o.tenant_id AND e.workspace_id=o.workspace_id AND e.state='retracted')",
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "DELETE FROM oc.relation_owners o WHERE EXISTS(SELECT 1 FROM oc.events e WHERE e.id=o.source_event_id AND e.tenant_id=o.tenant_id AND e.workspace_id=o.workspace_id AND e.state='retracted')",
    )
    .execute(&mut **tx)
    .await?;
    // Delete relations first: relations reference entities via foreign keys.
    let relations: u64 = sqlx::query(
        "DELETE FROM oc.relations r USING (SELECT id FROM oc.relations x WHERE NOT EXISTS(SELECT 1 FROM oc.relation_owners o WHERE o.relation_id=x.id AND o.tenant_id=x.tenant_id AND o.workspace_id=x.workspace_id)) dead WHERE r.id=dead.id AND r.tenant_id=dead.tenant_id AND r.workspace_id=dead.workspace_id",
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let entities: u64 = sqlx::query(
        "DELETE FROM oc.entities e USING (SELECT id FROM oc.entities y WHERE NOT EXISTS(SELECT 1 FROM oc.entity_owners o WHERE o.entity_id=y.id AND o.tenant_id=y.tenant_id AND o.workspace_id=y.workspace_id)) dead WHERE e.id=dead.id AND e.tenant_id=dead.tenant_id AND e.workspace_id=dead.workspace_id",
    )
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(json!({"relations_removed": relations, "entities_removed": entities}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_stable() {
        assert_eq!(entity_id(" Atlas "), entity_id("atlas"));
    }
    #[test]
    fn rejects_unknown_relation() {
        let value = json!({"entities":[{"name":"A","entity_type":"x","description":""}],"relations":[{"source":"A","predicate":"knows","target":"B"}]});
        assert!(parse_graph_extraction(&value).is_err());
    }
    #[test]
    fn accepts_graph() {
        let value = json!({"entities":[{"name":"A","entity_type":"x","description":""},{"name":"B","entity_type":"y","description":""}],"relations":[{"source":"A","predicate":"knows","target":"B"}]});
        assert_eq!(parse_graph_extraction(&value).unwrap().relations.len(), 1);
    }
    #[test]
    fn render_graph_respects_budget_and_cites() {
        let entities = vec![json!({"id":"e1","name":"Atlas"})];
        let relations = vec![json!({"id":"r1","fact_text":"Atlas owned_by Payments"})];
        let (text, sources) = render_graph(&entities, &relations, 1024);
        assert!(text.len() <= 1024);
        assert_eq!(sources.len(), 2);
        let (small, empty) = render_graph(&entities, &relations, 0);
        assert!(small.is_empty());
        assert!(empty.is_empty());
    }
}
