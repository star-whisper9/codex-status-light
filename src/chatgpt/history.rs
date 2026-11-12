use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

// 先筛选少量运行候选，再用 (thread_id, rollout_ordinal) 索引排除旧轮次，避免全表分组。
const LATEST_IN_PROGRESS: &str = "
    SELECT t.thread_id FROM thread_turns t
    WHERE t.status = 'inProgress'
      AND NOT EXISTS (
          SELECT 1 FROM thread_turns newer
          WHERE newer.thread_id = t.thread_id
            AND newer.rollout_ordinal > t.rollout_ordinal
      )";

pub(super) struct Catalog {
    pub candidates: HashSet<String>,
    pub desktop_ids: HashSet<String>,
    pub parents: HashMap<String, String>,
    pub roots: HashMap<String, String>,
}

/// 数据库只用于发现会话；持久化的 inProgress 可能是崩溃遗留，不能作为运行证据。
pub(super) fn read(home: &Path) -> Result<Catalog> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let db = Connection::open_with_flags(home.join("state_5.sqlite"), flags)
        .context("无法只读打开 Codex 会话数据库")?;
    let mut query = db.prepare(
        "SELECT id, source, attached_thread_parent_id FROM threads ORDER BY updated_at DESC",
    )?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut records: HashMap<String, Option<String>> = HashMap::new();
    let mut ordered = Vec::new();
    for row in rows {
        let (id, source, attached_parent) = row?;
        let parent = if source == "vscode" {
            attached_parent
        } else if source.starts_with('{') {
            let source: serde_json::Value =
                serde_json::from_str(&source).context("会话 source JSON 无效")?;
            let Some(parent) = source["subagent"]["thread_spawn"]["parent_thread_id"].as_str()
            else {
                continue;
            };
            Some(parent.to_owned())
        } else {
            continue;
        };
        ordered.push(id.clone());
        records.insert(id, parent);
    }
    let mut desktop_ids = HashSet::new();
    let mut parents = HashMap::new();
    let mut roots = HashMap::new();
    for id in &ordered {
        let mut cursor = id;
        let mut visited = HashSet::new();
        while visited.insert(cursor) {
            match records.get(cursor) {
                Some(Some(parent)) => cursor = parent,
                Some(None) => {
                    desktop_ids.insert(id.clone());
                    roots.insert(id.clone(), cursor.clone());
                    if let Some(Some(parent)) = records.get(id) {
                        parents.insert(id.clone(), parent.clone());
                    }
                    break;
                }
                None => break,
            }
        }
    }
    let recent: Vec<_> = ordered
        .into_iter()
        .filter(|id| desktop_ids.contains(id))
        .take(32)
        .collect();
    let history = Connection::open_with_flags(home.join("thread_history_1.sqlite"), flags)
        .context("无法只读打开 Codex 轮次数据库")?;
    let mut query = history.prepare(LATEST_IN_PROGRESS)?;
    let mut candidates: HashSet<String> = recent.into_iter().collect();
    for id in query.query_map([], |row| row.get::<_, String>(0))? {
        let id = id?;
        if desktop_ids.contains(&id) {
            candidates.insert(id);
        }
    }
    for id in candidates.clone() {
        if let Some(root) = roots.get(&id) {
            candidates.insert(root.clone());
        }
    }
    Ok(Catalog {
        candidates,
        desktop_ids,
        parents,
        roots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    #[test]
    fn only_latest_in_progress_turn_is_selected() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE thread_turns(thread_id TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL, status TEXT NOT NULL);
             CREATE UNIQUE INDEX idx_thread_turns_page ON thread_turns(thread_id, rollout_ordinal);
             INSERT INTO thread_turns VALUES
                ('finished',0,'inProgress'),('finished',1,'completed'),
                ('active',0,'inProgress'),('active',1,'inProgress'),('active',2,'inProgress'),
                ('restarted',0,'completed'),('restarted',1,'inProgress');",
        ).unwrap();
        let mut query = db.prepare(LATEST_IN_PROGRESS).unwrap();
        let mut ids = query
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        ids.sort();
        assert_eq!(ids, ["active", "restarted"]);
    }

    #[test]
    fn discovery_excludes_cli_guardian_and_finished_old_turns() {
        let home = std::env::temp_dir().join(format!(
            "status-light-history-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&home).unwrap();
        let db = Connection::open(home.join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT, source TEXT, attached_thread_parent_id TEXT, updated_at INTEGER);
            INSERT INTO threads VALUES('cli','cli',NULL,100),('guardian','{\"subagent\":{}}',NULL,100),('attached','vscode','parent',100),('old','vscode',NULL,0),('active','vscode',NULL,0);").unwrap();
        for index in 0..32 {
            db.execute(
                "INSERT INTO threads VALUES(?1,'vscode',NULL,50)",
                [format!("recent-{index}")],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO threads VALUES('child',?1,NULL,100)",
            [r#"{"subagent":{"thread_spawn":{"parent_thread_id":"active"}}}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO threads VALUES('cli-child',?1,NULL,100)",
            [r#"{"subagent":{"thread_spawn":{"parent_thread_id":"cli"}}}"#],
        )
        .unwrap();
        drop(db);
        let db = Connection::open(home.join("thread_history_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE thread_turns(thread_id TEXT, rollout_ordinal INTEGER, status TEXT);
            INSERT INTO thread_turns VALUES('old',0,'inProgress'),('old',1,'completed'),('active',0,'inProgress'),('cli',0,'inProgress'),('guardian',0,'inProgress');").unwrap();
        drop(db);
        let catalog = read(&home).unwrap();
        assert!(!catalog.desktop_ids.contains("cli"));
        assert!(!catalog.desktop_ids.contains("guardian"));
        assert!(!catalog.desktop_ids.contains("attached"));
        assert!(!catalog.candidates.contains("old"));
        assert!(catalog.candidates.contains("active"));
        assert!(catalog.candidates.contains("child"));
        assert_eq!(
            catalog.parents.get("child").map(String::as_str),
            Some("active")
        );
        assert!(!catalog.desktop_ids.contains("cli-child"));
        std::fs::remove_dir_all(home).unwrap();
    }
}
