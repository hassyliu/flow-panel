package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

func awaitAuto(t *testing.T, check func() bool) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if check() {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("automatic task transition did not occur")
}

func closeManager(t *testing.T, m *TaskManager) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	m.Close(ctx)
}

func TestAutoStartAndRestartPreserveStopIntent(t *testing.T) {
	var a, b atomic.Int64
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/a" {
			a.Add(1)
		} else {
			b.Add(1)
		}
		w.WriteHeader(200)
		w.(http.Flusher).Flush()
		<-r.Context().Done()
	}))
	defer origin.Close()
	filename := filepath.Join(t.TempDir(), "tasks.json")
	m := persistentManager(t, filename)
	ids, err := m.AddAndStart(testConfig(t, []string{origin.URL + "/a", origin.URL + "/b"}, map[string]any{"repeat": 0, "concurrency": 2}), "自动下载")
	if err != nil {
		t.Fatal(err)
	}
	awaitAuto(t, func() bool { return a.Load() == 2 && b.Load() == 2 })
	if err := m.Stop(ids[0]); err != nil {
		t.Fatal(err)
	}
	waitTask(t, m, ids[0], "stopped")
	closeManager(t, m)
	beforeA, beforeB := a.Load(), b.Load()
	restored := persistentManager(t, filename)
	restored.Resume()
	awaitAuto(t, func() bool { return b.Load() == beforeB+2 })
	if a.Load() != beforeA || restored.Board().Tasks[0].Busy || !restored.Board().Tasks[1].Busy {
		t.Fatal("manual stop was lost across restart", restored.Board())
	}
	if err := restored.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	awaitAuto(t, func() bool { return a.Load() == beforeA+2 })
	if err := restored.StopAll(); err != nil {
		t.Fatal(err)
	}
	closeManager(t, restored)
	final := persistentManager(t, filename)
	final.Resume()
	if board := final.Board(); board.Running != 0 || board.Tasks[0].Busy || board.Tasks[1].Busy {
		t.Fatal("stop-all did not persist", board)
	}
}

func TestAutoStartBatchWaitsForCapacity(t *testing.T) {
	origin, _ := newOrigin(t)
	m := newTaskManager()
	cleanupTasks(t, m)
	urls := []string{}
	for i := 0; i < 5; i++ {
		urls = append(urls, fmt.Sprintf("%s/hang?task=%d", origin.URL, i))
	}
	ids, err := m.AddAndStart(testConfig(t, urls, map[string]any{"repeat": 0, "concurrency": 64}), "排队测试")
	if err != nil {
		t.Fatal(err)
	}
	if board := m.Board(); board.Allocated != 256 || board.Running != 4 || board.Tasks[4].State != "queued" {
		t.Fatal(board)
	}
	if err := m.Stop(ids[0]); err != nil {
		t.Fatal(err)
	}
	awaitAuto(t, func() bool { return m.Board().Tasks[4].State == "running" })
	if board := m.Board(); board.Allocated != 256 || board.Tasks[0].Busy {
		t.Fatal("automatic queue exceeded budget", board)
	}
	// A stopped queued task must not be started when another slot is released.
	more, err := m.AddAndStart(testConfig(t, []string{origin.URL + "/hang?extra"}, map[string]any{"repeat": 0, "concurrency": 64}), "")
	if err != nil {
		t.Fatal(err)
	}
	if err := m.Stop(more[0]); err != nil {
		t.Fatal(err)
	}
	if err := m.Stop(ids[1]); err != nil {
		t.Fatal(err)
	}
	waitTask(t, m, ids[1], "stopped")
	if board := m.Board(); board.Tasks[5].Busy || board.Allocated != 192 {
		t.Fatal("stopped queue entry was started", board)
	}
}

func TestCompletedAutoTaskStaysStoppedAfterRestart(t *testing.T) {
	var hits atomic.Int64
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits.Add(1)
		_, _ = w.Write([]byte("done"))
	}))
	defer origin.Close()
	filename := filepath.Join(t.TempDir(), "tasks.json")
	m := persistentManager(t, filename)
	ids, err := m.AddAndStart(testConfig(t, []string{origin.URL}, map[string]any{"repeat": 3}), "有限下载")
	if err != nil {
		t.Fatal(err)
	}
	waitTask(t, m, ids[0], "completed")
	closeManager(t, m)
	restored := persistentManager(t, filename)
	restored.Resume()
	if board := restored.Board(); board.Tasks[0].Busy || board.Running != 0 || hits.Load() != 3 {
		t.Fatal("completed task restarted", board, hits.Load())
	}
	data, _ := os.ReadFile(filename)
	var saved taskFile
	if err := json.Unmarshal(data, &saved); err != nil || saved.Version != 2 || saved.Tasks[0].Enabled {
		t.Fatal("completion was not persisted", string(data), err)
	}
}

func TestLegacyTasksAutomaticallyStartAfterUpgrade(t *testing.T) {
	origin, _ := newOrigin(t)
	c := testConfig(t, []string{origin.URL + "/hang"}, map[string]any{"repeat": 0})
	data := taskFile{Version: 1, NextID: 7, Tasks: []storedTask{{ID: "task-7", Name: "旧任务", Config: c}}}
	body, _ := json.Marshal(data)
	filename := filepath.Join(t.TempDir(), "tasks.json")
	if err := os.WriteFile(filename, body, 0600); err != nil {
		t.Fatal(err)
	}
	m := persistentManager(t, filename)
	m.Resume()
	if board := m.Board(); board.Tasks[0].State != "running" || board.Tasks[0].ID != "task-7" {
		t.Fatal(board)
	}
	if err := m.Stop("task-7"); err != nil {
		t.Fatal(err)
	}
	closeManager(t, m)
	migrated, _ := os.ReadFile(filename)
	var saved taskFile
	if err := json.Unmarshal(migrated, &saved); err != nil || saved.Version != 2 || saved.Tasks[0].Enabled {
		t.Fatal("legacy stop was not migrated", string(migrated), err)
	}
}

func TestStartStopStorageFailurePreservesIntent(t *testing.T) {
	origin, _ := newOrigin(t)
	filename := filepath.Join(t.TempDir(), "tasks.json")
	m := persistentManager(t, filename)
	ids, err := m.Add(testConfig(t, []string{origin.URL + "/hang"}, map[string]any{"repeat": 0}), "")
	if err != nil {
		t.Fatal(err)
	}
	blocked := filepath.Join(filepath.Dir(filename), "blocked")
	if err := os.Mkdir(blocked, 0755); err != nil {
		t.Fatal(err)
	}
	m.dataFile = blocked
	if err := m.Start(ids[0]); !errors.Is(err, errTaskStorage) || m.Board().Tasks[0].Busy {
		t.Fatal("failed start changed state", err)
	}
	m.dataFile = filename
	if err := m.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	m.dataFile = blocked
	if err := m.Stop(ids[0]); !errors.Is(err, errTaskStorage) || !m.Board().Tasks[0].Busy {
		t.Fatal("failed stop changed state", err)
	}
	if err := m.StopAll(); !errors.Is(err, errTaskStorage) || !m.Board().Tasks[0].Busy {
		t.Fatal("failed stop-all changed state", err)
	}
	m.dataFile = filename
	closeManager(t, m)
	restored := persistentManager(t, filename)
	restored.Resume()
	if !restored.Board().Tasks[0].Busy {
		t.Fatal("failed stop changed saved intent")
	}
}
