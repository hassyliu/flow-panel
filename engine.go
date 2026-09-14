package main

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptrace"
	"sync"
	"time"
)

type Job struct {
	ID                int    `json:"id"`
	URL               string `json:"url"`
	Started           int    `json:"started"`
	Success           int    `json:"success"`
	Failed            int    `json:"failed"`
	Cancelled         int    `json:"cancelled"`
	Active            int    `json:"active"`
	Bytes             int64  `json:"bytes"`
	LastError         string `json:"last_error"`
	Status            string `json:"status"`
	ConnectionHost    string `json:"connection_host,omitempty"`
	ServerIP          string `json:"server_ip,omitempty"`
	IPOverridden      bool   `json:"ip_overridden"`
	consecutiveErrors int
	readyAt           time.Time
}
type Event struct {
	ID      int     `json:"id"`
	Time    float64 `json:"time"`
	Level   string  `json:"level"`
	Message string  `json:"message"`
}
type Point struct {
	Time  float64 `json:"time"`
	Speed float64 `json:"speed"`
}
type Snapshot struct {
	State        string  `json:"state"`
	RunID        string  `json:"run_id"`
	Reason       string  `json:"reason"`
	TotalBytes   int64   `json:"total_bytes"`
	Speed        float64 `json:"speed"`
	AverageSpeed float64 `json:"average_speed"`
	PeakSpeed    float64 `json:"peak_speed"`
	Elapsed      float64 `json:"elapsed"`
	StartedAt    float64 `json:"started_at"`
	Active       int     `json:"active"`
	Success      int     `json:"success"`
	Failed       int     `json:"failed"`
	Cancelled    int     `json:"cancelled"`
	Config       *Config `json:"config"`
	Jobs         []Job   `json:"jobs"`
	History      []Point `json:"history"`
	Events       []Event `json:"events"`
}
type Engine struct {
	mu        sync.Mutex
	current   *Run
	tlsConfig *tls.Config
}
type Run struct {
	mu                         sync.Mutex
	config                     Config
	state, reason, id          string
	ctx                        context.Context
	cancel                     context.CancelFunc
	done                       chan struct{}
	jobs                       []*Job
	nextJob                    int
	nextIP                     int
	events                     []Event
	sequence                   int
	history                    []Point
	total                      int64
	success, failed, cancelled int
	started, ended, nextRead   time.Time
	peak                       float64
	transport                  *http.Transport
}

func seconds(v float64) time.Duration { return time.Duration(v * float64(time.Second)) }
func waitContext(ctx context.Context, d time.Duration) bool {
	if d <= 0 {
		return ctx.Err() == nil
	}
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}

// Each read gets an idle timeout, rather than imposing a total file duration.
type idleConn struct {
	net.Conn
	timeout time.Duration
}

// Pin one IP for this download, including redirects within the original host.
// Redirects to another host use normal DNS and never inherit this IP pool.
type downloadRouteKey struct{}
type downloadRoute struct{ host, ip string }

func (c *idleConn) Read(b []byte) (int, error) {
	_ = c.Conn.SetReadDeadline(time.Now().Add(c.timeout))
	return c.Conn.Read(b)
}
func (c *idleConn) Write(b []byte) (int, error) {
	_ = c.Conn.SetWriteDeadline(time.Now().Add(c.timeout))
	return c.Conn.Write(b)
}

func (e *Engine) Start(config Config) (*Run, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if e.current != nil {
		select {
		case <-e.current.done:
		default:
			return nil, errors.New("已有任务正在运行，请先停止")
		}
	}
	ctx, cancel := context.WithCancel(context.Background())
	r := &Run{config: config, state: "running", reason: "正在下载", id: fmt.Sprintf("%x", time.Now().UnixNano()), ctx: ctx, cancel: cancel, done: make(chan struct{}), started: time.Now(), events: []Event{}, history: []Point{}}
	for i, u := range config.URLs {
		r.jobs = append(r.jobs, &Job{ID: i + 1, URL: u})
	}
	timeout := seconds(config.Timeout)
	r.transport = &http.Transport{DisableCompression: true, DisableKeepAlives: true, TLSHandshakeTimeout: timeout, ResponseHeaderTimeout: timeout, TLSClientConfig: e.tlsConfig,
		DialContext: func(ctx context.Context, network, address string) (net.Conn, error) {
			host, port, err := net.SplitHostPort(address)
			if err != nil {
				return nil, err
			}
			key, err := hostKey(host)
			if err != nil {
				return nil, err
			}
			if route, ok := ctx.Value(downloadRouteKey{}).(downloadRoute); ok && route.host == key {
				address = net.JoinHostPort(route.ip, port)
			}
			conn, err := (&net.Dialer{Timeout: timeout}).DialContext(ctx, network, address)
			if err != nil {
				return nil, err
			}
			return &idleConn{Conn: conn, timeout: timeout}, nil
		}}
	r.logLocked(fmt.Sprintf("已启动 %d 条链接，%d 个并发", len(r.jobs), config.Concurrency), "info")
	if len(config.ServerIPs) > 0 {
		r.logLocked(fmt.Sprintf("每次下载按顺序轮询 %d 个服务器 IP", len(config.ServerIPs)), "info")
	}
	e.current = r
	go r.supervise()
	return r, nil
}

func (e *Engine) Stop(reason string) {
	e.mu.Lock()
	r := e.current
	e.mu.Unlock()
	if r != nil {
		r.stop(reason)
	}
}
func (r *Run) stop(reason string) {
	r.mu.Lock()
	if r.state != "running" {
		r.mu.Unlock()
		return
	}
	r.state = "stopping"
	r.reason = reason
	r.logLocked(reason, "info")
	r.mu.Unlock()
	r.cancel()
	r.transport.CloseIdleConnections()
}
func (r *Run) logLocked(message, level string) {
	r.sequence++
	r.events = append(r.events, Event{r.sequence, float64(time.Now().UnixMilli()) / 1000, level, message})
	if len(r.events) > 200 {
		r.events = append([]Event(nil), r.events[len(r.events)-200:]...)
	}
}
func (r *Run) supervise() {
	var workers sync.WaitGroup
	for i := 0; i < r.config.Concurrency; i++ {
		workers.Add(1)
		go func() { defer workers.Done(); r.worker() }()
	}
	finished := make(chan struct{})
	go func() { workers.Wait(); close(finished) }()
	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()
	var deadline <-chan time.Time
	var timer *time.Timer
	if r.config.Duration > 0 {
		timer = time.NewTimer(seconds(r.config.Duration))
		deadline = timer.C
		defer timer.Stop()
	}
	previousTime := r.started
	var previousBytes int64
	sample := func() {
		r.mu.Lock()
		defer r.mu.Unlock()
		now := time.Now()
		elapsed := now.Sub(previousTime).Seconds()
		speed := float64(r.total-previousBytes) / elapsed
		if speed > r.peak {
			r.peak = speed
		}
		r.history = append(r.history, Point{now.Sub(r.started).Seconds(), speed})
		if len(r.history) > 120 {
			r.history = append([]Point(nil), r.history[len(r.history)-120:]...)
		}
		previousTime = now
		previousBytes = r.total
	}
	for {
		select {
		case <-ticker.C:
			sample()
		case <-deadline:
			r.stop("已达到运行时长上限")
			deadline = nil
		case <-finished:
			sample()
			r.cancel()
			r.transport.CloseIdleConnections()
			r.mu.Lock()
			r.ended = time.Now()
			if r.state == "stopping" {
				r.state = "stopped"
			} else {
				r.state = "completed"
				r.reason = "所有下载已结束"
				if r.failed > 0 {
					r.reason = "下载已结束，部分请求失败"
				}
			}
			r.logLocked(fmt.Sprintf("%s；成功 %d，失败 %d，取消 %d", r.reason, r.success, r.failed, r.cancelled), "info")
			r.mu.Unlock()
			close(r.done)
			return
		}
	}
}

func (r *Run) claim() (*Job, time.Duration) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.ctx.Err() != nil {
		return nil, 0
	}
	now := time.Now()
	var earliest time.Time
	for offset := 0; offset < len(r.jobs); offset++ {
		index := (r.nextJob + offset) % len(r.jobs)
		job := r.jobs[index]
		if r.config.Repeat > 0 && job.Started >= r.config.Repeat {
			continue
		}
		if job.readyAt.After(now) {
			if earliest.IsZero() || job.readyAt.Before(earliest) {
				earliest = job.readyAt
			}
			continue
		}
		r.nextJob = (index + 1) % len(r.jobs)
		job.Started++
		job.Active++
		return job, 0
	}
	if !earliest.IsZero() {
		return nil, time.Until(earliest)
	}
	return nil, 0
}
func (r *Run) worker() {
	buffer := make([]byte, chunkSize)
	for r.ctx.Err() == nil {
		job, wait := r.claim()
		if job == nil {
			if wait > 0 {
				if !waitContext(r.ctx, wait) {
					return
				}
				continue
			}
			return
		}
		err := r.download(job, buffer)
		r.mu.Lock()
		job.Active--
		if r.ctx.Err() != nil {
			job.Cancelled++
			r.cancelled++
		} else if err != nil {
			job.Failed++
			r.failed++
			message := []rune(err.Error())
			if len(message) > 240 {
				message = message[:240]
			}
			job.LastError = string(message)
			job.consecutiveErrors++
			exponent := job.consecutiveErrors - 1
			if exponent > 5 {
				exponent = 5
			}
			delay := 1 << exponent
			if delay > 30 {
				delay = 30
			}
			job.readyAt = time.Now().Add(time.Duration(delay) * time.Second)
			r.logLocked(fmt.Sprintf("链接 %d：%s", job.ID, job.LastError), "error")
		} else {
			job.Success++
			r.success++
			job.LastError = ""
			job.consecutiveErrors = 0
			job.readyAt = time.Time{}
		}
		r.mu.Unlock()
		if !waitContext(r.ctx, seconds(r.config.Interval)) {
			return
		}
	}
}
func (r *Run) readSize() (int, error) {
	if err := r.ctx.Err(); err != nil {
		return 0, err
	}
	rate := r.config.RateMiB * 1048576
	size := chunkSize
	if rate > 0 {
		size = int(rate / 10)
		if size < 1 {
			size = 1
		}
		if size > chunkSize {
			size = chunkSize
		}
		r.mu.Lock()
		scheduled := r.nextRead
		if now := time.Now(); scheduled.Before(now) {
			scheduled = now
		}
		r.nextRead = scheduled.Add(seconds(float64(size) / rate))
		r.mu.Unlock()
		if !waitContext(r.ctx, time.Until(scheduled)) {
			return 0, r.ctx.Err()
		}
	}
	return size, nil
}

func (r *Run) download(job *Job, buffer []byte) error {
	client := http.Client{Transport: r.transport, CheckRedirect: func(req *http.Request, via []*http.Request) error {
		if len(via) > 5 {
			return errors.New("重定向超过 5 次")
		}
		_, err := normalizeURL(req.URL.String())
		return err
	}}
	var peerMu sync.Mutex
	peer := ""
	trace := &httptrace.ClientTrace{GotConn: func(info httptrace.GotConnInfo) {
		peerMu.Lock()
		peer, _, _ = net.SplitHostPort(info.Conn.RemoteAddr().String())
		peerMu.Unlock()
	}}
	ctx := httptrace.WithClientTrace(r.ctx, trace)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, job.URL, nil)
	if err != nil {
		return err
	}
	var route downloadRoute
	if len(r.config.ServerIPs) > 0 {
		route.host, _ = hostKey(req.URL.Hostname())
		// Workers share one cursor for this run. Advance once per download,
		// including failed attempts; same-host redirects keep the selected IP.
		r.mu.Lock()
		route.ip = r.config.ServerIPs[r.nextIP]
		r.nextIP = (r.nextIP + 1) % len(r.config.ServerIPs)
		r.mu.Unlock()
		req = req.WithContext(context.WithValue(req.Context(), downloadRouteKey{}, route))
	}
	req.Header.Set("User-Agent", "FlowPanel/1.7")
	req.Header.Set("Accept-Encoding", "identity")
	req.Header.Set("Cache-Control", "no-cache")
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	host := resp.Request.URL.Hostname()
	key, _ := hostKey(host)
	overridden := route.ip != "" && key == route.host
	peerMu.Lock()
	ip := peer
	peerMu.Unlock()
	r.mu.Lock()
	job.ConnectionHost = host
	job.ServerIP = ip
	job.IPOverridden = overridden
	r.mu.Unlock()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return fmt.Errorf("HTTP %s", resp.Status)
	}
	for {
		size, err := r.readSize()
		if err != nil {
			return err
		}
		n, readErr := resp.Body.Read(buffer[:size])
		if n > 0 {
			r.mu.Lock()
			r.total += int64(n)
			job.Bytes += int64(n)
			reached := r.config.MaxGiB > 0 && float64(r.total) >= r.config.MaxGiB*1073741824
			r.mu.Unlock()
			if reached {
				r.stop("已达到累计流量上限")
			}
		}
		if readErr == io.EOF {
			return nil
		}
		if readErr != nil {
			return readErr
		}
	}
}

func (e *Engine) Snapshot() Snapshot {
	e.mu.Lock()
	r := e.current
	e.mu.Unlock()
	s := Snapshot{State: "idle", Reason: "等待创建任务", Jobs: []Job{}, History: []Point{}, Events: []Event{}}
	if r == nil {
		return s
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	if !r.ended.IsZero() {
		now = r.ended
	}
	elapsed := now.Sub(r.started).Seconds()
	s.State = r.state
	s.Reason = r.reason
	s.RunID = r.id
	s.Config = &r.config
	s.TotalBytes = r.total
	s.Elapsed = elapsed
	s.StartedAt = float64(r.started.UnixMilli()) / 1000
	if elapsed > 0 {
		s.AverageSpeed = float64(r.total) / elapsed
	}
	s.PeakSpeed = r.peak
	s.Success = r.success
	s.Failed = r.failed
	s.Cancelled = r.cancelled
	s.History = append(s.History, r.history...)
	start := len(r.events) - 50
	if start < 0 {
		start = 0
	}
	s.Events = append(s.Events, r.events[start:]...)
	if r.state == "running" || r.state == "stopping" {
		count := len(r.history)
		if count > 5 {
			count = 5
		}
		for _, point := range r.history[len(r.history)-count:] {
			s.Speed += point.Speed
		}
		if count > 0 {
			s.Speed /= float64(count)
		}
	}
	for _, job := range r.jobs {
		item := *job
		s.Active += item.Active
		switch {
		case item.Active > 0:
			item.Status = "running"
		case r.state == "completed" || r.state == "stopped":
			item.Status = r.state
			if item.Failed > 0 {
				item.Status = "failed"
			}
		case r.config.Repeat > 0 && item.Started >= r.config.Repeat:
			item.Status = "completed"
			if item.Failed > 0 {
				item.Status = "failed"
			}
		case item.readyAt.After(time.Now()):
			item.Status = "backoff"
		default:
			item.Status = "queued"
		}
		s.Jobs = append(s.Jobs, item)
	}
	return s
}
