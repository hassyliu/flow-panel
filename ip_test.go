package main

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
)

func TestServerIPInputs(t *testing.T) {
	for _, tc := range []struct {
		name string
		raw  string
		want []string
		bad  bool
	}{
		{"omitted", "", []string{}, false},
		{"empty", `" \n\r\n"`, []string{}, false},
		{"null", `null`, []string{}, false},
		{"empty_array", `[]`, []string{}, false},
		{"text", `" 192.168.1.10\r\n\n[2001:0db8::1]\n192.168.1.10\n2001:db8::1 "`, []string{"192.168.1.10", "2001:db8::1"}, false},
		{"array", `["127.0.0.1", "::1", "::ffff:127.0.0.1"]`, []string{"127.0.0.1", "::1"}, false},
		{"mapping", `"example.test=127.0.0.1"`, nil, true},
		{"port", `"127.0.0.1:80"`, nil, true},
		{"v6_port", `"[::1]:443"`, nil, true},
		{"invalid", `"999.1.1.1"`, nil, true},
		{"hostname", `"localhost"`, nil, true},
		{"csv", `"127.0.0.1,127.0.0.2"`, nil, true},
		{"object", `{"example.test":"127.0.0.1"}`, nil, true},
		{"number", `123`, nil, true},
		{"mixed_array", `["127.0.0.1", 123]`, nil, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := parseServerIPs(json.RawMessage(tc.raw))
			if (err != nil) != tc.bad || !tc.bad && !reflect.DeepEqual(got, tc.want) {
				t.Fatalf("got %v, %v; want %v, bad=%v", got, err, tc.want, tc.bad)
			}
		})
	}
	ips := make([]string, 501)
	for i := range ips {
		ips[i] = fmt.Sprintf("10.0.%d.%d", i/256, i%256)
	}
	raw, _ := json.Marshal(ips)
	if _, err := parseServerIPs(raw); err == nil {
		t.Fatal("accepted more than 500 unique IPs")
	}
	if _, err := parseConfig([]byte(`{"urls":["http://example.test/file"],"host_overrides":"example.test=127.0.0.1"}`)); err == nil {
		t.Fatal("silently ignored legacy IP setting")
	}
}

// Two real loopback IPs on the same port expose wrong routing or connection reuse.
func dualIPOrigin(t *testing.T, handler http.Handler) string {
	t.Helper()
	first, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	_, port, _ := net.SplitHostPort(first.Addr().String())
	second, err := net.Listen("tcp4", "127.0.0.2:"+port)
	if err != nil {
		first.Close()
		t.Fatal(err)
	}
	for _, listener := range []net.Listener{first, second} {
		server := httptest.NewUnstartedServer(handler)
		server.Listener.Close()
		server.Listener = listener
		server.Start()
		t.Cleanup(server.Close)
	}
	return port
}

func TestRoundRobinIPDownloadsAndRedirects(t *testing.T) {
	var mu sync.Mutex
	hits := map[string]int{}
	var wrong atomic.Int64
	port := dualIPOrigin(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		local := r.Context().Value(http.LocalAddrContextKey).(net.Addr).String()
		ip, port, _ := net.SplitHostPort(local)
		if r.Host != "download.test:"+port {
			wrong.Add(1)
		}
		if r.URL.Path == "/start" {
			http.Redirect(w, r, "/file?chosen="+ip, http.StatusFound)
			return
		}
		if r.URL.Query().Get("chosen") != ip {
			wrong.Add(1) // A same-host redirect must keep this download's chosen IP.
		}
		mu.Lock()
		hits[ip]++
		mu.Unlock()
		_, _ = w.Write([]byte("payload"))
	}))
	c := testConfig(t, []string{"http://download.test:" + port + "/start"}, map[string]any{
		"server_ips": "127.0.0.1\n127.0.0.2", "repeat": 129, "concurrency": 8,
	})
	v := runDownload(t, &Engine{}, c)
	mu.Lock()
	defer mu.Unlock()
	if v.Success != 129 || v.Failed != 0 || wrong.Load() != 0 || v.TotalBytes != 129*7 || !v.Jobs[0].IPOverridden {
		t.Fatalf("incorrect downloads: %+v, wrong routes=%d", v, wrong.Load())
	}
	if hits["127.0.0.1"] != 65 || hits["127.0.0.2"] != 64 || len(hits) != 2 {
		t.Fatalf("concurrent requests did not share a round-robin cursor: %v", hits)
	}
	t.Logf("129 downloads reached the two IPs in a 65/64 split: %v", hits)
}

func TestOrderedIPSequenceAndRestart(t *testing.T) {
	var mu sync.Mutex
	sequence := []string{}
	port := dualIPOrigin(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ip, _, _ := net.SplitHostPort(r.Context().Value(http.LocalAddrContextKey).(net.Addr).String())
		mu.Lock()
		sequence = append(sequence, ip)
		mu.Unlock()
		_, _ = w.Write([]byte("ok"))
	}))
	c := testConfig(t, []string{"http://download.test:" + port + "/file"}, map[string]any{
		"server_ips": "127.0.0.2\n127.0.0.1", "repeat": 5, "concurrency": 1,
	})
	e := &Engine{}
	for i := 0; i < 2; i++ {
		if v := runDownload(t, e, c); v.Success != 5 {
			t.Fatal(v)
		}
	}
	want := []string{"127.0.0.2", "127.0.0.1", "127.0.0.2", "127.0.0.1", "127.0.0.2"}
	want = append(want, want...)
	mu.Lock()
	defer mu.Unlock()
	if !reflect.DeepEqual(sequence, want) {
		t.Fatalf("IP order/restart reset: got %v, want %v", sequence, want)
	}
}

func TestOrderedIPFailureAdvancesToNext(t *testing.T) {
	var mu sync.Mutex
	sequence := []string{}
	port := dualIPOrigin(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ip, _, _ := net.SplitHostPort(r.Context().Value(http.LocalAddrContextKey).(net.Addr).String())
		mu.Lock()
		sequence = append(sequence, ip)
		mu.Unlock()
		if ip == "127.0.0.2" {
			w.WriteHeader(http.StatusServiceUnavailable)
			return
		}
		_, _ = w.Write([]byte("ok"))
	}))
	c := testConfig(t, []string{"http://download.test:" + port + "/file"}, map[string]any{
		"server_ips": "127.0.0.2\n127.0.0.1", "repeat": 4, "concurrency": 1,
	})
	v := runDownload(t, &Engine{}, c)
	mu.Lock()
	defer mu.Unlock()
	if v.Success != 2 || v.Failed != 2 || !reflect.DeepEqual(sequence, []string{"127.0.0.2", "127.0.0.1", "127.0.0.2", "127.0.0.1"}) {
		t.Fatalf("failure did not advance the IP cursor: %+v, %v", v, sequence)
	}
}

func TestIPPoolDoesNotOverrideRedirectHost(t *testing.T) {
	var source, target atomic.Int64
	port := dualIPOrigin(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		local := r.Context().Value(http.LocalAddrContextKey).(net.Addr).String()
		ip, port, _ := net.SplitHostPort(local)
		if r.URL.Path == "/start" {
			source.Add(1)
			http.Redirect(w, r, "http://127.0.0.1:"+port+"/file", http.StatusFound)
			return
		}
		if ip != "127.0.0.1" || r.Host != "127.0.0.1:"+port {
			w.WriteHeader(400)
			return
		}
		target.Add(1)
		_, _ = w.Write([]byte("redirected"))
	}))
	v := runDownload(t, &Engine{}, testConfig(t, []string{"http://download.test:" + port + "/start"}, map[string]any{"server_ips": "127.0.0.2"}))
	if v.Success != 1 || source.Load() != 1 || target.Load() != 1 || v.Jobs[0].IPOverridden || v.Jobs[0].ServerIP != "127.0.0.1" {
		t.Fatal(v)
	}
}

func TestTaskIPPoolsAPIAndIsolation(t *testing.T) {
	var wrong atomic.Int64
	port := dualIPOrigin(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		local := r.Context().Value(http.LocalAddrContextKey).(net.Addr).String()
		ip, _, _ := net.SplitHostPort(local)
		wantIP, wantHost := "127.0.0.1", "a.test:"
		if r.URL.Path == "/b" {
			wantIP, wantHost = "127.0.0.2", "b.test:"
		}
		if ip != wantIP || !strings.HasPrefix(r.Host, wantHost) {
			wrong.Add(1)
		}
		_, _ = w.Write([]byte("ok"))
	}))
	m := newTaskManager()
	cleanupTasks(t, m)
	handler := panelHandler(m)
	call := func(method, path string, data any) TaskBoard {
		t.Helper()
		body, _ := json.Marshal(data)
		rec := httptest.NewRecorder()
		handler.ServeHTTP(rec, httptest.NewRequest(method, path, strings.NewReader(string(body))))
		if rec.Code != http.StatusOK {
			t.Fatalf("%s %s: %d %s", method, path, rec.Code, rec.Body.String())
		}
		var board TaskBoard
		if err := json.Unmarshal(rec.Body.Bytes(), &board); err != nil {
			t.Fatal(err)
		}
		return board
	}
	board := call("POST", "/api/tasks", map[string]any{
		"urls":       []string{"http://a.test:" + port + "/a", "http://b.test:" + port + "/b"},
		"server_ips": "127.0.0.1\n127.0.0.2", "repeat": 0, "interval": 0,
	})
	if len(board.Tasks) != 2 || len(board.Tasks[0].Config.ServerIPs) != 2 || len(board.Tasks[1].Config.ServerIPs) != 2 {
		t.Fatal(board)
	}
	for _, task := range board.Tasks {
		call("POST", "/api/tasks/"+task.ID+"/stop", nil)
		waitTask(t, m, task.ID, "stopped")
	}
	wrong.Store(0)
	// Board snapshots and batch tasks must not share mutable IP slices.
	copy := m.Board()
	copy.Tasks[0].Config.ServerIPs[0] = "192.0.2.1"
	if m.Board().Tasks[0].Config.ServerIPs[0] != "127.0.0.1" {
		t.Fatal("snapshot mutation changed task")
	}
	for i, task := range board.Tasks {
		call("PUT", "/api/tasks/"+task.ID, map[string]any{"urls": task.Config.URLs, "server_ips": []string{fmt.Sprintf("127.0.0.%d", i+1)}, "repeat": 2, "interval": 0})
	}
	for _, task := range board.Tasks {
		call("POST", "/api/tasks/"+task.ID+"/start", nil)
	}
	for _, task := range board.Tasks {
		waitTask(t, m, task.ID, "completed")
	}
	if wrong.Load() != 0 {
		t.Fatal("one task used another task's IP pool or Host")
	}
	for _, task := range m.Board().Tasks {
		if task.LastError != "" || task.ServerIP != task.Config.ServerIPs[0] || !task.IPOverridden {
			t.Fatal(task)
		}
	}
	first := board.Tasks[0]
	cleared := call("PUT", "/api/tasks/"+first.ID, map[string]any{"urls": first.Config.URLs, "server_ips": ""})
	if len(cleared.Tasks[0].Config.ServerIPs) != 0 || len(cleared.Tasks[1].Config.ServerIPs) != 1 {
		t.Fatal("clearing IPs affected another task")
	}
}
