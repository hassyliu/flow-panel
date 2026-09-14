# 构建

Go 1.26+，使用 go.mod / go.sum 锁定构建依赖。程序运行时不依赖 Go 安装。

在 Linux 上：

```sh
go test ./...
CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags='-s -w' -o flow-panel .
```

ARM64：将 GOARCH 改为 arm64。amd64 使用 Go 默认 v1 指令集。
网页由 `//go:embed static/*` 嵌入。构建时依赖 Go 官方 x/net 的 IDNA 库及 x509roots 的备用根证书，运行时无需下载依赖。

任务 API 为 `/api/state`、`/api/tasks` 和 `/api/tasks/{id}/start|stop` 等，不需要登录，详见 README。
