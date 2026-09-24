use crate::*;
use arrow_array::{
    Array, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchIterator, StringArray,
    types::Float32Type,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use std::sync::Arc;

pub async fn run(path: &Path, request: Value) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("workspace", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, false),
        Field::new("value", DataType::Int64, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 2),
            true,
        ),
    ]));
    let db = lancedb::connect(
        path.to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 path"))?,
    )
    .read_consistency_interval(Duration::ZERO)
    .execute()
    .await?;
    if request["op"] == "init" {
        if !db
            .table_names()
            .execute()
            .await?
            .iter()
            .any(|s| s == "records")
        {
            db.create_empty_table("records", schema).execute().await?;
        }
        return emit(json!({"ok": true}));
    }
    let table = db.open_table("records").execute().await?;
    let mut requests = Requests::new(request)?;
    while let Some(r) = requests.next()? {
        let tenant = field(&r, "tenant")?;
        let workspace = field(&r, "workspace")?;
        let id = field(&r, "id")?;
        let quote = |s: &str| s.replace('\'', "''");
        let scope = format!(
            "tenant='{}' AND workspace='{}'",
            quote(tenant),
            quote(workspace)
        );
        let filter = format!("{scope} AND id='{}'", quote(id));
        match field(&r, "op")? {
            "put" => {
                let value = r["value"].as_i64().unwrap_or(0);
                let batch = RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(StringArray::from(vec![tenant])),
                        Arc::new(StringArray::from(vec![workspace])),
                        Arc::new(StringArray::from(vec![id])),
                        Arc::new(Int64Array::from(vec![value])),
                        Arc::new(
                            FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                                vec![Some(vec![Some(value as f32), Some(0.0)])],
                                2,
                            ),
                        ),
                    ],
                )?;
                table
                    .add(Box::new(RecordBatchIterator::new(
                        vec![Ok(batch)],
                        schema.clone(),
                    )))
                    .execute()
                    .await?;
                emit(json!({"ok": true}))?;
            }
            "get" | "search" => {
                let batches: Vec<RecordBatch> = if r["op"] == "search" {
                    table
                        .query()
                        .nearest_to(&[0.0_f32, 0.0])?
                        .only_if(scope)
                        .limit(1)
                        .execute()
                        .await?
                        .try_collect()
                        .await?
                } else {
                    table
                        .query()
                        .only_if(filter)
                        .execute()
                        .await?
                        .try_collect()
                        .await?
                };
                let mut values = Vec::new();
                for batch in batches {
                    let column = batch
                        .column_by_name("value")
                        .ok_or_else(|| anyhow::anyhow!("value missing"))?
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .ok_or_else(|| anyhow::anyhow!("value type"))?;
                    for i in 0..column.len() {
                        values.push(column.value(i));
                    }
                }
                values.sort();
                emit(json!({"values": values}))?;
            }
            "delete" => {
                table.delete(&filter).await?;
                emit(json!({"ok": true}))?;
            }
            op => bail!("unsupported lancedb operation {op}"),
        }
    }
    Ok(())
}
