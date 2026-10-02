pub mod loader;
pub mod manager;
pub mod process;
pub mod tmux;
pub mod worktree;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub project_root: String, // resolved git root (worktree-aware), falls back to cwd
    pub summary: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub dir_path: PathBuf,
    pub edited_files: Vec<String>,
    pub last_user_message: Option<String>,
    pub turn_count: usize,
    pub tool_call_count: usize,
    /// Bytes of this session's `events.jsonl` already summarized, so reselecting it
    /// only has to read whatever was appended since.
    pub details_parsed_len: u64,
}

/// Raw workspace.yaml structure
#[derive(Debug, Deserialize)]
pub struct WorkspaceYaml {
    pub id: String,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub summary: Option<String>,
    #[allow(dead_code)]
    pub summary_count: Option<u32>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl Session {
    pub fn display_name(&self) -> &str {
        self.summary
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("(unnamed)")
    }

    pub fn project_name(&self) -> &str {
        std::path::Path::new(&self.project_root)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.project_root)
    }

    pub fn relative_time(&self) -> String {
        let dt = self.updated_at.or(self.created_at);
        match dt {
            Some(dt) => {
                let now = Utc::now();
                let diff = now.signed_duration_since(dt);
                if diff.num_minutes() < 1 {
                    "just now".to_string()
                } else if diff.num_minutes() < 60 {
                    format!("{}m ago", diff.num_minutes())
                } else if diff.num_hours() < 24 {
                    format!("{}h ago", diff.num_hours())
                } else if diff.num_days() < 30 {
                    format!("{}d ago", diff.num_days())
                } else {
                    format!("{}mo ago", diff.num_days() / 30)
                }
            }
            None => "unknown".to_string(),
        }
    }
}
