package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/url"
	"path"
	"strconv"
	"strings"
	"sync"
	"time"
)

const maxTotalWorkers = 256

type Task struct {
	ID         string
	Name       string
	Config     Config
	CreatedAt  float64
	Enabled    bool
	engine     *Engine
	run        *Run
	storeError string
}

type TaskView struct {
	ID             string  `json:"id"`
	Name           string  `json:"name"`
	Config         Config  `json:"config"`
	CreatedAt      float64 `json:"created_at"`
	State          string  `json:"state"`
	Speed          float64 `json:"speed"`
	Active         int     `json:"active"`
	LastError      string  `json:"last_error"`
	ServerIP       string  `json:"server_ip"`
	ConnectionHost string  `json:"connection_host"`
	IPOverridden   bool    `json:"ip_overridden"`
	Busy           bool    `json:"busy"`
}

type TaskBoard struct {
	Instance   string     `json:"instance"`
	Version    uint64     `json:"version"`
	Tasks      []TaskView `json:"tasks"`
	Speed      float64    `json:"speed"`
	Running    int        `json:"running"`
	Active     int        `json:"active"`
	Allocated  int        `json:"allocated"`
	MaxWorkers int        `json:"max_workers"`
	MaxTasks   int        `json:"max_tasks"`
}

type TaskManager struct {
	mu       sync.Mutex
	tasks    []*Task
	nextID   uint64
	version  uint64
	instance string
	closing  bool
	dataFile string
	watchers sync.WaitGroup
}

func newTaskManager() *TaskManager {
	return &TaskManager{instance: strconv.FormatInt(time.Now().UnixNano(), 36), tasks: []*Task{}}
}

func taskName(body []byte) (string, error) {
	var value struct {
		Name string `json:"name"`
	}
	if err := json.Unmarshal(body, &value); err != nil {
		return "", errors.New("任务名称格式错误")
	}
	value.Name = strings.TrimSpace(value.Name)
	if len([]rune(value.Name)) > 80 {
		return "", errors.New("任务名称最多 80 个字符")
	}
	return value.Name, nil
}

func defaultTaskName(raw string) string {
	parsed, _ := url.Parse(raw)
	name := path.Base(parsed.Path)
	if name == "/" || name == "." || name == "" {
		name = parsed.Hostname()
	}
	chars := []rune(name)
	if len(chars) > 80 {
		name = string(chars[:80])
	}
	return name
}

func cloneConfig(config Config) Config {
	result := config
	result.URLs = append([]string(nil), config.URLs...)
	result.ServerIPs = append([]string{}, config.ServerIPs...)
	return result
}

func taskBusy(task *Task) bool {
	return task.Enabled || task.run != nil
}

func (m *TaskManager) findLocked(id string) (*Task, int) {
	for index, task := range m.tasks {
		if task.ID == id {
			return task, index
		}
	}
	return nil, -1
}

func (m *TaskManager) Add(config Config, name string) ([]string, error) {
	return m.add(config, name, false)
}

func (m *TaskManager) AddAndStart(config Config, name string) ([]string, error) {
	return m.add(config, name, true)
}

func (m *TaskManager) add(config Config, name string, enabled bool) ([]string, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closing {
		return nil, errors.New("服务正在关闭")
	}
	if len(config.URLs)+len(m.tasks) > maxURLs {
		return nil, errors.New("任务总数最多 500 条")
	}
	ids := make([]string, 0, len(config.URLs))
	tasks := append([]*Task{}, m.tasks...)
	nextID := m.nextID
	for i, raw := range config.URLs {
		c := cloneConfig(config)
		c.URLs = []string{raw}
		title := name
		if title == "" {
			title = defaultTaskName(raw)
		} else if len(config.URLs) > 1 {
			title = fmt.Sprintf("%s · %d", name, i+1)
		}
		nextID++
		id := "task-" + strconv.FormatUint(nextID, 10)
		tasks = append(tasks, &Task{ID: id, Name: title, Config: c, CreatedAt: float64(time.Now().UnixMilli()) / 1000, Enabled: enabled, engine: &Engine{}})
		ids = append(ids, id)
	}
	if err := m.saveTasks(tasks, nextID); err != nil {
		return nil, err
	}
	m.tasks, m.nextID = tasks, nextID
	m.scheduleLocked()
	return ids, nil
}

var errTaskMissing = errors.New("任务不存在")

func (m *TaskManager) Start(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closing {
		return errors.New("服务正在关闭")
	}
	task, _ := m.findLocked(id)
	if task == nil {
		return errTaskMissing
	}
	if taskBusy(task) {
		return errors.New("该任务已在运行或正在停止")
	}
	task.Enabled = true
	if err := m.saveTasks(m.tasks, m.nextID); err != nil {
		task.Enabled = false
		return err
	}
	task.storeError = ""
	m.scheduleLocked()
	return nil
}

func (m *TaskManager) Stop(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	task, _ := m.findLocked(id)
	if task == nil {
		return errTaskMissing
	}
	if m.closing {
		return errors.New("服务正在关闭")
	}
	previous := task.Enabled
	task.Enabled = false
	if previous {
		if err := m.saveTasks(m.tasks, m.nextID); err != nil {
			task.Enabled = previous
			return err
		}
	}
	task.storeError = ""
	task.engine.Stop("手动停止")
	return nil
}

func (m *TaskManager) StopAll() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closing {
		return errors.New("服务正在关闭")
	}
	previous := make([]bool, len(m.tasks))
	changed := false
	for i, task := range m.tasks {
		previous[i] = task.Enabled
		changed = changed || task.Enabled
		task.Enabled = false
	}
	if changed {
		if err := m.saveTasks(m.tasks, m.nextID); err != nil {
			for i, task := range m.tasks {
				task.Enabled = previous[i]
			}
			return err
		}
	}
	for _, task := range m.tasks {
		task.storeError = ""
		task.engine.Stop("全部停止")
	}
	return nil
}

// Called once after the HTTP listener has been opened. Loaded tasks with Enabled
// set resume automatically; user-stopped and completed tasks remain idle.
func (m *TaskManager) Resume() {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.scheduleLocked()
}

func (m *TaskManager) scheduleLocked() {
	if m.closing {
		return
	}
	allocated := 0
	for _, task := range m.tasks {
		if task.run != nil {
			allocated += task.Config.Concurrency
		}
	}
	for _, task := range m.tasks {
		if !task.Enabled || task.run != nil || allocated+task.Config.Concurrency > maxTotalWorkers {
			continue
		}
		run, err := task.engine.Start(cloneConfig(task.Config))
		if err != nil {
			task.storeError = err.Error()
			continue
		}
		task.run = run
		allocated += task.Config.Concurrency
		m.watchers.Add(1)
		go m.runFinished(task, run)
	}
}

func (m *TaskManager) runFinished(task *Task, run *Run) {
	defer m.watchers.Done()
	<-run.done
	m.mu.Lock()
	defer m.mu.Unlock()
	if task.run != run {
		return
	}
	task.run = nil
	// Shutdown cancels requests but preserves Enabled for the next process.
	// Finite tasks that naturally finish must not restart after a later reboot.
	run.mu.Lock()
	completed := run.state == "completed"
	run.mu.Unlock()
	if task.Enabled && (!m.closing || completed) {
		task.Enabled = false
		if err := m.saveTasks(m.tasks, m.nextID); err != nil {
			task.storeError = err.Error()
			log.Print(err)
		}
	}
	m.scheduleLocked()
}

func (m *TaskManager) Update(id string, config Config, name string) error {
	if len(config.URLs) != 1 {
		return errors.New("编辑任务时只能填写一条下载链接")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	task, index := m.findLocked(id)
	if task == nil {
		return errTaskMissing
	}
	if taskBusy(task) {
		return errors.New("请先停止该任务再编辑")
	}
	if m.closing {
		return errors.New("服务正在关闭")
	}
	if name == "" {
		name = defaultTaskName(config.URLs[0])
	}
	updated := &Task{ID: task.ID, Name: name, Config: cloneConfig(config), CreatedAt: task.CreatedAt, engine: &Engine{}}
	tasks := append([]*Task{}, m.tasks...)
	tasks[index] = updated
	if err := m.saveTasks(tasks, m.nextID); err != nil {
		return err
	}
	m.tasks = tasks
	return nil
}

func (m *TaskManager) Delete(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	task, index := m.findLocked(id)
	if task == nil {
		return errTaskMissing
	}
	if taskBusy(task) {
		return errors.New("请先停止该任务再删除")
	}
	if m.closing {
		return errors.New("服务正在关闭")
	}
	tasks := append([]*Task{}, m.tasks[:index]...)
	tasks = append(tasks, m.tasks[index+1:]...)
	if err := m.saveTasks(tasks, m.nextID); err != nil {
		return err
	}
	m.tasks = tasks
	return nil
}

func (m *TaskManager) Board() TaskBoard {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.version++
	board := TaskBoard{Instance: m.instance, Version: m.version, Tasks: []TaskView{}, MaxWorkers: maxTotalWorkers, MaxTasks: maxURLs}
	for _, task := range m.tasks {
		view := task.live()
		board.Tasks = append(board.Tasks, view)
		board.Speed += view.Speed
		board.Active += view.Active
		if task.run != nil {
			board.Running++
			board.Allocated += task.Config.Concurrency
		}
	}
	return board
}

// Return operational state only. No traffic totals, counters, logs or charts in the task API.
func (task *Task) live() TaskView {
	view := TaskView{ID: task.ID, Name: task.Name, Config: cloneConfig(task.Config), CreatedAt: task.CreatedAt, State: "idle", Busy: taskBusy(task)}
	if task.Enabled && task.run == nil {
		view.State = "queued"
		view.LastError = task.storeError
		return view
	}
	task.engine.mu.Lock()
	r := task.engine.current
	task.engine.mu.Unlock()
	if r == nil {
		return view
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	view.State = r.state
	if view.State == "running" || view.State == "stopping" {
		count := len(r.history)
		if count > 5 {
			count = 5
		}
		for _, point := range r.history[len(r.history)-count:] {
			view.Speed += point.Speed
		}
		if count > 0 {
			view.Speed /= float64(count)
		}
	}
	for _, job := range r.jobs {
		view.Active += job.Active
		view.LastError = job.LastError
		view.ServerIP = job.ServerIP
		view.ConnectionHost = job.ConnectionHost
		view.IPOverridden = job.IPOverridden
	}
	if task.storeError != "" {
		view.LastError = task.storeError
	}
	return view
}

func (m *TaskManager) Close(ctx context.Context) {
	m.mu.Lock()
	m.closing = true
	runs := []*Run{}
	for _, task := range m.tasks {
		task.engine.Stop("服务正在关闭")
		task.engine.mu.Lock()
		run := task.engine.current
		task.engine.mu.Unlock()
		if run != nil {
			runs = append(runs, run)
		}
	}
	m.mu.Unlock()
	for _, run := range runs {
		select {
		case <-run.done:
		case <-ctx.Done():
			return
		}
	}
	finished := make(chan struct{})
	go func() { m.watchers.Wait(); close(finished) }()
	select {
	case <-finished:
	case <-ctx.Done():
	}
}
