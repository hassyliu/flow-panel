package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// Persist definitions and desired on/off state, never payloads or speed samples.
type storedTask struct {
	ID        string  `json:"id"`
	Name      string  `json:"name"`
	Config    Config  `json:"config"`
	CreatedAt float64 `json:"created_at"`
	Enabled   bool    `json:"enabled"`
}

type taskFile struct {
	Version int          `json:"version"`
	NextID  uint64       `json:"next_id"`
	Tasks   []storedTask `json:"tasks"`
}

var errTaskStorage = errors.New("保存任务配置失败")

func loadTaskManager(filename string) (*TaskManager, error) {
	m := newTaskManager()
	if strings.TrimSpace(filename) == "" {
		return nil, errors.New("任务配置文件路径不能为空")
	}
	absolute, err := filepath.Abs(filename)
	if err != nil {
		return nil, err
	}
	m.dataFile = absolute
	body, err := os.ReadFile(absolute)
	if errors.Is(err, os.ErrNotExist) {
		return m, nil
	}
	if err != nil {
		return nil, fmt.Errorf("读取任务配置 %s：%w", absolute, err)
	}
	var data taskFile
	if err := json.Unmarshal(body, &data); err != nil {
		return nil, fmt.Errorf("任务配置文件格式错误，原文件未修改：%w", err)
	}
	if (data.Version != 1 && data.Version != 2) || len(data.Tasks) > maxURLs {
		return nil, errors.New("任务配置文件版本或任务数量无效，原文件未修改")
	}
	seen := map[string]bool{}
	m.nextID = data.NextID
	for _, saved := range data.Tasks {
		number, err := strconv.ParseUint(strings.TrimPrefix(saved.ID, "task-"), 10, 64)
		if err != nil || number == 0 || saved.ID != "task-"+strconv.FormatUint(number, 10) || seen[saved.ID] {
			return nil, errors.New("任务配置包含无效或重复的任务编号，原文件未修改")
		}
		seen[saved.ID] = true
		if number > m.nextID {
			m.nextID = number
		}
		body, err := json.Marshal(saved.Config)
		if err != nil {
			return nil, fmt.Errorf("任务 %s 的配置无效：%w", saved.ID, err)
		}
		config, err := parseConfig(body)
		if err != nil || len(config.URLs) != 1 {
			return nil, fmt.Errorf("任务 %s 的下载配置无效，原文件未修改", saved.ID)
		}
		if saved.Name == "" {
			saved.Name = defaultTaskName(config.URLs[0])
		}
		// Version 1 did not record stop intent. Existing tasks opt into auto-start
		// on their first upgrade; subsequent explicit stops are preserved by v2.
		enabled := saved.Enabled || data.Version == 1
		m.tasks = append(m.tasks, &Task{ID: saved.ID, Name: saved.Name, Config: config, CreatedAt: saved.CreatedAt, Enabled: enabled, engine: &Engine{}})
	}
	return m, nil
}

// Caller holds m.mu. Commit the file before publishing changes to memory or API.
func (m *TaskManager) saveTasks(tasks []*Task, nextID uint64) error {
	if m.dataFile == "" { // In-memory managers are used by engine and API tests.
		return nil
	}
	data := taskFile{Version: 2, NextID: nextID, Tasks: []storedTask{}}
	for _, task := range tasks {
		data.Tasks = append(data.Tasks, storedTask{task.ID, task.Name, task.Config, task.CreatedAt, task.Enabled})
	}
	body, err := json.MarshalIndent(data, "", "  ")
	if err == nil {
		err = replaceTaskFile(m.dataFile, append(body, '\n'))
	}
	if err != nil {
		return fmt.Errorf("%w：%v", errTaskStorage, err)
	}
	return nil
}

func replaceTaskFile(filename string, body []byte) error {
	directory := filepath.Dir(filename)
	if err := os.MkdirAll(directory, 0755); err != nil {
		return err
	}
	file, err := os.CreateTemp(directory, ".flow-panel-tasks-*.tmp")
	if err != nil {
		return err
	}
	defer os.Remove(file.Name())
	if _, err := file.Write(body); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	return os.Rename(file.Name(), filename)
}
