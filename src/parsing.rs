use crate::{
    error::{AppError, Result},
    types::Chunk,
};
use serde_json::json;
use std::{path::Path, sync::LazyLock, time::Duration};
static JIEBA: LazyLock<jieba_rs::Jieba> = LazyLock::new(jieba_rs::Jieba::new);
pub const MAX_TEXT: usize = 1_000_000;
pub const MAX_FILE: usize = 10 * 1024 * 1024;

pub fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > MAX_TEXT || text.contains('\0') {
        return Err(AppError::Invalid(
            "text must be nonempty UTF-8, without NUL, and at most 1 MB".into(),
        ));
    }
    Ok(())
}

pub fn lexical(text: &str) -> String {
    JIEBA
        .cut_for_search(text, false)
        .into_iter()
        .filter(|s| s.chars().any(char::is_alphanumeric))
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn chunks(text: &str, format: &str) -> Result<Vec<Chunk>> {
    validate_text(text)?;
    if !["text", "markdown"].contains(&format) {
        return Err(AppError::Invalid("unsupported text format".into()));
    }
    let mut out = Vec::new();
    if format == "markdown" {
        // Preserve original Markdown ranges, including code and headings, rather than losing evidence offsets.
        let mut start = 0;
        let mut end = 0;
        for (_, range) in pulldown_cmark::Parser::new(text).into_offset_iter() {
            if range.end.saturating_sub(start) > 2400 && end > start {
                split_range(text, start, end, &mut out);
                start = end;
            }
            end = end.max(range.end);
        }
        if start < text.len() {
            split_range(text, start, text.len(), &mut out);
        }
    } else {
        split_range(text, 0, text.len(), &mut out);
    }
    if out.is_empty() {
        return Err(AppError::Invalid("no indexable text".into()));
    }
    Ok(out)
}

fn split_range(text: &str, start: usize, end: usize, out: &mut Vec<Chunk>) {
    let mut pos = start;
    while pos < end {
        let mut next = (pos + 2400).min(end);
        while !text.is_char_boundary(next) {
            next -= 1;
        }
        let value = &text[pos..next];
        out.push(Chunk { content: value.to_owned(), locator: json!({"byte_start":pos,"byte_end":next,"line_start":text[..pos].bytes().filter(|b| *b==b'\n').count()+1,"line_end":text[..next].bytes().filter(|b| *b==b'\n').count()+1,"parser":"source-ranges-v1"}) });
        pos = next;
    }
}

pub fn pdf_child(path: &Path) -> anyhow::Result<Vec<Chunk>> {
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= MAX_FILE as u64,
        "file too large"
    );
    let doc = lopdf::Document::load(path)?;
    let count = doc.get_pages().len();
    anyhow::ensure!(
        (1..=200).contains(&count),
        "PDF page limit exceeded or empty PDF"
    );
    drop(doc);
    let pages = pdf_extract::extract_text_by_pages(path)?;
    anyhow::ensure!(
        pages.len() == count,
        "PDF extraction failed before final page"
    );
    let mut result = Vec::new();
    let mut total = 0;
    for (page, text) in pages.iter().enumerate() {
        anyhow::ensure!(
            text.chars().filter(|c| c.is_alphanumeric()).count() >= 3,
            "PDF page has no usable text; OCR is unsupported"
        );
        total += text.len();
        anyhow::ensure!(total <= MAX_TEXT, "PDF extracted text limit exceeded");
        for mut chunk in chunks(text, "text")? {
            chunk.locator["page"] = json!(page + 1);
            chunk.locator["parser"] = json!("pdf-extract-0.10-v1");
            result.push(chunk);
        }
    }
    Ok(result)
}

pub async fn parse_file(path: &Path, format: &str) -> Result<Vec<Chunk>> {
    if format != "pdf" {
        let bytes = tokio::fs::read(path).await.map_err(anyhow::Error::from)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| AppError::Invalid("file must use UTF-8".into()))?;
        return chunks(&text, format);
    }
    let exe = std::env::current_exe().map_err(anyhow::Error::from)?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("parse-pdf").arg(path).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| AppError::Invalid("PDF parser timed out".into()))?
        .map_err(anyhow::Error::from)?;
    if !output.status.success() {
        return Err(AppError::Invalid(
            "PDF unsupported or failed quality checks".into(),
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| AppError::Internal(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_ranges_and_chinese_terms() {
        let text = "# 决策\n".to_owned() + &"生产发布需要审批。\n".repeat(500);
        let parts = chunks(&text, "markdown").unwrap();
        assert!(parts.len() > 2);
        assert_eq!(
            parts.iter().map(|c| c.content.as_str()).collect::<String>(),
            text
        );
        for c in parts {
            let a = c.locator["byte_start"].as_u64().unwrap() as usize;
            let b = c.locator["byte_end"].as_u64().unwrap() as usize;
            assert_eq!(c.content, text[a..b]);
        }
        assert!(lexical("生产发布需要审批").contains("审批"));
    }
    #[test]
    fn rejects_empty_and_nul() {
        assert!(chunks("  ", "text").is_err());
        assert!(chunks("a\0b", "text").is_err());
    }
}
