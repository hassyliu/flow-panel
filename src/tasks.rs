//! Task list: independent tasks, each with its own on/off intent, scheduled
//! first-in first-out under a global concurrency budget.

use crate::config::{Config, MAX_TASKS, Schedule, TaskRequest, batch_names, default_task_name, runs_at};
use crate::engine::{Engine, Phase, Run};
use crate::storage::{self, Record};
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::task::TaskTracker;

pub const MAX_TOTAL_WORKERS: u32 = 256;

#[derive(Debug, PartialEq)]
pub enum TaskError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Storage(String),
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::NotFound => f.write_str("任务不存在"),
            TaskError::Invalid(message) | TaskError::Conflict(message) => f.write_str(message),
            TaskError::Storage(message) => write!(f, "保存任务配置失败：{message}"),
        }
    }
}

#[derive(Clone)]
struct Task {
    id: String,
    name: String,
    config: Config,
    created_at: f64,
    /// User intent: should this task be downloading (or queued)?
    enabled: bool,
    /// The active run, cleared when it ends.
    run: Option<Arc<Run>>,
    /// The most recent run, kept to show its final state and last error.
    last_run: Option<Arc<Run>>,
    /// The current run was stopped because its hours ended; keep it enabled.
    paused: bool,
    error: String,
}

impl Task {
    fn new(id: String, name: String, config: Config, created_at: f64, enabled: bool) -> Task {
        Task { id, name, config, created_at, enabled, run: None, last_run: None, paused: false, error: String::new() }
    }

    fn busy(&self) -> bool {
        self.enabled || self.run.is_some()
    }

    fn in_hours(&self, hour: u8, global: &Schedule) -> bool {
        runs_at(&self.config.settings.hours, global, hour)
    }

    fn record(&self) -> Record<'_> {
        Record {
            id: &self.id,
            name: &self.name,
            enabled: self.enabled,
            created_at: self.created_at,
            config: &self.config,
        }
    }
}

#[derive(Serialize)]
pub struct TaskView {
    pub id: String,
    pub name: String,
    pub config: Config,
    pub created_at: f64,
    pub state: &'static str,
    pub speed: f64,
    pub active: u32,
    pub last_error: String,
    pub server_ip: String,
    pub connection_host: String,
    pub ip_overridden: bool,
    pub tls: String,
    pub busy: bool,
}

#[derive(Serialize)]
pub struct Board {
    pub app_version: &'static str,
    pub instance: String,
    pub version: u64,
    pub tasks: Vec<TaskView>,
    pub speed: f64,
    pub running: usize,
    pub queued: usize,
    /// Enabled tasks outside their hours.
    pub waiting: usize,
    pub active: u32,
    pub allocated: u32,
    pub max_workers: u32,
    pub max_tasks: usize,
    /// Panel clock used for schedules, e.g. "14:05", and its zone, e.g. "UTC+8".
    pub clock: String,
    pub utc_offset: String,
    /// Hours for tasks without their own.
    pub schedule: Schedule,
    /// An access password is set (filled in by the web server).
    pub auth: bool,
}

struct Inner {
    tasks: Vec<Task>,
    next_id: u64,
    schedule: Schedule,
    version: u64,
    closing: bool,
}

pub struct TaskManager {
    engine: Arc<Engine>,
    store: Option<PathBuf>,
    instance: String,
    inner: Mutex<Inner>,
    watchers: TaskTracker,
    /// Time zone of the schedule hours, in minutes east of UTC.
    utc_offset: AtomicI32,
    #[cfg(test)]
    fixed_hour: Mutex<Option<u8>>,
}

pub fn now() -> f64 {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    (seconds * 1000.0).round() / 1000.0
}

impl TaskManager {
    /// In-memory manager without persistence.
    #[cfg(test)]
    pub fn memory(engine: Arc<Engine>) -> Arc<TaskManager> {
        Self::build(engine, None, Vec::new(), 0, Schedule::default())
    }

    /// Load tasks from `path` (created on the first save).
    pub fn load(engine: Arc<Engine>, path: PathBuf) -> Result<Arc<TaskManager>, String> {
        let loaded = storage::load(&path)?;
        let tasks =
            loaded.tasks.into_iter().map(|t| Task::new(t.id, t.name, t.config, t.created_at, t.enabled)).collect();
        Ok(Self::build(engine, Some(path), tasks, loaded.next_id, loaded.schedule))
    }

    fn build(
        engine: Arc<Engine>,
        store: Option<PathBuf>,
        tasks: Vec<Task>,
        next_id: u64,
        schedule: Schedule,
    ) -> Arc<TaskManager> {
        let instance = format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos());
        Arc::new(TaskManager {
            engine,
            store,
            instance,
            inner: Mutex::new(Inner { tasks, next_id, schedule, version: 0, closing: false }),
            watchers: TaskTracker::new(),
            utc_offset: AtomicI32::new(8 * 60),
            #[cfg(test)]
            fixed_hour: Mutex::new(None),
        })
    }

    pub fn set_utc_offset(&self, minutes: i32) {
        self.utc_offset.store(minutes, Ordering::Relaxed);
    }

    /// Minutes since local midnight in the schedule time zone.
    fn local_minutes(&self) -> u32 {
        let offset = i64::from(self.utc_offset.load(Ordering::Relaxed)) * 60;
        let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 + offset;
        (seconds.rem_euclid(86_400) / 60) as u32
    }

    fn hour(&self) -> u8 {
        #[cfg(test)]
        if let Some(hour) = *self.fixed_hour.lock().unwrap() {
            return hour;
        }
        (self.local_minutes() / 60) as u8
    }

    #[cfg(test)]
    fn set_hour(&self, hour: u8) {
        *self.fixed_hour.lock().unwrap() = Some(hour);
    }

    /// Re-check schedules every few seconds: pause tasks whose hours ended and
    /// start those whose hours began.
    pub fn start_clock(self: &Arc<Self>) {
        let manager: Weak<TaskManager> = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            loop {
                ticker.tick().await;
                let Some(manager) = manager.upgrade() else { return };
                manager.tick();
            }
        });
    }

    pub fn tick(self: &Arc<Self>) {
        let mut inner = self.lock();
        self.apply_hours(&mut inner);
    }

    /// Pause running tasks outside their hours, then start what may run now.
    fn apply_hours(self: &Arc<Self>, inner: &mut Inner) {
        if inner.closing {
            return;
        }
        let hour = self.hour();
        for task in inner.tasks.iter_mut() {
            if let Some(run) = &task.run
                && task.enabled
                && !task.in_hours(hour, &inner.schedule)
            {
                task.paused = true;
                run.stop("不在运行时段");
            }
        }
        self.schedule(inner);
    }

    /// Replace the global schedule and apply it right away.
    pub fn set_schedule(self: &Arc<Self>, schedule: Schedule) -> Result<(), TaskError> {
        let mut inner = self.lock();
        Self::open(&inner)?;
        self.save(&inner.tasks, inner.next_id, &schedule)?;
        inner.schedule = schedule;
        self.apply_hours(&mut inner);
        Ok(())
    }

    pub fn data_file(&self) -> Option<&PathBuf> {
        self.store.as_ref()
    }

    pub fn task_count(&self) -> usize {
        self.lock().tasks.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Persist a candidate task list. Callers commit to memory only on success.
    fn save(&self, tasks: &[Task], next_id: u64, schedule: &Schedule) -> Result<(), TaskError> {
        let Some(path) = &self.store else { return Ok(()) };
        storage::save(path, tasks.iter().map(Task::record).collect(), next_id, schedule)
            .map_err(|e| TaskError::Storage(e.to_string()))
    }

    fn open(inner: &Inner) -> Result<(), TaskError> {
        if inner.closing { Err(TaskError::Conflict("服务正在关闭".into())) } else { Ok(()) }
    }

    fn find(inner: &Inner, id: &str) -> Result<usize, TaskError> {
        inner.tasks.iter().position(|task| task.id == id).ok_or(TaskError::NotFound)
    }

    /// Start enabled tasks in list order while the budget allows. A task that
    /// does not fit blocks those after it, so large tasks are never starved.
    fn schedule(self: &Arc<Self>, inner: &mut Inner) {
        if inner.closing {
            return;
        }
        let hour = self.hour();
        let mut allocated: u32 =
            inner.tasks.iter().filter(|t| t.run.is_some()).map(|t| t.config.settings.concurrency).sum();
        for task in inner.tasks.iter_mut() {
            if !task.enabled || task.run.is_some() || !task.in_hours(hour, &inner.schedule) {
                continue;
            }
            let needed = task.config.settings.concurrency;
            if allocated + needed > MAX_TOTAL_WORKERS {
                break;
            }
            allocated += needed;
            let run = Run::start(self.engine.clone(), task.config.clone());
            task.run = Some(run.clone());
            task.last_run = Some(run.clone());
            let (manager, id, done) = (self.clone(), task.id.clone(), run.done_token());
            self.watchers.spawn(async move {
                done.cancelled().await;
                manager.run_finished(&id, &run);
            });
        }
    }

    fn run_finished(self: &Arc<Self>, id: &str, run: &Arc<Run>) {
        let mut inner = self.lock();
        let Ok(index) = Self::find(&inner, id) else { return };
        if !inner.tasks[index].run.as_ref().is_some_and(|current| Arc::ptr_eq(current, run)) {
            return;
        }
        inner.tasks[index].run = None;
        // A shutdown or the end of the task's hours keeps the intent so the task
        // resumes later; a run that finished or hit a limit is switched off.
        let paused = std::mem::take(&mut inner.tasks[index].paused);
        let finished = (!inner.closing && !paused) || run.phase() == Phase::Completed;
        if inner.tasks[index].enabled && finished {
            inner.tasks[index].enabled = false;
            if let Err(error) = self.save(&inner.tasks, inner.next_id, &inner.schedule) {
                eprintln!("{error}");
                inner.tasks[index].error = error.to_string();
            }
        }
        self.schedule(&mut inner);
    }

    /// Start the tasks that were enabled when the file was saved.
    pub fn resume(self: &Arc<Self>) {
        let mut inner = self.lock();
        self.schedule(&mut inner);
    }

    /// Create one enabled task per URL; they start as soon as there is capacity.
    pub fn add(self: &Arc<Self>, request: TaskRequest) -> Result<Vec<String>, TaskError> {
        let mut inner = self.lock();
        Self::open(&inner)?;
        if inner.tasks.len() + request.urls.len() > MAX_TASKS {
            return Err(TaskError::Invalid(format!("任务总数最多 {MAX_TASKS} 条")));
        }
        let names = batch_names(&request.name, &request.urls);
        let mut tasks = inner.tasks.clone();
        let mut next_id = inner.next_id;
        let mut ids = Vec::with_capacity(request.urls.len());
        let created = now();
        for (url, name) in request.urls.into_iter().zip(names) {
            next_id += 1;
            let id = format!("task-{next_id}");
            let config = Config { url, settings: request.settings.clone() };
            tasks.push(Task::new(id.clone(), name, config, created, true));
            ids.push(id);
        }
        self.save(&tasks, next_id, &inner.schedule)?;
        inner.tasks = tasks;
        inner.next_id = next_id;
        self.schedule(&mut inner);
        Ok(ids)
    }

    pub fn update(self: &Arc<Self>, id: &str, request: TaskRequest) -> Result<(), TaskError> {
        if request.urls.len() != 1 {
            return Err(TaskError::Invalid("编辑任务时只能填写一条下载链接".into()));
        }
        let mut inner = self.lock();
        Self::open(&inner)?;
        let index = Self::find(&inner, id)?;
        let task = &inner.tasks[index];
        if task.busy() {
            return Err(TaskError::Conflict("请先停止该任务再编辑".into()));
        }
        let url = request.urls.into_iter().next().unwrap_or_default();
        let name = if request.name.is_empty() { default_task_name(&url) } else { request.name };
        let updated =
            Task::new(task.id.clone(), name, Config { url, settings: request.settings }, task.created_at, false);
        let mut tasks = inner.tasks.clone();
        tasks[index] = updated;
        self.save(&tasks, inner.next_id, &inner.schedule)?;
        inner.tasks = tasks;
        Ok(())
    }

    pub fn delete(self: &Arc<Self>, id: &str) -> Result<(), TaskError> {
        let mut inner = self.lock();
        Self::open(&inner)?;
        let index = Self::find(&inner, id)?;
        if inner.tasks[index].busy() {
            return Err(TaskError::Conflict("请先停止该任务再删除".into()));
        }
        let mut tasks = inner.tasks.clone();
        tasks.remove(index);
        self.save(&tasks, inner.next_id, &inner.schedule)?;
        inner.tasks = tasks;
        Ok(())
    }

    /// Switch tasks on (`Some(id)` or all idle tasks) and save the intent first.
    fn enable(self: &Arc<Self>, only: Option<&str>) -> Result<(), TaskError> {
        let mut inner = self.lock();
        Self::open(&inner)?;
        let targets: Vec<usize> = match only {
            Some(id) => {
                let index = Self::find(&inner, id)?;
                if inner.tasks[index].busy() {
                    return Err(TaskError::Conflict("该任务已在运行或正在停止".into()));
                }
                vec![index]
            }
            None => (0..inner.tasks.len()).filter(|&i| !inner.tasks[i].busy()).collect(),
        };
        if targets.is_empty() {
            return Ok(());
        }
        for &index in &targets {
            inner.tasks[index].enabled = true;
        }
        if let Err(error) = self.save(&inner.tasks, inner.next_id, &inner.schedule) {
            for &index in &targets {
                inner.tasks[index].enabled = false;
            }
            return Err(error);
        }
        for &index in &targets {
            inner.tasks[index].error.clear();
        }
        self.schedule(&mut inner);
        Ok(())
    }

    /// Switch tasks off (`Some(id)` or all) and stop their downloads.
    fn disable(self: &Arc<Self>, only: Option<&str>, reason: &str) -> Result<(), TaskError> {
        let mut inner = self.lock();
        Self::open(&inner)?;
        let targets: Vec<usize> = match only {
            Some(id) => vec![Self::find(&inner, id)?],
            None => (0..inner.tasks.len()).collect(),
        };
        let changed: Vec<usize> = targets.iter().copied().filter(|&i| inner.tasks[i].enabled).collect();
        if !changed.is_empty() {
            for &index in &changed {
                inner.tasks[index].enabled = false;
            }
            if let Err(error) = self.save(&inner.tasks, inner.next_id, &inner.schedule) {
                for &index in &changed {
                    inner.tasks[index].enabled = true;
                }
                return Err(error);
            }
        }
        for &index in &targets {
            let task = &mut inner.tasks[index];
            task.error.clear();
            task.paused = false;
            if let Some(run) = &task.run {
                run.stop(reason);
            }
        }
        Ok(())
    }

    pub fn start(self: &Arc<Self>, id: &str) -> Result<(), TaskError> {
        self.enable(Some(id))
    }

    pub fn start_all(self: &Arc<Self>) -> Result<(), TaskError> {
        self.enable(None)
    }

    pub fn stop(self: &Arc<Self>, id: &str) -> Result<(), TaskError> {
        self.disable(Some(id), "手动停止")
    }

    pub fn stop_all(self: &Arc<Self>) -> Result<(), TaskError> {
        self.disable(None, "全部停止")
    }

    pub fn export(&self) -> Vec<u8> {
        let inner = self.lock();
        let records: Vec<Record<'_>> = inner.tasks.iter().map(Task::record).collect();
        storage::export(&records, &inner.schedule, now())
    }

    /// Import tasks from an export file. Imported tasks are stopped. With
    /// `replace`, every existing task is removed first (all must be stopped)
    /// and the file's global schedule, if any, replaces the current one.
    pub fn import(self: &Arc<Self>, body: &[u8], replace: bool) -> Result<usize, TaskError> {
        let storage::Import { tasks: imported, schedule } = storage::parse_import(body).map_err(TaskError::Invalid)?;
        let mut inner = self.lock();
        Self::open(&inner)?;
        if replace && inner.tasks.iter().any(Task::busy) {
            return Err(TaskError::Conflict("替换前请先全部停止".into()));
        }
        let kept = if replace { 0 } else { inner.tasks.len() };
        if kept + imported.len() > MAX_TASKS {
            return Err(TaskError::Invalid(format!("导入后任务总数将超过 {MAX_TASKS} 条")));
        }
        let mut tasks = if replace { Vec::new() } else { inner.tasks.clone() };
        let mut next_id = inner.next_id;
        let created = now();
        let count = imported.len();
        for (name, config) in imported {
            next_id += 1;
            tasks.push(Task::new(format!("task-{next_id}"), name, config, created, false));
        }
        let schedule = schedule.filter(|_| replace).unwrap_or_else(|| inner.schedule.clone());
        self.save(&tasks, next_id, &schedule)?;
        inner.tasks = tasks;
        inner.next_id = next_id;
        inner.schedule = schedule;
        Ok(count)
    }

    pub fn board(&self) -> Board {
        let mut inner = self.lock();
        inner.version += 1;
        let mut board = Board {
            app_version: env!("CARGO_PKG_VERSION"),
            instance: self.instance.clone(),
            version: inner.version,
            tasks: Vec::with_capacity(inner.tasks.len()),
            speed: 0.0,
            running: 0,
            queued: 0,
            active: 0,
            allocated: 0,
            max_workers: MAX_TOTAL_WORKERS,
            max_tasks: MAX_TASKS,
            waiting: 0,
            clock: String::new(),
            utc_offset: String::new(),
            schedule: inner.schedule.clone(),
            auth: false,
        };
        let minutes = self.local_minutes();
        let offset = self.utc_offset.load(Ordering::Relaxed);
        board.clock = format!("{:02}:{:02}", minutes / 60, minutes % 60);
        board.utc_offset = match (offset / 60, offset % 60) {
            (hours, 0) => format!("UTC{hours:+}"),
            (hours, mins) => format!("UTC{}{}:{:02}", if offset < 0 { "-" } else { "+" }, hours.abs(), mins.abs()),
        };
        let hour = self.hour();
        for task in &inner.tasks {
            let view = view(task, hour, &inner.schedule);
            board.speed += view.speed;
            board.active += view.active;
            match view.state {
                "queued" => board.queued += 1,
                "waiting" => board.waiting += 1,
                _ if task.run.is_some() => {
                    board.running += 1;
                    board.allocated += task.config.settings.concurrency;
                }
                _ => {}
            }
            board.tasks.push(view);
        }
        board
    }

    /// Stop all downloads for shutdown, keeping each task's on/off intent.
    pub async fn close(&self, wait: Duration) {
        {
            let mut inner = self.lock();
            inner.closing = true;
            for task in &inner.tasks {
                if let Some(run) = &task.run {
                    run.stop("服务正在关闭");
                }
            }
        }
        self.watchers.close();
        let _ = tokio::time::timeout(wait, self.watchers.wait()).await;
    }
}

fn view(task: &Task, hour: u8, global: &Schedule) -> TaskView {
    let mut view = TaskView {
        id: task.id.clone(),
        name: task.name.clone(),
        config: task.config.clone(),
        created_at: task.created_at,
        state: "idle",
        speed: 0.0,
        active: 0,
        last_error: String::new(),
        server_ip: String::new(),
        connection_host: String::new(),
        ip_overridden: false,
        tls: String::new(),
        busy: task.busy(),
    };
    if let Some(run) = task.run.as_ref().or(task.last_run.as_ref()) {
        let run = run.view();
        view.state = run.phase.as_str();
        view.speed = run.speed;
        view.active = run.active;
        view.last_error = run.last_error;
        view.server_ip = run.server_ip;
        view.connection_host = run.connection_host;
        view.ip_overridden = run.ip_overridden;
        view.tls = run.tls;
    }
    if task.enabled && task.run.is_none() {
        // Waiting for capacity, or for the task's hours to begin.
        view.state = if task.in_hours(hour, global) { "queued" } else { "waiting" };
        view.speed = 0.0;
    }
    if !task.error.is_empty() {
        view.last_error = task.error.clone();
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::{Reply, default_reply, origin, request, test_engine};
    use serde_json::json;

    async fn eventually(what: &str, check: impl Fn() -> bool) {
        for _ in 0..200 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    fn state(m: &TaskManager, id: &str) -> (&'static str, bool) {
        let board = m.board();
        let task = board.tasks.iter().find(|t| t.id == id).unwrap();
        (task.state, task.busy)
    }

    fn temp_store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flow-panel-tasks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("tasks.json")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tasks_start_and_stop_independently() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let m = TaskManager::memory(test_engine());
        let urls = [format!("http://127.0.0.1:{}/slow?a", o.port), format!("http://127.0.0.1:{}/slow?b", o.port)];
        let ids = m.add(request(&urls, json!({"repeat": 0, "concurrency": 1}))).unwrap();
        assert_eq!(ids, ["task-1", "task-2"]);
        eventually("both running", || state(&m, &ids[0]).0 == "running" && state(&m, &ids[1]).0 == "running").await;
        m.stop(&ids[0]).unwrap();
        eventually("first stopped", || state(&m, &ids[0]) == ("stopped", false)).await;
        assert_eq!(state(&m, &ids[1]), ("running", true));
        m.start(&ids[0]).unwrap();
        assert!(matches!(m.start(&ids[0]), Err(TaskError::Conflict(_))), "duplicate start accepted");
        eventually("restarted", || state(&m, &ids[0]).0 == "running").await;
        m.stop_all().unwrap();
        eventually("all stopped", || m.board().tasks.iter().all(|t| !t.busy)).await;
        assert_eq!(m.board().speed, 0.0);
        m.start_all().unwrap();
        eventually("all restarted", || m.board().running == 2).await;
        m.stop_all().unwrap();
        eventually("all stopped again", || m.board().tasks.iter().all(|t| !t.busy)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn edit_and_delete_rules() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let m = TaskManager::memory(test_engine());
        let urls = [format!("http://127.0.0.1:{}/slow?1", o.port), format!("http://127.0.0.1:{}/slow?2", o.port)];
        let mut req = request(&urls, json!({"repeat": 0}));
        req.name = "下载".into();
        let ids = m.add(req).unwrap();
        let names: Vec<String> = m.board().tasks.into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["下载 · 1", "下载 · 2"]);
        let mut edit = request(&urls[..1], json!({"concurrency": 3}));
        edit.name = "更新后".into();
        assert!(matches!(m.update(&ids[0], edit), Err(TaskError::Conflict(_))));
        assert!(matches!(m.delete(&ids[0]), Err(TaskError::Conflict(_))));
        m.stop_all().unwrap();
        eventually("stopped", || m.board().tasks.iter().all(|t| !t.busy)).await;
        let mut edit = request(&urls[..1], json!({"concurrency": 3}));
        edit.name = "更新后".into();
        m.update(&ids[0], edit).unwrap();
        let task = m.board().tasks.into_iter().next().unwrap();
        assert_eq!((task.name.as_str(), task.config.settings.concurrency, task.state), ("更新后", 3, "idle"));
        assert!(matches!(m.update(&ids[0], request(&urls, json!({}))), Err(TaskError::Invalid(_))));
        m.delete(&ids[0]).unwrap();
        assert_eq!(m.board().tasks.len(), 1);
        assert_eq!(m.start("task-99"), Err(TaskError::NotFound));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failure_of_one_task_does_not_affect_another() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let m = TaskManager::memory(test_engine());
        let urls = [format!("http://127.0.0.1:{}/error", o.port), format!("http://127.0.0.1:{}/slow", o.port)];
        let ids = m.add(request(&urls, json!({"repeat": 0, "concurrency": 1}))).unwrap();
        eventually("error shown", || !m.board().tasks[0].last_error.is_empty()).await;
        assert!(m.board().tasks[1].last_error.is_empty());
        assert_eq!(state(&m, &ids[1]), ("running", true));
        m.stop_all().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrency_budget_is_first_in_first_out() {
        let o = origin("127.0.0.1:0", |_, _| Reply::Hang).await;
        let m = TaskManager::memory(test_engine());
        let url = |i: usize| format!("http://127.0.0.1:{}/hang?{i}", o.port);
        let big: Vec<String> = (0..5).map(url).collect();
        let ids = m.add(request(&big, json!({"concurrency": 64, "repeat": 0, "timeout": 120}))).unwrap();
        let small = m.add(request(&[url(9)], json!({"concurrency": 1, "repeat": 0, "timeout": 120}))).unwrap();
        let board = m.board();
        assert_eq!((board.allocated, board.running, board.queued), (256, 4, 2));
        // The small task does not jump ahead of the waiting 64-worker task.
        assert_eq!(state(&m, &small[0]).0, "queued");
        m.stop(&ids[0]).unwrap();
        eventually("queued big task started", || state(&m, &ids[4]).0 == "running").await;
        assert_eq!(m.board().allocated, 256);
        assert_eq!(state(&m, &small[0]).0, "queued");
        m.stop(&ids[1]).unwrap();
        eventually("small task started", || state(&m, &small[0]).0 == "running").await;
        assert_eq!(m.board().allocated, 193);
        m.stop_all().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hours_start_and_pause_tasks() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let m = TaskManager::memory(test_engine());
        m.set_hour(10);
        let url = format!("http://127.0.0.1:{}/slow", o.port);
        let ids = m.add(request(&[url], json!({"repeat": 0, "concurrency": 1, "hours": [11, 12]}))).unwrap();
        assert_eq!(state(&m, &ids[0]), ("waiting", true));
        assert_eq!(m.board().waiting, 1);
        m.set_hour(11);
        m.tick();
        assert_eq!(state(&m, &ids[0]), ("running", true));
        m.set_hour(12);
        m.tick();
        assert_eq!(state(&m, &ids[0]).0, "running", "stopped inside its hours");
        m.set_hour(13);
        m.tick();
        eventually("paused outside hours", || state(&m, &ids[0]) == ("waiting", true)).await;
        m.set_hour(11);
        m.tick();
        assert_eq!(state(&m, &ids[0]), ("running", true));
        m.stop(&ids[0]).unwrap();
        eventually("stopped by user", || state(&m, &ids[0]) == ("stopped", false)).await;
        m.set_hour(12);
        m.tick();
        assert_eq!(state(&m, &ids[0]), ("stopped", false), "a stopped task was started by its schedule");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_schedule_applies_to_tasks_without_hours() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let m = TaskManager::memory(test_engine());
        m.set_hour(10);
        let url = |q: &str| format!("http://127.0.0.1:{}/slow?{q}", o.port);
        let all_day: Vec<u8> = (0..24).collect();
        let follow = m.add(request(&[url("a")], json!({"repeat": 0, "concurrency": 1}))).unwrap();
        let own = m.add(request(&[url("b")], json!({"repeat": 0, "concurrency": 1, "hours": all_day}))).unwrap();
        assert_eq!(state(&m, &follow[0]).0, "running");
        m.set_schedule(Schedule { enabled: true, hours: vec![22, 23] }).unwrap();
        eventually("follower paused", || state(&m, &follow[0]) == ("waiting", true)).await;
        assert_eq!(state(&m, &own[0]).0, "running", "a task with its own hours followed the global schedule");
        m.set_hour(22);
        m.tick();
        assert_eq!(state(&m, &follow[0]).0, "running");
        m.set_hour(10);
        m.tick();
        eventually("paused again", || state(&m, &follow[0]) == ("waiting", true)).await;
        // Switching the global schedule off starts the follower at once.
        m.set_schedule(Schedule { enabled: false, hours: vec![22, 23] }).unwrap();
        assert_eq!(state(&m, &follow[0]).0, "running");

        // The schedule travels with an export and is restored by a replace import.
        m.set_schedule(Schedule { enabled: true, hours: vec![1] }).unwrap();
        let exported = m.export();
        m.stop_all().unwrap();
        eventually("stopped", || m.board().tasks.iter().all(|t| !t.busy)).await;
        m.set_schedule(Schedule::default()).unwrap();
        m.import(&exported, false).unwrap();
        assert!(!m.board().schedule.enabled, "an append import changed the global schedule");
        m.import(&exported, true).unwrap();
        assert_eq!(m.board().schedule, Schedule { enabled: true, hours: vec![1] });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intent_survives_restart_and_completion_is_saved() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let path = temp_store("restart");
        let m = TaskManager::load(test_engine(), path.clone()).unwrap();
        let urls = [format!("http://127.0.0.1:{}/slow?a", o.port), format!("http://127.0.0.1:{}/slow?b", o.port)];
        let ids = m.add(request(&urls, json!({"repeat": 0, "concurrency": 1}))).unwrap();
        let finite = m.add(request(&[format!("http://127.0.0.1:{}/file", o.port)], json!({"repeat": 3}))).unwrap();
        m.stop(&ids[0]).unwrap();
        eventually("finite task done", || state(&m, &finite[0]) == ("completed", false)).await;
        m.close(Duration::from_secs(2)).await;

        let restored = TaskManager::load(test_engine(), path.clone()).unwrap();
        restored.resume();
        assert_eq!(state(&restored, &ids[0]), ("idle", false));
        assert!(state(&restored, &ids[1]).1, "enabled task was not resumed");
        assert_eq!(state(&restored, &finite[0]), ("idle", false));
        restored.stop_all().unwrap();
        eventually("stopped", || restored.board().running == 0).await;
        restored.close(Duration::from_secs(2)).await;

        let again = TaskManager::load(test_engine(), path).unwrap();
        again.resume();
        assert!(again.board().tasks.iter().all(|t| !t.busy), "stop-all was not persisted");
        // IDs are never reused after a restart.
        let next = again.add(request(&urls[..1], json!({}))).unwrap();
        assert_eq!(next, ["task-4"]);
        again.stop_all().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn storage_failure_keeps_previous_intent() {
        let path = temp_store("failure");
        let m = TaskManager::load(test_engine(), path.clone()).unwrap();
        // Repeat until stopped, so the run cannot end by itself (and switch the
        // task off) before the test stops it.
        m.import(br#"{"tasks":[{"config":{"url":"http://127.0.0.1:9/","repeat":0}}]}"#, false).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap(); // the rename now fails
        assert!(matches!(m.start("task-1"), Err(TaskError::Storage(_))));
        assert_eq!(state(&m, "task-1"), ("idle", false));
        assert!(matches!(m.start_all(), Err(TaskError::Storage(_))));
        assert_eq!(state(&m, "task-1"), ("idle", false));
        std::fs::remove_dir(&path).unwrap();
        m.start("task-1").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(m.stop("task-1"), Err(TaskError::Storage(_))));
        assert!(state(&m, "task-1").1, "failed stop changed intent");
        std::fs::remove_dir(&path).unwrap();
        m.stop("task-1").unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_append_and_replace() {
        let o = origin("127.0.0.1:0", |p, _| default_reply(p)).await;
        let m = TaskManager::memory(test_engine());
        let url = format!("http://127.0.0.1:{}/slow", o.port);
        m.add(request(std::slice::from_ref(&url), json!({"repeat": 0, "server_ips": "127.0.0.1"}))).unwrap();
        let exported = m.export();
        assert!(String::from_utf8_lossy(&exported).contains("127.0.0.1"));
        assert_eq!(m.import(&exported, false).unwrap(), 1);
        let board = m.board();
        assert_eq!(board.tasks.len(), 2);
        assert_eq!(board.tasks[1].config, board.tasks[0].config);
        assert!(!board.tasks[1].busy, "imported task started automatically");
        assert!(matches!(m.import(&exported, true), Err(TaskError::Conflict(_))));
        m.stop_all().unwrap();
        eventually("stopped", || m.board().running == 0).await;
        assert_eq!(m.import(&exported, true).unwrap(), 1);
        let board = m.board();
        assert_eq!(board.tasks.len(), 1);
        assert_eq!(board.tasks[0].id, "task-3");
        assert!(matches!(m.import(b"{\"tasks\":[{}]}", false), Err(TaskError::Invalid(_))));
    }
}
