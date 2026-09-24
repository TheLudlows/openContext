//! M0 probe only: JSON-lines commands, isolated from application data.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};

#[cfg(feature = "graph")]
mod graph;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "vector")]
mod vector;

fn emit(value: Value) -> Result<()> {
    println!("{value}");
    io::stdout().flush()?;
    Ok(())
}

fn field<'a>(request: &'a Value, name: &str) -> Result<&'a str> {
    request[name]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing string field {name}"))
}

struct Requests {
    first: Option<Value>,
    serving: bool,
}
impl Requests {
    fn new(request: Value) -> Result<Self> {
        let serving = request["op"] == "serve";
        if serving {
            emit(json!({"ready": true}))?;
        }
        Ok(Self {
            first: if serving { None } else { Some(request) },
            serving,
        })
    }
    fn next(&mut self) -> Result<Option<Value>> {
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        if !self.serving {
            return Ok(None);
        }
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&line)?))
    }
}

fn hold_until_killed() -> Result<()> {
    emit(json!({"ready": true, "uncommitted": true}))?;
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    anyhow::ensure!(args.len() == 4, "usage: probe BACKEND PATH REQUEST_JSON");
    let path = Path::new(&args[2]);
    let request: Value = serde_json::from_str(&args[3])?;
    match args[1].as_str() {
        #[cfg(feature = "sqlite")]
        "sqlite" => sqlite::run(path, request).await,
        #[cfg(feature = "vector")]
        "lancedb" => vector::run(path, request).await,
        #[cfg(feature = "graph")]
        "kuzu" => graph::run(path, request),
        backend => bail!("backend not compiled: {backend}"),
    }
}
