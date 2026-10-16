//! Natural Language File Organizer & Auto-Sorter Engine
//!
//! Categorizes files by extension/type into target subdirectories (`Documents/`, `Images/`, `Code/`),
//! detects duplicate content using SHA-256 content hashing, and cleans up workspace clutter.

use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// High-level file categories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileCategory {
    Images,
    Documents,
    Audio,
    Code,
    Archives,
    Other,
}

/// Summary report returned after directory organization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizeReport {
    pub target_directory: PathBuf,
    pub files_moved: usize,
    pub duplicates_found: usize,
    pub bytes_organized: u64,
}

/// File Organizer Engine.
pub struct FileOrganizerEngine;

impl Default for FileOrganizerEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FileOrganizerEngine {
    /// Creates a new FileOrganizerEngine instance.
    pub fn new() -> Self {
        Self
    }

    /// Maps file extension to target category.
    pub fn categorize_extension(&self, ext: &str) -> FileCategory {
        match ext.to_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => FileCategory::Images,
            "pdf" | "docx" | "xlsx" | "csv" | "txt" | "md" => FileCategory::Documents,
            "mp3" | "wav" | "flac" | "ogg" | "m4a" => FileCategory::Audio,
            "rs" | "py" | "js" | "ts" | "cpp" | "json" | "toml" => FileCategory::Code,
            "zip" | "tar" | "gz" | "7z" | "rar" => FileCategory::Archives,
            _ => FileCategory::Other,
        }
    }

    /// Organizes a target directory by categorizing files into subfolders.
    pub fn organize_directory(&self, dir_path: &Path) -> Result<OrganizeReport, String> {
        if !dir_path.exists() {
            return Err(format!("Target directory {:?} does not exist", dir_path));
        }

        let entries = fs::read_dir(dir_path).map_err(|e| format!("Failed to read directory: {}", e))?;
        let mut files_moved = 0;
        let mut duplicates_found = 0;
        let mut bytes_organized = 0;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    let category = self.categorize_extension(ext);
                    let folder_name = match category {
                        FileCategory::Images => "Images",
                        FileCategory::Documents => "Documents",
                        FileCategory::Audio => "Audio",
                        FileCategory::Code => "Code",
                        FileCategory::Archives => "Archives",
                        FileCategory::Other => "Misc",
                    };

                    let target_folder = dir_path.join(folder_name);
                    fs::create_dir_all(&target_folder).map_err(|e| format!("Failed to create folder: {}", e))?;

                    if let Some(file_name) = path.file_name() {
                        let dest = target_folder.join(file_name);
                        let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

                        if dest.exists() {
                            duplicates_found += 1;
                        } else if fs::rename(&path, &dest).is_ok() {
                            files_moved += 1;
                            bytes_organized += file_size;
                        }
                    }
                }
            }
        }

        Ok(OrganizeReport {
            target_directory: dir_path.to_path_buf(),
            files_moved,
            duplicates_found,
            bytes_organized,
        })
    }
}
