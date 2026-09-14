package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"net/http/httptest"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func persistentManager(t *testing.T, filename string) *TaskManager {
	t.Helper()
	m, err := loadTaskManager(filename)
	if err != nil {
		t.Fatal(err)
	}
	cleanupTasks(t, m)
	return m
}

func TestTaskPersistenceRoundTrip(t *testing.T) {
	filename := filepath.Join(t.TempDir(), "nested", "tasks.json")
	m := persistentManager(t, filename)
	c := testConfig(t, []string{"https://a.test/file", "https://b.test/file"}, map[string]any{"server_ips": "127.0.0.1\n::1", "concurrency": 3, "repeat": 0, "rate_mib": 2.5})
	ids, err := m.Add(c, "重启保留")
	if err != nil {
		t.Fatal(err)
	}
	// Restore before Close: definitions must already be on disk when Add returns.
	restored := persistentManager(t, filename)
	before, after := m.Board().Tasks, restored.Board().Tasks
	if !reflect.DeepEqual(before, after) {
		t.Fatalf("task definitions changed across reload: %+v -> %+v", before, after)
	}
	edited := cloneConfig(c)
	edited.URLs = []string{"https://edited.test/new"}
	edited.ServerIPs = []string{"192.0.2.1", "2001:db8::1"}
	if err := restored.Update(ids[0], edited, "已修改"); err != nil {
		t.Fatal(err)
	}
	if err := restored.Delete(ids[1]); err != nil {
		t.Fatal(err)
	}
	reloaded := persistentManager(t, filename)
	want := restored.Board().Tasks
	if got := reloaded.Board().Tasks; !reflect.DeepEqual(got, want) {
		t.Fatalf("edits/deletes were not saved: %+v", got)
	}
	newIDs, err := reloaded.Add(edited, "新任务")
	if err != nil || newIDs[0] == ids[0] || newIDs[0] == ids[1] {
		t.Fatalf("task IDs reused: %v, %v", newIDs, err)
	}
	for _, task := range reloaded.Board().Tasks {
		if err := reloaded.Delete(task.ID); err != nil {
			t.Fatal(err)
		}
	}
	if len(persistentManager(t, filename).Board().Tasks) != 0 {
		t.Fatal("deleted tasks reappeared")
	}
	files, err := os.ReadDir(filepath.Dir(filename))
	if err != nil || len(files) != 1 || files[0].Name() != "tasks.json" {
		t.Fatalf("unexpected temporary/download files: %v, %v", files, err)
	}
}

func TestPersistenceDoesNotSaveSpeedSamples(t *testing.T) {
	origin, _ := newOrigin(t)
	filename := filepath.Join(t.TempDir(), "tasks.json")
	m := persistentManager(t, filename)
	ids, err := m.Add(testConfig(t, []string{origin.URL + "/slow"}, map[string]any{"repeat": 0}), "")
	if err != nil {
		t.Fatal(err)
	}
	if err := m.Start(ids[0]); err != nil {
		t.Fatal(err)
	}
	before, _ := os.ReadFile(filename)
	beforeInfo, _ := os.Stat(filename)
	for i := 0; i < 10; i++ {
		m.Board()
	}
	// Loading restores desired state; Resume starts it after the HTTP listener is ready.
	reloaded := persistentManager(t, filename).Board()
	if reloaded.Running != 0 || !reloaded.Tasks[0].Busy || reloaded.Tasks[0].State != "queued" || reloaded.Speed != 0 {
		t.Fatal(reloaded)
	}
	after, _ := os.ReadFile(filename)
	afterInfo, _ := os.Stat(filename)
	if !bytes.Equal(before, after) || !beforeInfo.ModTime().Equal(afterInfo.ModTime()) {
		t.Fatal("runtime activity rewrote task configuration")
	}
	for _, unwanted := range []string{`"state"`, `"speed"`, `"total_bytes"`, `"history"`, `"events"`, `"success"`} {
		if bytes.Contains(after, []byte(unwanted)) {
			t.Fatalf("persisted runtime field %s", unwanted)
		}
	}
}

func TestPersistenceFailureRollsBackChanges(t *testing.T) {
	filename := filepath.Join(t.TempDir(), "tasks.json")
	m := persistentManager(t, filename)
	c := testConfig(t, []string{"https://a.test/file"}, nil)
	ids, err := m.Add(c, "原任务")
	if err != nil {
		t.Fatal(err)
	}
	saved, _ := os.ReadFile(filename)
	before := m.Board().Tasks
	// A directory cannot be replaced by a file on Windows or Linux.
	m.dataFile = filepath.Join(filepath.Dir(filename), "blocked")
	if err := os.Mkdir(m.dataFile, 0755); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Add(c, "不应添加"); !errors.Is(err, errTaskStorage) {
		t.Fatalf("Add did not report storage error: %v", err)
	}
	if err := m.Update(ids[0], c, "不应修改"); !errors.Is(err, errTaskStorage) {
		t.Fatalf("Update did not report storage error: %v", err)
	}
	if err := m.Delete(ids[0]); !errors.Is(err, errTaskStorage) {
		t.Fatalf("Delete did not report storage error: %v", err)
	}
	if !reflect.DeepEqual(before, m.Board().Tasks) {
		t.Fatal("failed save still changed tasks in memory")
	}
	rec := httptest.NewRecorder()
	body, _ := json.Marshal(c)
	panelHandler(m).ServeHTTP(rec, httptest.NewRequest("POST", "/api/tasks", bytes.NewReader(body)))
	if rec.Code != 500 || !strings.Contains(rec.Body.String(), "保存任务配置失败") {
		t.Fatalf("save failure reported as success: %d %s", rec.Code, rec.Body.String())
	}
	got, _ := os.ReadFile(filename)
	if !bytes.Equal(got, saved) {
		t.Fatal("previous configuration damaged")
	}
	temps, _ := filepath.Glob(filepath.Join(filepath.Dir(filename), ".flow-panel-tasks-*.tmp"))
	if len(temps) != 0 {
		t.Fatal("failed saves left temporary files", temps)
	}
}

func TestInvalidTaskFileIsPreserved(t *testing.T) {
	for _, body := range []string{
		`{"version":`,
		`{"version":999,"tasks":[]}`,
		`{"version":1,"tasks":[{"id":"task-1","config":{"urls":["file:///bad"]}}]}`,
		`{"version":1,"tasks":[{"id":"task-1","config":{"urls":["https://a.test"]}},{"id":"task-1","config":{"urls":["https://b.test"]}}]}`,
	} {
		filename := filepath.Join(t.TempDir(), "tasks.json")
		if err := os.WriteFile(filename, []byte(body), 0600); err != nil {
			t.Fatal(err)
		}
		if _, err := loadTaskManager(filename); err == nil {
			t.Fatal("invalid task file was silently accepted", body)
		}
		got, _ := os.ReadFile(filename)
		if string(got) != body {
			t.Fatal("loading invalid configuration overwrote original")
		}
	}
}
