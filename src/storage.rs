//! Task file persistence and the import / export format.
//!
//! Only task definitions and their on/off intent are stored; downloaded data,
//! counters and speed samples never touch the disk.

use crate::config::{Config, MAX_TASKS, Schedule, default_task_name, parse_config, parse_name, parse_schedule};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const FILE_VERSION: u64 = 1;

pub struct StoredTask {
    pub id: String,
    pub name: String,
    pub config: Config,
    pub created_at: f64,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct Record<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub enabled: bool,
    pub created_at: f64,
    pub config: &'a Config,
}

#[derive(Serialize)]
struct TaskFile<'a> {
    version: u64,
    next_id: u64,
    schedule: &'a Schedule,
    tasks: Vec<Record<'a>>,
}

#[derive(Default)]
pub struct Loaded {
    pub tasks: Vec<StoredTask>,
    pub next_id: u64,
    pub schedule: Schedule,
}

/// A missing schedule (older files) is the default: off.
fn schedule_of(value: &Value) -> Result<Schedule, String> {
    match value.get("schedule") {
        None | Some(Value::Null) => Ok(Schedule::default()),
        Some(schedule) => parse_schedule(schedule),
    }
}

pub fn id_number(id: &str) -> Option<u64> {
    let number: u64 = id.strip_prefix("task-")?.parse().ok()?;
    (number > 0 && id == format!("task-{number}")).then_some(number)
}

/// Read the task file. A missing file is an empty list; a damaged file is an
/// error so that it is never overwritten.
pub fn load(path: &Path) -> Result<Loaded, String> {
    let body = match fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Loaded::default()),
        Err(error) => return Err(format!("读取任务文件 {} 失败：{error}", path.display())),
    };
    let damaged = |reason: String| format!("任务文件 {} 无效（{reason}），原文件未修改", path.display());
    let value: Value = serde_json::from_slice(&body).map_err(|e| damaged(e.to_string()))?;
    if value.get("version").and_then(Value::as_u64) != Some(FILE_VERSION) {
        return Err(damaged("版本不受支持".into()));
    }
    let items = value.get("tasks").and_then(Value::as_array).ok_or_else(|| damaged("缺少任务列表".into()))?;
    if items.len() > MAX_TASKS {
        return Err(damaged(format!("超过 {MAX_TASKS} 条任务")));
    }
    let schedule = schedule_of(&value).map_err(|e| damaged(format!("全局定时：{e}")))?;
    let mut next_id = value.get("next_id").and_then(Value::as_u64).unwrap_or(0);
    let mut tasks: Vec<StoredTask> = Vec::with_capacity(items.len());
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        let number = id_number(id).ok_or_else(|| damaged(format!("任务编号 {id:?} 无效")))?;
        if tasks.iter().any(|task| task.id == id) {
            return Err(damaged(format!("任务编号 {id} 重复")));
        }
        next_id = next_id.max(number);
        let config =
            parse_config(item.get("config").unwrap_or(&Value::Null)).map_err(|e| damaged(format!("{id}：{e}")))?;
        let name = parse_name(item.get("name")).map_err(|e| damaged(format!("{id}：{e}")))?;
        tasks.push(StoredTask {
            id: id.to_owned(),
            name: if name.is_empty() { default_task_name(&config.url) } else { name },
            created_at: item.get("created_at").and_then(Value::as_f64).unwrap_or(0.0),
            enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            config,
        });
    }
    Ok(Loaded { tasks, next_id, schedule })
}

pub fn save(path: &Path, tasks: Vec<Record<'_>>, next_id: u64, schedule: &Schedule) -> io::Result<()> {
    let file = TaskFile { version: FILE_VERSION, next_id, schedule, tasks };
    let mut body = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    body.push(b'\n');
    write_atomic(path, &body)
}

/// Portable export: the global schedule plus task names and download
/// settings; no IDs or run state.
pub fn export(tasks: &[Record<'_>], schedule: &Schedule, exported_at: f64) -> Vec<u8> {
    let tasks: Vec<Value> = tasks.iter().map(|task| json!({"name": task.name, "config": task.config})).collect();
    let mut body = serde_json::to_vec_pretty(&json!({
        "app": "flow-panel",
        "version": FILE_VERSION,
        "exported_at": exported_at,
        "schedule": schedule,
        "tasks": tasks,
    }))
    .expect("JSON values always serialize");
    body.push(b'\n');
    body
}

#[derive(Debug)]
pub struct Import {
    pub tasks: Vec<(String, Config)>,
    /// Present when the file carries a global schedule.
    pub schedule: Option<Schedule>,
}

/// Parse an export (or a task file) into (name, config) pairs.
pub fn parse_import(body: &[u8]) -> Result<Import, String> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "导入文件不是有效的 JSON")?;
    let items = value.get("tasks").and_then(Value::as_array).ok_or("导入文件缺少任务列表 tasks")?;
    if items.is_empty() {
        return Err("导入文件中没有任务".into());
    }
    if items.len() > MAX_TASKS {
        return Err(format!("导入文件超过 {MAX_TASKS} 条任务"));
    }
    let schedule = match value.get("schedule") {
        None | Some(Value::Null) => None,
        Some(_) => Some(schedule_of(&value).map_err(|e| format!("全局定时：{e}"))?),
    };
    let tasks = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let config = parse_config(item.get("config").unwrap_or(&Value::Null))
                .map_err(|e| format!("第 {} 条任务：{e}", index + 1))?;
            let name = parse_name(item.get("name")).map_err(|e| format!("第 {} 条任务：{e}", index + 1))?;
            let name = if name.is_empty() { default_task_name(&config.url) } else { name };
            Ok((name, config))
        })
        .collect::<Result<_, String>>()?;
    Ok(Import { tasks, schedule })
}

/// Write to a temporary file in the same directory, flush it, then rename over
/// the target so a crash never leaves a half-written task file.
fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let temp = directory.join(format!(".tasks-{}-{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&temp)?;
        file.write_all(body)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    #[cfg(unix)]
    if result.is_ok() {
        if let Ok(dir) = fs::File::open(directory) {
            let _ = dir.sync_all();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    pub fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flow-panel-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Config {
        Config {
            url: "http://a.test/file".into(),
            settings: Settings { server_ips: vec!["10.0.0.1".into()], ..Settings::default() },
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("tasks.json");
        let config = sample();
        let records = vec![Record { id: "task-3", name: "a", enabled: true, created_at: 1.5, config: &config }];
        let schedule = Schedule { enabled: true, hours: vec![1, 2] };
        save(&path, records, 7, &schedule).unwrap();
        let loaded = load(&path).unwrap();
        let task = &loaded.tasks[0];
        assert_eq!((loaded.tasks.len(), loaded.next_id, task.enabled, task.config.clone()), (1, 7, true, config));
        assert_eq!(loaded.schedule, schedule);
        let leftovers: Vec<_> =
            fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name() != "tasks.json").collect();
        assert!(leftovers.is_empty(), "temporary files left behind");
        assert!(load(&dir.join("missing.json")).unwrap().tasks.is_empty());
        fs::write(&path, r#"{"version":1,"tasks":[]}"#).unwrap();
        assert_eq!(load(&path).unwrap().schedule, Schedule::default());
    }

    #[test]
    fn rejects_damaged_files_without_touching_them() {
        let dir = temp_dir("damaged");
        let path = dir.join("tasks.json");
        let good = r#"{"url":"http://a.test/"}"#;
        for body in [
            "not json".to_owned(),
            r#"{"version":9,"tasks":[]}"#.to_owned(),
            format!(r#"{{"version":1,"tasks":[{{"id":"task-0","config":{good}}}]}}"#),
            format!(r#"{{"version":1,"tasks":[{{"id":"task-1","config":{good}}},{{"id":"task-1","config":{good}}}]}}"#),
            r#"{"version":1,"tasks":[{"id":"task-1","config":{"url":"ftp://x"}}]}"#.to_owned(),
            r#"{"version":1,"schedule":{"enabled":true,"hours":[]},"tasks":[]}"#.to_owned(),
        ] {
            fs::write(&path, &body).unwrap();
            assert!(load(&path).is_err(), "accepted {body}");
            assert_eq!(fs::read_to_string(&path).unwrap(), body);
        }
    }

    #[test]
    fn export_import_round_trip() {
        let config = sample();
        let schedule = Schedule { enabled: true, hours: vec![3] };
        let record = Record { id: "task-1", name: "名字", enabled: true, created_at: 0.0, config: &config };
        let imported = parse_import(&export(&[record], &schedule, 1.0)).unwrap();
        assert_eq!(imported.tasks, vec![("名字".to_owned(), config)]);
        assert_eq!(imported.schedule, Some(schedule));
        assert_eq!(parse_import(br#"{"tasks":[{"config":{"url":"http://a/"}}]}"#).unwrap().schedule, None);
        assert!(parse_import(b"{}").is_err());
        assert!(parse_import(br#"{"tasks":[]}"#).is_err());
        let err = parse_import(br#"{"tasks":[{"config":{"url":"http://a/"}},{"config":{"url":"bad"}}]}"#).unwrap_err();
        assert!(err.starts_with("第 2 条任务"), "{err}");
    }
}
