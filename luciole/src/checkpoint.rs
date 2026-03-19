use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// CheckpointStore — persist DAG execution progress
// ---------------------------------------------------------------------------

/// Persist DAG execution progress for crash recovery.
///
/// After each node completes, the runtime saves it to the store.
/// On restart, the caller reads the checkpoint to decide which nodes
/// to skip (by building a smaller DAG with only remaining work).
pub trait CheckpointStore: Send + Sync {
    /// Record that a node completed successfully.
    fn save_node_completed(&self, dag_id: &str, node_name: &str, node_type: &str);

    /// Record that a node failed.
    fn save_node_failed(&self, dag_id: &str, node_name: &str, error: &str);

    /// Mark the entire DAG as completed.
    fn mark_completed(&self, dag_id: &str);

    /// Mark the entire DAG as failed.
    fn mark_failed(&self, dag_id: &str, error: &str);

    /// Load checkpoint for a DAG. Returns None if no checkpoint exists.
    fn load(&self, dag_id: &str) -> Option<DagCheckpoint>;
}

/// Snapshot of a DAG execution's progress.
#[derive(Debug, Clone)]
pub struct DagCheckpoint {
    pub dag_id: String,
    pub completed_nodes: Vec<String>,
    pub failed_node: Option<String>,
    pub error: Option<String>,
    pub status: CheckpointStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointStatus {
    Running,
    Completed,
    Failed,
}

impl DagCheckpoint {
    /// Check if a node was already completed in this checkpoint.
    pub fn is_completed(&self, node_name: &str) -> bool {
        self.completed_nodes.iter().any(|n| n == node_name)
    }
}

// ---------------------------------------------------------------------------
// MemoryCheckpointStore — in-memory, for tests
// ---------------------------------------------------------------------------

pub struct MemoryCheckpointStore {
    data: Mutex<HashMap<String, CheckpointData>>,
}

struct CheckpointData {
    completed: Vec<(String, String)>, // (node_name, node_type)
    failed_node: Option<String>,
    error: Option<String>,
    status: CheckpointStatus,
}

impl MemoryCheckpointStore {
    pub fn new() -> Self {
        Self { data: Mutex::new(HashMap::new()) }
    }
}

impl CheckpointStore for MemoryCheckpointStore {
    fn save_node_completed(&self, dag_id: &str, node_name: &str, node_type: &str) {
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(dag_id.to_string()).or_insert_with(|| CheckpointData {
            completed: Vec::new(),
            failed_node: None,
            error: None,
            status: CheckpointStatus::Running,
        });
        entry.completed.push((node_name.to_string(), node_type.to_string()));
    }

    fn save_node_failed(&self, dag_id: &str, node_name: &str, error: &str) {
        let mut data = self.data.lock().unwrap();
        let entry = data.entry(dag_id.to_string()).or_insert_with(|| CheckpointData {
            completed: Vec::new(),
            failed_node: None,
            error: None,
            status: CheckpointStatus::Running,
        });
        entry.failed_node = Some(node_name.to_string());
        entry.error = Some(error.to_string());
        entry.status = CheckpointStatus::Failed;
    }

    fn mark_completed(&self, dag_id: &str) {
        let mut data = self.data.lock().unwrap();
        if let Some(entry) = data.get_mut(dag_id) {
            entry.status = CheckpointStatus::Completed;
        }
    }

    fn mark_failed(&self, dag_id: &str, error: &str) {
        let mut data = self.data.lock().unwrap();
        if let Some(entry) = data.get_mut(dag_id) {
            entry.status = CheckpointStatus::Failed;
            entry.error = Some(error.to_string());
        }
    }

    fn load(&self, dag_id: &str) -> Option<DagCheckpoint> {
        let data = self.data.lock().unwrap();
        let entry = data.get(dag_id)?;
        Some(DagCheckpoint {
            dag_id: dag_id.to_string(),
            completed_nodes: entry.completed.iter().map(|(n, _)| n.clone()).collect(),
            failed_node: entry.failed_node.clone(),
            error: entry.error.clone(),
            status: entry.status.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// FileCheckpointStore — filesystem-based, for production
// ---------------------------------------------------------------------------

/// Checkpoint store using a directory on the filesystem.
/// Each DAG execution gets a JSON file: `{dir}/{dag_id}.checkpoint.json`
pub struct FileCheckpointStore {
    dir: std::path::PathBuf,
}

impl FileCheckpointStore {
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        let dir = dir.into();
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    fn path(&self, dag_id: &str) -> std::path::PathBuf {
        self.dir.join(format!("{}.checkpoint", dag_id))
    }

    fn write_lines(&self, dag_id: &str, lines: &[String]) {
        let path = self.path(dag_id);
        let content = lines.join("\n");
        let _ = std::fs::write(&path, content);
    }

    fn read_lines(&self, dag_id: &str) -> Option<Vec<String>> {
        let path = self.path(dag_id);
        let content = std::fs::read_to_string(&path).ok()?;
        Some(content.lines().map(|l| l.to_string()).collect())
    }
}

impl CheckpointStore for FileCheckpointStore {
    fn save_node_completed(&self, dag_id: &str, node_name: &str, node_type: &str) {
        let mut lines = self.read_lines(dag_id).unwrap_or_default();
        lines.push(format!("COMPLETED:{}:{}", node_name, node_type));
        self.write_lines(dag_id, &lines);
    }

    fn save_node_failed(&self, dag_id: &str, node_name: &str, error: &str) {
        let mut lines = self.read_lines(dag_id).unwrap_or_default();
        lines.push(format!("FAILED:{}:{}", node_name, error));
        self.write_lines(dag_id, &lines);
    }

    fn mark_completed(&self, dag_id: &str) {
        let mut lines = self.read_lines(dag_id).unwrap_or_default();
        lines.push("STATUS:COMPLETED".to_string());
        self.write_lines(dag_id, &lines);
    }

    fn mark_failed(&self, dag_id: &str, error: &str) {
        let mut lines = self.read_lines(dag_id).unwrap_or_default();
        lines.push(format!("STATUS:FAILED:{}", error));
        self.write_lines(dag_id, &lines);
    }

    fn load(&self, dag_id: &str) -> Option<DagCheckpoint> {
        let lines = self.read_lines(dag_id)?;
        let mut completed = Vec::new();
        let mut failed_node = None;
        let mut error = None;
        let mut status = CheckpointStatus::Running;

        for line in &lines {
            if let Some(rest) = line.strip_prefix("COMPLETED:") {
                let name = rest.split(':').next().unwrap_or(rest);
                completed.push(name.to_string());
            } else if let Some(rest) = line.strip_prefix("FAILED:") {
                let mut parts = rest.splitn(2, ':');
                failed_node = parts.next().map(|s| s.to_string());
                error = parts.next().map(|s| s.to_string());
                status = CheckpointStatus::Failed;
            } else if line == "STATUS:COMPLETED" {
                status = CheckpointStatus::Completed;
            } else if let Some(rest) = line.strip_prefix("STATUS:FAILED:") {
                status = CheckpointStatus::Failed;
                error = Some(rest.to_string());
            }
        }

        Some(DagCheckpoint {
            dag_id: dag_id.to_string(),
            completed_nodes: completed,
            failed_node,
            error,
            status,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(store: &dyn CheckpointStore) {
        assert!(store.load("dag1").is_none());

        store.save_node_completed("dag1", "merge_0", "merge");
        store.save_node_completed("dag1", "merge_1", "merge");

        let cp = store.load("dag1").unwrap();
        assert_eq!(cp.completed_nodes, vec!["merge_0", "merge_1"]);
        assert_eq!(cp.status, CheckpointStatus::Running);
        assert!(cp.is_completed("merge_0"));
        assert!(!cp.is_completed("merge_2"));

        store.save_node_failed("dag1", "merge_2", "out of disk");
        let cp = store.load("dag1").unwrap();
        assert_eq!(cp.failed_node, Some("merge_2".to_string()));
        assert_eq!(cp.error, Some("out of disk".to_string()));
        assert_eq!(cp.status, CheckpointStatus::Failed);
    }

    #[test]
    fn memory_checkpoint_store() {
        let store = MemoryCheckpointStore::new();
        test_store(&store);
    }

    #[test]
    fn file_checkpoint_store() {
        let dir = std::env::temp_dir().join("luciole_test_checkpoint");
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileCheckpointStore::new(&dir);
        test_store(&store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_completed() {
        let store = MemoryCheckpointStore::new();
        store.save_node_completed("dag1", "a", "node");
        store.mark_completed("dag1");
        let cp = store.load("dag1").unwrap();
        assert_eq!(cp.status, CheckpointStatus::Completed);
    }
}
