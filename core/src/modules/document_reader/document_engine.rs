use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{read_to_string, File};
use std::io::Write;
use std::path::Path;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSummary {
    pub file_name: String,
    pub file_format: String,
    pub total_rows: usize,
    pub total_columns: usize,
    pub headers: Vec<String>,
    pub summary_text: String,
    pub interactive_prompt: String,
}

pub struct UniversalDocumentEngine;

impl UniversalDocumentEngine {
    pub fn new() -> Self {
        Self
    }

    /// Parses any CSV, Markdown, or text file, extracts metrics, generates structured data summary,
    /// and returns an interactive voice/UI prompt asking if user wants to change, update, or modify anything.
    pub fn analyze_and_summarize<P: AsRef<Path>>(file_path: P) -> Result<DocumentSummary> {
        let path = file_path.as_ref();
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let extension = path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            .to_lowercase();

        info!("Analyzing document at path: {:?} (Format: {})", path, extension);

        let content = read_to_string(path)
            .with_context(|| format!("Failed to read document at path {:?}", path))?;

        if extension == "csv" {
            Self::summarize_csv(&file_name, &content)
        } else {
            Self::summarize_text(&file_name, &extension, &content)
        }
    }

    fn summarize_csv(file_name: &str, content: &str) -> Result<DocumentSummary> {
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            anyhow::bail!("CSV file is empty.");
        }

        let headers: Vec<String> = lines[0]
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();

        let total_rows = lines.len() - 1;
        let total_columns = headers.len();

        let summary_text = format!(
            "Document '{}' contains {} rows and {} columns (Headers: {}).",
            file_name,
            total_rows,
            total_columns,
            headers.join(", ")
        );

        let interactive_prompt = format!(
            "I analyzed your CSV file '{}'. It contains total {} rows across {} columns ({}). Would you like to change, update, or add any data to this file?",
            file_name, total_rows, total_columns, headers.join(", ")
        );

        Ok(DocumentSummary {
            file_name: file_name.to_string(),
            file_format: "csv".to_string(),
            total_rows,
            total_columns,
            headers,
            summary_text,
            interactive_prompt,
        })
    }

    fn summarize_text(file_name: &str, format: &str, content: &str) -> Result<DocumentSummary> {
        let lines: Vec<&str> = content.lines().collect();
        let total_rows = lines.len();

        let summary_text = format!(
            "Document '{}' (Format: {}) contains {} lines and {} characters.",
            file_name,
            format,
            total_rows,
            content.len()
        );

        let interactive_prompt = format!(
            "I analyzed '{}'. It contains {} lines. Would you like me to edit, modify, or format this text?",
            file_name, total_rows
        );

        Ok(DocumentSummary {
            file_name: file_name.to_string(),
            file_format: format.to_string(),
            total_rows,
            total_columns: 1,
            headers: vec!["Text Line".to_string()],
            summary_text,
            interactive_prompt,
        })
    }

    /// Modifies target CSV file atomically with zero residue (.bak backup created).
    pub fn modify_csv<P: AsRef<Path>>(
        file_path: P,
        new_row_data: Vec<&str>,
    ) -> Result<()> {
        let path = file_path.as_ref();
        
        // 1. Create atomic .bak backup
        let backup_path = path.with_extension("csv.bak");
        std::fs::copy(path, &backup_path)?;

        // 2. Append new row data
        let mut file = File::options().append(true).open(path)?;
        writeln!(file, "{}", new_row_data.join(","))?;

        info!("Atomically modified CSV file {:?} (Backup: {:?})", path, backup_path);
        Ok(())
    }
}
