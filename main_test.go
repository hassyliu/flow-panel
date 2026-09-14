package main

import (
	"bytes"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func testConfig(t *testing.T, urls []string, extra map[string]any) Config {
	t.Helper()
	m := map[string]any{"urls": urls, "interval": 0, "concurrency": 4}
	for k, v := range extra {
		m[k] = v
	}
	body, _ := json.Marshal(m)
	c, err := parseConfig(body)
	if err != nil {
		t.Fatal(err)
	}
	return c
}
func runDownload(t *testing.T, e *Engine, c Config) Snapshot {
	t.Helper()
	r, err := e.Start(c)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { r.stop("test cleanup") })
	select {
	case <-r.done:
	case <-time.After(6 * time.Second):
		t.Fatal("download timed out")
	}
	return e.Snapshot()
}
func newOrigin(t *testing.T) (*httptest.Server, *atomic.Int64) {
	t.Helper()
	var active, peak atomic.Int64
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		current := active.Add(1)
		defer active.Add(-1)
		for old := peak.Load(); current > old && !peak.CompareAndSwap(old, current); old = peak.Load() {
		}
		switch r.URL.Path {
		case "/error":
			w.WriteHeader(503)
			return
		case "/redirect":
			http.Redirect(w, r, "/file", 302)
			return
		case "/loop":
			http.Redirect(w, r, "/loop", 302)
			return
		case "/hang":
			w.Header().Set("Content-Length", "65536")
			w.WriteHeader(200)
			w.(http.Flusher).Flush()
			<-r.Context().Done()
			return
		}
		if r.URL.Path == "/truncated" {
			w.Header().Set("Content-Length", "131072")
		} else if r.URL.Path != "/stream" {
			w.Header().Set("Content-Length", "65536")
		}
		block := bytes.Repeat([]byte("x"), 8192)
		for i := 0; i < 8; i++ {
			if _, err := w.Write(block); err != nil {
				return
			}
			w.(http.Flusher).Flush()
			if r.URL.Path == "/slow" {
				time.Sleep(15 * time.Millisecond)
			}
		}
	}))
	t.Cleanup(server.Close)
	return server, &peak
}

func TestBatchRepeatAndConcurrency(t *testing.T) {
	s, peak := newOrigin(t)
	e := &Engine{}
	v := runDownload(t, e, testConfig(t, []string{s.URL + "/slow", s.URL + "/file"}, map[string]any{"repeat": 3}))
	if v.Success != 6 || v.TotalBytes != 6*65536 || v.Active != 0 || peak.Load() < 2 || peak.Load() > 4 {
		t.Fatalf("bad batch: %+v, peak %d", v, peak.Load())
	}
}
func TestSingleRepeatNotMultiplied(t *testing.T) {
	s, _ := newOrigin(t)
	v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + "/file"}, map[string]any{"concurrency": 64}))
	if v.Success != 1 {
		t.Fatalf("got %d", v.Success)
	}
}
func TestInfiniteStopAndRestart(t *testing.T) {
	s, _ := newOrigin(t)
	e := &Engine{}
	r, err := e.Start(testConfig(t, []string{s.URL + "/slow"}, map[string]any{"repeat": 0}))
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(160 * time.Millisecond)
	e.Stop("手动停止")
	select {
	case <-r.done:
	case <-time.After(time.Second):
		t.Fatal("stop timed out")
	}
	if e.Snapshot().State != "stopped" {
		t.Fatal("not stopped")
	}
	v := runDownload(t, e, testConfig(t, []string{s.URL + "/file"}, nil))
	if v.TotalBytes != 65536 || v.RunID == r.id {
		t.Fatal("restart did not reset")
	}
}
func TestStopBlockedBody(t *testing.T) {
	s, _ := newOrigin(t)
	e := &Engine{}
	r, _ := e.Start(testConfig(t, []string{s.URL + "/hang"}, nil))
	time.Sleep(60 * time.Millisecond)
	e.Stop("手动停止")
	select {
	case <-r.done:
	case <-time.After(time.Second):
		t.Fatal("blocked response not cancelled")
	}
	if e.Snapshot().Cancelled != 1 {
		t.Fatal(e.Snapshot())
	}
}
func TestLimits(t *testing.T) {
	s, _ := newOrigin(t)
	for _, tc := range []struct {
		name   string
		extra  map[string]any
		reason string
	}{{"duration", map[string]any{"repeat": 0, "duration": .2}, "时长"}, {"traffic", map[string]any{"repeat": 0, "max_gib": .0001}, "流量"}} {
		t.Run(tc.name, func(t *testing.T) {
			v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + "/slow"}, tc.extra))
			if !strings.Contains(v.Reason, tc.reason) || v.State != "stopped" {
				t.Fatal(v)
			}
			if tc.name == "traffic" && v.TotalBytes > 107375+4*chunkSize {
				t.Fatal("quota overshoot")
			}
		})
	}
}
func TestAggregateRateLimit(t *testing.T) {
	s, _ := newOrigin(t)
	start := time.Now()
	v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + "/file"}, map[string]any{"repeat": 2, "rate_mib": .125}))
	if v.Success != 2 || time.Since(start) < 800*time.Millisecond {
		t.Fatalf("limiter failed %+v", v)
	}
}
func TestResponses(t *testing.T) {
	s, _ := newOrigin(t)
	for _, tc := range []struct {
		path            string
		success, failed int
		count           int64
	}{{"/redirect", 1, 0, 65536}, {"/stream", 1, 0, 65536}, {"/error", 0, 1, 0}, {"/truncated", 0, 1, 65536}, {"/loop", 0, 1, 0}} {
		t.Run(tc.path, func(t *testing.T) {
			v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + tc.path}, nil))
			if v.Success != tc.success || v.Failed != tc.failed || v.TotalBytes != tc.count {
				t.Fatal(v)
			}
		})
	}
}
func TestIdleReadTimeout(t *testing.T) {
	s, _ := newOrigin(t)
	v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + "/hang"}, map[string]any{"timeout": 1}))
	if v.Failed != 1 || v.Elapsed > 2.5 {
		t.Fatal(v)
	}
}
func TestFailureBackoffAllowsHealthyURL(t *testing.T) {
	s, _ := newOrigin(t)
	v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + "/error", s.URL + "/file"}, map[string]any{"repeat": 0, "duration": .25, "interval": .01, "concurrency": 1}))
	if v.Success < 2 || v.Failed != 1 {
		t.Fatal(v)
	}
}
func TestNoDownloadFiles(t *testing.T) {
	t.Chdir(t.TempDir())
	s, _ := newOrigin(t)
	v := runDownload(t, &Engine{}, testConfig(t, []string{s.URL + "/file"}, nil))
	files, err := os.ReadDir(".")
	if err != nil || len(files) != 0 || v.Success != 1 {
		t.Fatalf("unexpected files: %v, %v", files, err)
	}
}

func TestPinnedHTTPHostAndRedirect(t *testing.T) {
	var mu sync.Mutex
	hosts := []string{}
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		hosts = append(hosts, r.Host)
		mu.Unlock()
		if r.URL.Path == "/redirect" {
			_, port, _ := net.SplitHostPort(r.Host)
			http.Redirect(w, r, "http://DOWNLOAD.TEST.:"+port+"/file", 302)
			return
		}
		_, _ = w.Write([]byte("test"))
	}))
	defer s.Close()
	_, port, _ := net.SplitHostPort(strings.TrimPrefix(s.URL, "http://"))
	c := testConfig(t, []string{"http://download.test:" + port + "/redirect"}, map[string]any{"server_ips": "127.0.0.1"})
	v := runDownload(t, &Engine{}, c)
	if v.Success != 1 || v.Jobs[0].ServerIP != "127.0.0.1" || v.Jobs[0].ConnectionHost != "DOWNLOAD.TEST." || !v.Jobs[0].IPOverridden {
		t.Fatal(v)
	}
	if len(hosts) != 2 || hosts[0] != "download.test:"+port || hosts[1] != "DOWNLOAD.TEST.:"+port {
		t.Fatal(hosts)
	}
}
func TestPinnedIPv6(t *testing.T) {
	ln, err := net.Listen("tcp6", "[::1]:0")
	if err != nil {
		t.Skip(err)
	}
	s := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { _, _ = w.Write([]byte("ipv6")) }))
	_ = s.Listener.Close()
	s.Listener = ln
	s.Start()
	defer s.Close()
	_, port, _ := net.SplitHostPort(ln.Addr().String())
	v := runDownload(t, &Engine{}, testConfig(t, []string{"http://download.test:" + port + "/file"}, map[string]any{"server_ips": "[::1]"}))
	if v.Success != 1 || v.Jobs[0].ServerIP != "::1" {
		t.Fatal(v)
	}
}
func TestPinnedTLSIdentity(t *testing.T) {
	cert, err := tls.LoadX509KeyPair("testdata/test-server.pem", "testdata/test-server.key")
	if err != nil {
		t.Fatal(err)
	}
	ca, err := os.ReadFile("testdata/test-ca.pem")
	if err != nil {
		t.Fatal(err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(ca) {
		t.Fatal("bad fixture")
	}
	var mu sync.Mutex
	var sni, host string
	s := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		host = r.Host
		mu.Unlock()
		_, _ = w.Write([]byte("tls"))
	}))
	s.Config.ErrorLog = nil
	s.TLS = &tls.Config{Certificates: []tls.Certificate{cert}, GetConfigForClient: func(info *tls.ClientHelloInfo) (*tls.Config, error) {
		mu.Lock()
		sni = info.ServerName
		mu.Unlock()
		return nil, nil
	}}
	s.StartTLS()
	defer s.Close()
	_, port, _ := net.SplitHostPort(s.Listener.Addr().String())
	for _, tc := range []struct {
		name, domain string
		trust        bool
		success      int
	}{{"valid", "download.test", true, 1}, {"wrong_name", "wrong.test", true, 0}, {"untrusted", "download.test", false, 0}} {
		t.Run(tc.name, func(t *testing.T) {
			pool := x509.NewCertPool()
			if tc.trust {
				pool = roots
			}
			e := &Engine{tlsConfig: &tls.Config{RootCAs: pool}}
			v := runDownload(t, e, testConfig(t, []string{"https://" + tc.domain + ":" + port + "/file"}, map[string]any{"server_ips": []string{"127.0.0.1"}}))
			if v.Success != tc.success {
				t.Fatal(v)
			}
			mu.Lock()
			defer mu.Unlock()
			if sni != tc.domain {
				t.Fatal(sni)
			}
			if tc.success == 1 && host != tc.domain+":"+port {
				t.Fatal(host)
			}
		})
	}
}
func TestNormalResolutionWithoutMapping(t *testing.T) {
	s, _ := newOrigin(t)
	u := strings.Replace(s.URL, "127.0.0.1", "localhost", 1) + "/file"
	v := runDownload(t, &Engine{}, testConfig(t, []string{u}, nil))
	if v.Success != 1 || v.Jobs[0].IPOverridden {
		t.Fatal(v)
	}
}
func TestValidation(t *testing.T) {
	for _, bad := range []string{`null`, `[]`, `{"urls":[]}`, `{"urls":["file:///tmp/x"]}`, `{"urls":["http://a:0/"]}`, `{"urls":["http://a"],"concurrency":65}`, `{"urls":["http://a"],"repeat":1.2}`, `{"urls":["http://a"],"timeout":null}`, `{"urls":["http://a"],"concurrency":true}`} {
		if _, err := parseConfig([]byte(bad)); err == nil {
			t.Errorf("accepted %s", bad)
		}
	}
	for _, bad := range []string{"https://a.test=127.0.0.1", "a.test:80=127.0.0.1", "*.test=127.0.0.1", "a.test=127.0.0.1:80", "a.test=fe80::1%eth0", "a.test=wrong", "a.test=127.0.0.1\nA.TEST=127.0.0.2"} {
		raw, _ := json.Marshal(bad)
		if _, err := parseServerIPs(raw); err == nil {
			t.Errorf("accepted %s", bad)
		}
	}
	c := testConfig(t, []string{"http://下载.test/file", "http://下载.test/file"}, map[string]any{"server_ips": "2001:db8::1"})
	if len(c.URLs) != 1 || c.URLs[0] != "http://xn--ghq880n.test/file" || c.ServerIPs[0] != "2001:db8::1" {
		t.Fatal(c)
	}
}
func TestEmbeddedUIAndUnauthenticatedAPI(t *testing.T) {
	manager := newTaskManager()
	s := httptest.NewServer(panelHandler(manager))
	defer s.Close()
	for _, path := range []string{"/", "/app.js", "/style.css", "/favicon.svg", "/healthz", "/api/state"} {
		resp, err := http.Get(s.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if resp.StatusCode != 200 {
			t.Fatal(path, resp.Status)
		}
		if path == "/" && (bytes.Contains(body, []byte("login-dialog")) || bytes.Contains(body, []byte("累计下载")) || bytes.Contains(body, []byte("canvas"))) {
			t.Fatal("unwanted login or statistics UI")
		}
	}
}

func TestBinaryCanServeFromEmptyDirectory(t *testing.T) {
	// The embedded filesystem is independent of the process working directory.
	temp := t.TempDir()
	t.Chdir(temp)
	rec := httptest.NewRecorder()
	panelHandler(newTaskManager()).ServeHTTP(rec, httptest.NewRequest("GET", "/", nil))
	if rec.Code != 200 || !strings.Contains(rec.Body.String(), "下载任务") {
		t.Fatal("embedded page unavailable")
	}
	if _, err := os.Stat(filepath.Join(temp, "static")); !os.IsNotExist(err) {
		t.Fatal("unexpected static folder")
	}
}
