//! Reinforcement Learning Agent & Experience Replay Engine
//!
//! Captures feedback rewards (+1.0 for success, -1.0 for error),
//! updates policy confidence online ($C \leftarrow C + 0.05 \cdot R$), and keeps tools that fail at a low score (they are not purged).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Trajectory transition tuple.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub state_intent: String,
    pub action_tool: String,
    pub reward: f32,
    pub timestamp: String,
}

/// Learned policy weight record per tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyWeight {
    pub tool_name: String,
    pub confidence_score: f32,
    pub reuse_count: u64,
}

/// Reinforcement Learning Engine.
pub struct RlEngine {
    trajectories_file: PathBuf,
    weights_file: PathBuf,
}

impl RlEngine {
    /// Creates a new RlEngine instance.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let exp_dir = base_dir.as_ref().join("learning").join("experience");
        let _ = fs::create_dir_all(&exp_dir);

        Self {
            trajectories_file: exp_dir.join("trajectories.json"),
            weights_file: exp_dir.join("policy_weights.json"),
        }
    }

    /// Records a feedback transition and applies a single online weight update.
    pub fn record_transition(&self, intent: &str, tool: &str, reward: f32) -> Result<(), String> {
        let reward = reward.clamp(-1.0, 1.0);
        let step = TrajectoryStep {
            // What is kept is the goal with addresses, numbers and secrets removed, as in the recipe store.
            state_intent: crate::memory::experience::sanitize::redact(intent).0,
            action_tool: tool.to_string(),
            reward,
            timestamp: utc_now(),
        };

        let mut steps = self.load_trajectories();
        // Entries written before goals were redacted are cleaned the next time the file is written.
        for old in &mut steps {
            old.state_intent = crate::memory::experience::sanitize::redact(&old.state_intent).0;
        }
        steps.push(step);
        let json = serde_json::to_string_pretty(&steps)
            .map_err(|e| format!("Failed to serialize trajectories: {e}"))?;
        write_atomic(&self.trajectories_file, &json)
            .map_err(|e| format!("Failed to write trajectories file: {e}"))?;

        // Online update for this step only — never replay the full history here.
        self.apply_reward(tool, reward)?;
        Ok(())
    }

    /// Rebuilds policy weights from all stored trajectories (offline / correction).
    pub fn update_policy_weights(&self) -> Result<usize, String> {
        let trajectories = self.load_trajectories();
        let mut weights: Vec<PolicyWeight> = Vec::new();

        for step in &trajectories {
            apply_reward_in_place(&mut weights, &step.action_tool, step.reward);
        }

        // A tool that keeps failing stays in the table with its low score. (Dropping it made `get_tool_confidence`
        // fall back to the neutral default 0.80, so the worst tool came back as one of the most trusted.)

        let json = serde_json::to_string_pretty(&weights)
            .map_err(|e| format!("Failed to serialize policy weights: {e}"))?;
        write_atomic(&self.weights_file, &json)
            .map_err(|e| format!("Failed to write weights file: {e}"))?;

        tracing::info!("Rebuilt RL policy weights (Active Tools: {})", weights.len());
        Ok(weights.len())
    }

    /// Returns learned confidence score for a candidate tool.
    pub fn get_tool_confidence(&self, tool: &str) -> f32 {
        let weights = self.load_weights();
        weights
            .iter()
            .find(|w| w.tool_name == tool)
            .map(|w| w.confidence_score)
            .unwrap_or(0.80)
    }

    /// Tools sorted by descending learned confidence (unknown tools keep default 0.80).
    pub fn rank_tools<'a>(&self, tools: &[&'a str]) -> Vec<(&'a str, f32)> {
        let mut ranked: Vec<_> = tools
            .iter()
            .map(|t| (*t, self.get_tool_confidence(t)))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranked
    }

    /// Forget everything learned about the named tools (case-insensitive): their trajectory steps
    /// and their policy weights. Used for tools Aqua no longer exposes, so an old habit (say, "use
    /// PowerShell for the weather") cannot steer the planner towards a tool that is not there.
    /// Returns how many records were removed.
    pub fn forget_tools(&self, names: &[&str]) -> usize {
        let matches = |t: &str| names.iter().any(|n| n.eq_ignore_ascii_case(t));
        let mut removed = 0;
        let steps = self.load_trajectories();
        let kept: Vec<TrajectoryStep> = steps.iter().filter(|s| !matches(&s.action_tool)).cloned().collect();
        if kept.len() != steps.len() {
            removed += steps.len() - kept.len();
            if let Ok(json) = serde_json::to_string_pretty(&kept) {
                let _ = write_atomic(&self.trajectories_file, &json);
            }
        }
        let weights = self.load_weights();
        let kept_w: Vec<PolicyWeight> = weights.iter().filter(|w| !matches(&w.tool_name)).cloned().collect();
        if kept_w.len() != weights.len() {
            removed += weights.len() - kept_w.len();
            if let Ok(json) = serde_json::to_string_pretty(&kept_w) {
                let _ = write_atomic(&self.weights_file, &json);
            }
        }
        removed
    }

    fn apply_reward(&self, tool: &str, reward: f32) -> Result<(), String> {
        let mut weights = self.load_weights();
        apply_reward_in_place(&mut weights, tool, reward);
        // A tool that keeps failing stays in the table with its low score. (Dropping it made `get_tool_confidence`
        // fall back to the neutral default 0.80, so the worst tool came back as one of the most trusted.)

        let json = serde_json::to_string_pretty(&weights)
            .map_err(|e| format!("Failed to serialize policy weights: {e}"))?;
        write_atomic(&self.weights_file, &json)
            .map_err(|e| format!("Failed to write weights file: {e}"))?;
        Ok(())
    }

    fn load_trajectories(&self) -> Vec<TrajectoryStep> {
        if !self.trajectories_file.exists() {
            return vec![];
        }
        let content = fs::read_to_string(&self.trajectories_file).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_default()
    }

    fn load_weights(&self) -> Vec<PolicyWeight> {
        if !self.weights_file.exists() {
            return vec![];
        }
        let content = fs::read_to_string(&self.weights_file).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_default()
    }
}

fn apply_reward_in_place(weights: &mut Vec<PolicyWeight>, tool: &str, reward: f32) {
    if let Some(w) = weights.iter_mut().find(|w| w.tool_name == tool) {
        w.confidence_score = (w.confidence_score + 0.05 * reward).clamp(0.0, 1.0);
        w.reuse_count = w.reuse_count.saturating_add(1);
    } else {
        weights.push(PolicyWeight {
            tool_name: tool.to_string(),
            confidence_score: (0.80 + 0.05 * reward).clamp(0.0, 1.0),
            reuse_count: 1,
        });
    }
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Write a file so that a crash halfway leaves the old contents, not a cut-off file (which the loader would read as an
/// empty history, and the next write would then erase what was there).
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn goals_are_stored_without_email_addresses_and_old_entries_are_cleaned() {
        let root = std::env::temp_dir().join(format!("aqua_rl_red_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let rl = RlEngine::new(&root);
        let old = vec![TrajectoryStep { state_intent: "send a mail to someone@example.com".into(), action_tool: "t".into(), reward: 1.0, timestamp: utc_now() }];
        fs::write(root.join("learning/experience/trajectories.json"), serde_json::to_string(&old).unwrap()).unwrap();
        rl.record_transition("mail bob@example.org the report", "browser_navigate", 1.0).unwrap();
        let kept = fs::read_to_string(root.join("learning/experience/trajectories.json")).unwrap();
        assert!(!kept.contains("@example"), "{kept}");
        assert!(kept.contains("browser_navigate"));
        let _ = fs::remove_dir_all(&root);
    }
}
