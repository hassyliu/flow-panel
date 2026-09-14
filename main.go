package main

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"time"

	_ "golang.org/x/crypto/x509roots/fallback"
)

//go:embed static/*
var assets embed.FS

func jsonResponse(w http.ResponseWriter, code int, value any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(value)
}
func panelHandler(manager *TaskManager) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodGet {
			switch r.URL.Path {
			case "/healthz":
				jsonResponse(w, 200, map[string]bool{"ok": true})
				return
			case "/api/state":
				jsonResponse(w, 200, manager.Board())
				return
			}
			files := map[string][2]string{"/": {"index.html", "text/html; charset=utf-8"}, "/app.js": {"app.js", "text/javascript; charset=utf-8"}, "/style.css": {"style.css", "text/css; charset=utf-8"}, "/favicon.svg": {"favicon.svg", "image/svg+xml"}}
			if file, ok := files[r.URL.Path]; ok {
				body, err := assets.ReadFile("static/" + file[0])
				if err != nil {
					http.Error(w, "页面读取失败", 500)
					return
				}
				w.Header().Set("Content-Type", file[1])
				w.Header().Set("Cache-Control", "no-store")
				_, _ = w.Write(body)
				return
			}
		}
		if r.Method == http.MethodPost || r.Method == http.MethodPut || r.Method == http.MethodDelete {
			body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 1024*1024))
			if err != nil {
				jsonResponse(w, 400, map[string]string{"error": "请求体过大或读取失败"})
				return
			}
			var actionErr error
			if (r.Method == http.MethodPost && r.URL.Path == "/api/tasks") || (r.Method == http.MethodPut && strings.HasPrefix(r.URL.Path, "/api/tasks/")) {
				config, err := parseConfig(body)
				if err != nil {
					jsonResponse(w, 400, map[string]string{"error": err.Error()})
					return
				}
				name, err := taskName(body)
				if err != nil {
					jsonResponse(w, 400, map[string]string{"error": err.Error()})
					return
				}
				if r.Method == http.MethodPost {
					_, actionErr = manager.AddAndStart(config, name)
				} else {
					actionErr = manager.Update(strings.TrimPrefix(r.URL.Path, "/api/tasks/"), config, name)
				}
			} else if r.Method == http.MethodPost && r.URL.Path == "/api/tasks/stop-all" {
				actionErr = manager.StopAll()
			} else if strings.HasPrefix(r.URL.Path, "/api/tasks/") {
				parts := strings.Split(strings.TrimPrefix(r.URL.Path, "/api/tasks/"), "/")
				if r.Method == http.MethodDelete && len(parts) == 1 {
					actionErr = manager.Delete(parts[0])
				} else if r.Method == http.MethodPost && len(parts) == 2 && parts[1] == "start" {
					actionErr = manager.Start(parts[0])
				} else if r.Method == http.MethodPost && len(parts) == 2 && parts[1] == "stop" {
					actionErr = manager.Stop(parts[0])
				} else {
					actionErr = errTaskMissing
				}
			} else {
				jsonResponse(w, 404, map[string]string{"error": "接口不存在，请使用 /api/tasks"})
				return
			}
			if actionErr != nil {
				code := http.StatusConflict
				if errors.Is(actionErr, errTaskMissing) {
					code = http.StatusNotFound
				} else if errors.Is(actionErr, errTaskStorage) {
					code = http.StatusInternalServerError
				}
				jsonResponse(w, code, map[string]string{"error": actionErr.Error()})
				return
			}
			jsonResponse(w, 200, manager.Board())
			return
		}
		jsonResponse(w, 404, map[string]string{"error": "页面不存在"})
	})
}
func main() {
	host := flag.String("host", "0.0.0.0", "监听地址")
	port := flag.Int("port", 8080, "监听端口")
	version := flag.Bool("version", false, "显示版本")
	flag.Parse()
	if *version {
		fmt.Printf("Flow Panel 1.7 · %s/%s\n", runtime.GOOS, runtime.GOARCH)
		return
	}
	if *port < 0 || *port > 65535 {
		log.Fatal("端口范围为 0–65535")
	}
	dataFile := "/abc/tasks.json"
	if runtime.GOOS == "windows" {
		// Local Windows debugging uses an abc folder beside the executable.
		executable, err := os.Executable()
		if err != nil {
			log.Fatal(err)
		}
		dataFile = filepath.Join(filepath.Dir(executable), "abc", "tasks.json")
	}
	manager, err := loadTaskManager(dataFile)
	if err != nil {
		log.Fatal(err)
	}
	listener, err := net.Listen("tcp", net.JoinHostPort(*host, strconv.Itoa(*port)))
	if err != nil {
		log.Fatal(err)
	}
	actualPort := listener.Addr().(*net.TCPAddr).Port
	manager.Resume()
	server := &http.Server{Handler: panelHandler(manager), ReadHeaderTimeout: 10 * time.Second, ReadTimeout: 15 * time.Second, WriteTimeout: 15 * time.Second, IdleTimeout: 30 * time.Second}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	fmt.Printf("Flow Panel 已启动：http://%s:%d\n内网访问：http://服务器IP:%d\n", *host, actualPort, actualPort)
	fmt.Printf("任务配置：%s；已恢复 %d 条任务，已开启的任务自动下载\n", manager.dataFile, len(manager.tasks))
	finished := make(chan error, 1)
	go func() { finished <- server.Serve(listener) }()
	select {
	case <-ctx.Done():
	case err := <-finished:
		if err != http.ErrServerClosed {
			log.Printf("服务结束：%v", err)
		}
	}
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	manager.Close(shutdownCtx)
	_ = server.Shutdown(shutdownCtx)
}
