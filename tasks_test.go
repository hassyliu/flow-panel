package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func cleanupTasks(t *testing.T, m *TaskManager) {
	t.Helper()
	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		m.Close(ctx)
	})
}
func waitTask(t *testing.T, m *TaskManager, id string, want string) {
	t.Helper()
	until := time.Now().Add(2 * time.Second)
	for time.Now().Before(until) {
		for _, task := range m.Board().Tasks {
			if task.ID == id && task.State == want && !task.Busy {
				return
			}
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatalf("task %s did not reach %s: %+v", id, want, m.Board())
}

func TestTaskIndependentStartStopAndRestart(t *testing.T) {
	var a, b atomic.Int64
	origin := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		counter := &a
		if r.URL.Path == "/b" {
			counter = &b
		}
		w.WriteHeader(200)
		w.(http.Flusher).Flush()
		for {
			select {
			case <-r.Context().Done():
				return
			case <-time.After(5 * time.Millisecond):
				if _, err := w.Write(make([]byte, 4096)); err != nil {
					return
				}
				w.(http.Flusher).Flush()
				counter.Add(1)
			}
		}
	}))
	defer origin.Close()
	manager := newTaskManager()
	cleanupTasks(t, manager)
	config := testConfig(t, []string{origin.URL + "/a", origin.URL + "/b"}, map[string]any{"repeat": 0, "concurrency": 2})
	ids, err := manager.Add(config, "")
	if err != nil || len(ids) != 2 {
		t.Fatal(ids, err)
	}
	if a.Load() != 0 || b.Load() != 0 {
		t.Fatal("adding tasks started downloads")
	}
	if err = manager.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	time.Sleep(50 * time.Millisecond)
	if b.Load() != 0 || a.Load() == 0 {
		t.Fatal("wrong task started")
	}
	if err = manager.Start(ids[1]); err != nil {
		t.Fatal(err)
	}
	time.Sleep(60 * time.Millisecond)
	if err = manager.Stop(ids[0]); err != nil {
		t.Fatal(err)
	}
	waitTask(t, manager, ids[0], "stopped")
	time.Sleep(20 * time.Millisecond)
	aBefore, bBefore := a.Load(), b.Load()
	time.Sleep(60 * time.Millisecond)
	if a.Load() != aBefore || b.Load() <= bBefore {
		t.Fatalf("stop isolation failed: a %d/%d, b %d/%d", aBefore, a.Load(), bBefore, b.Load())
	}
	if err = manager.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	time.Sleep(50 * time.Millisecond)
	if a.Load() <= aBefore {
		t.Fatal("task restart did not download")
	}
	manager.StopAll()
	for _, id := range ids {
		waitTask(t, manager, id, "stopped")
	}
	if manager.Board().Speed != 0 {
		t.Fatal("stopped speed was not zero")
	}
}

func TestTaskEditDeleteAndNames(t *testing.T) {
	origin, _ := newOrigin(t)
	m := newTaskManager()
	cleanupTasks(t, m)
	c := testConfig(t, []string{origin.URL + "/hang", origin.URL + "/file"}, nil)
	ids, err := m.Add(c, "下载")
	if err != nil {
		t.Fatal(err)
	}
	if m.Board().Tasks[0].Name != "下载 · 1" || m.Board().Tasks[1].Name != "下载 · 2" {
		t.Fatal("batch names missing")
	}
	if err = m.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	updated := testConfig(t, []string{origin.URL + "/file"}, map[string]any{"concurrency": 3, "repeat": 2})
	if m.Update(ids[0], updated, "更新后") == nil || m.Delete(ids[0]) == nil {
		t.Fatal("modified a running task")
	}
	_ = m.Stop(ids[0])
	waitTask(t, m, ids[0], "stopped")
	if err = m.Update(ids[0], updated, "更新后"); err != nil {
		t.Fatal(err)
	}
	task := m.Board().Tasks[0]
	if task.Name != "更新后" || task.Config.Concurrency != 3 || task.State != "idle" {
		t.Fatal(task)
	}
	if err = m.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	waitTask(t, m, ids[0], "completed")
	if err = m.Delete(ids[0]); err != nil {
		t.Fatal(err)
	}
	if len(m.Board().Tasks) != 1 {
		t.Fatal("delete affected unrelated task")
	}
	if m.Start("absent") != errTaskMissing {
		t.Fatal("missing task accepted")
	}
}

func TestTaskFailureDoesNotStopOtherTask(t *testing.T) {
	origin, _ := newOrigin(t)
	m := newTaskManager()
	cleanupTasks(t, m)
	ids, _ := m.Add(testConfig(t, []string{origin.URL + "/error", origin.URL + "/slow"}, map[string]any{"repeat": 0}), "")
	for _, id := range ids {
		if err := m.Start(id); err != nil {
			t.Fatal(err)
		}
	}
	time.Sleep(200 * time.Millisecond)
	views := m.Board().Tasks
	if views[0].LastError == "" || views[1].LastError != "" || !views[1].Busy {
		t.Fatal(views)
	}
}

func TestTaskConcurrentStartDoesNotDuplicate(t *testing.T) {
	origin, _ := newOrigin(t)
	m := newTaskManager()
	cleanupTasks(t, m)
	ids, _ := m.Add(testConfig(t, []string{origin.URL + "/hang"}, nil), "")
	var group sync.WaitGroup
	results := make(chan error, 2)
	for i := 0; i < 2; i++ {
		group.Add(1)
		go func() { defer group.Done(); results <- m.Start(ids[0]) }()
	}
	group.Wait()
	success := 0
	for i := 0; i < 2; i++ {
		if <-results == nil {
			success++
		}
	}
	if success != 1 {
		t.Fatal("duplicate start", success)
	}
}

func TestTaskNoStatisticsInAPI(t *testing.T) {
	m := newTaskManager()
	_, _ = m.Add(testConfig(t, []string{"http://example.test/file"}, nil), "")
	payload, _ := json.Marshal(m.Board())
	for _, field := range []string{"total_bytes", "history", "success", "failed", "cancelled", "peak_speed", "events", "average_speed"} {
		if strings.Contains(string(payload), `"`+field+`"`) {
			t.Fatal("unwanted statistic", field)
		}
	}
	first := m.Board()
	next := m.Board()
	if next.Version <= first.Version || next.Instance != first.Instance {
		t.Fatal("invalid snapshot ordering")
	}
}

func TestTaskAPIWorkflow(t *testing.T) {
	origin, _ := newOrigin(t)
	m := newTaskManager()
	cleanupTasks(t, m)
	s := httptest.NewServer(panelHandler(m))
	defer s.Close()
	request := func(method, path, body string) (int, TaskBoard) {
		t.Helper()
		req, _ := http.NewRequest(method, s.URL+path, strings.NewReader(body))
		req.Header.Set("Content-Type", "application/json")
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		raw, _ := io.ReadAll(resp.Body)
		var board TaskBoard
		_ = json.Unmarshal(raw, &board)
		return resp.StatusCode, board
	}
	c := testConfig(t, []string{origin.URL + "/slow", origin.URL + "/file"}, map[string]any{"repeat": 0})
	body, _ := json.Marshal(c)
	code, board := request("POST", "/api/tasks", string(body))
	if code != 200 || len(board.Tasks) != 2 {
		t.Fatal(code, board)
	}
	id := board.Tasks[0].ID
	if !board.Tasks[0].Busy || !board.Tasks[1].Busy {
		t.Fatal("API did not automatically start new tasks", board)
	}
	code, _ = request("POST", "/api/tasks/"+id+"/start", "{}")
	if code != 409 {
		t.Fatal(code)
	}
	code, _ = request("POST", "/api/tasks/"+id+"/stop", "{}")
	if code != 200 {
		t.Fatal(code)
	}
	waitTask(t, m, id, "stopped")
	c.URLs = c.URLs[:1]
	body, _ = json.Marshal(c)
	code, _ = request("PUT", "/api/tasks/"+id, string(body))
	if code != 200 {
		t.Fatal(code)
	}
	code, board = request("DELETE", "/api/tasks/"+id, "{}")
	if code != 200 || len(board.Tasks) != 1 {
		t.Fatal(code, board)
	}
	code, _ = request("POST", "/api/tasks/not-found/start", "{}")
	if code != 404 {
		t.Fatal(code)
	}
	code, _ = request("POST", "/api/tasks", "{}")
	if code != 400 {
		t.Fatal(code)
	}
	code, _ = request("POST", "/api/tasks/stop-all", "{}")
	if code != 200 {
		t.Fatal(code)
	}
}

func TestTaskConcurrencyBudget(t *testing.T) {
	origin, _ := newOrigin(t)
	m := newTaskManager()
	cleanupTasks(t, m)
	ids := []string{}
	for i := 0; i < 5; i++ {
		created, _ := m.Add(testConfig(t, []string{origin.URL + "/hang"}, map[string]any{"concurrency": 64}), "")
		ids = append(ids, created[0])
	}
	for _, id := range ids[:4] {
		if err := m.Start(id); err != nil {
			t.Fatal(err)
		}
	}
	if err := m.Start(ids[4]); err != nil {
		t.Fatal(err)
	}
	if board := m.Board(); board.Tasks[4].State != "queued" || board.Allocated != 256 {
		t.Fatal("task did not wait for capacity", board)
	}
	_ = m.Stop(ids[0])
	waitTask(t, m, ids[0], "stopped")
	if board := m.Board(); board.Tasks[4].State != "running" || board.Allocated != 256 {
		t.Fatal("queued task did not start automatically", board)
	}
}
