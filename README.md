# Flow Panel

局域网下载任务面板：每条链接是一个独立任务，可设置并发、次数、限速，并可指定服务器 IP。下载内容只经过内存，不写入磁盘。

用 Rust 编写，适合低配机器（例如 Celeron J1900）：

- 静态二进制约 3 MB，Docker 镜像基于 `scratch`，只包含这一个文件；
- 空闲时内存约 6 MB；256 个并发下载满载时约 15 MB；
- 每个连接使用固定 32 KiB 读缓冲，内存不会随文件大小增长；
- CPU 不支持 AES-NI 时（J1900 就是这样）自动优先协商 ChaCha20，降低 HTTPS 解密开销。

## Docker 部署

使用 Docker Hub 上的镜像（把 `<用户名>` 换成发布镜像的 Docker Hub 用户名）：

```sh
docker run -d --name flow-panel --restart unless-stopped \
  -p 8089:8089 -v flow-panel-data:/data <用户名>/flow-panel:latest
```

或者从源码构建：

```sh
docker compose up -d --build
```

浏览器访问 `http://服务器IP:8089`。任务配置保存在数据卷 `flow-panel-data` 的 `/data/tasks.json` 中。

不使用 compose 时：

```sh
docker build -t flow-panel .
docker run -d --name flow-panel --restart unless-stopped \
  -p 8089:8089 -v flow-panel-data:/data flow-panel
```

容器以非 root 用户（65534）运行。如果要把宿主机目录挂载到 `/data`，先执行 `chown 65534:65534 <目录>`。

修改端口时请用环境变量，这样健康检查会自动跟着改：

```sh
docker run -d -e FLOW_PANEL_PORT=9000 -p 9000:9000 -v flow-panel-data:/data flow-panel
```

## 参数

| 命令行 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `--host` | `FLOW_PANEL_HOST` | `0.0.0.0` | 监听地址 |
| `--port` | `FLOW_PANEL_PORT` | `8089` | 监听端口 |
| `--data-dir` | `FLOW_PANEL_DATA_DIR` | `./data`（镜像内为 `/data`） | 任务配置目录 |
| `--threads` | `FLOW_PANEL_THREADS` | CPU 核数，最多 4 | 工作线程数 |
| `--ca-file` | `FLOW_PANEL_CA_FILE` | 无 | 额外信任的 CA 证书（PEM），用于内网私有 CA |
| `--healthcheck` | | | 检查本机面板是否正常，用于容器健康检查 |

公共 CA 根证书已内置在程序中，不依赖系统证书。

## 使用

- **添加任务**：每行一个链接，每条链接创建一个独立任务，添加后立即开始下载。
- **全部开始 / 全部停止**：启动所有未运行的任务，或停止所有任务。
- **并发预算**：所有运行中任务的并发总和上限为 256；超出时任务显示“排队中”，按列表顺序依次启动，大任务不会被后来的小任务插队。
- **限速**：按每个任务所有并发的总速率计算。
- **流量上限 / 运行时长**：达到上限后任务自动停止。
- **指定服务器 IP**：每行一个 IP，每次下载按顺序轮流使用。链接里的域名、HTTP Host 和 HTTPS 证书校验保持不变；重定向到同一域名时继续使用这次选中的 IP，跳到其他域名时按正常 DNS 解析。
- **失败退避**：请求失败后，等待 1、2、4……最多 30 秒再重试。
- **重启恢复**：重启后，原本运行中或排队中的任务会自动恢复；手动停止的任务和已完成的有限次数任务保持停止。

### 导出 / 导入

“导出”会下载 `flow-panel-tasks-日期.json`，其中包含每个任务的名称和下载设置。“导入”有两种方式：

- **追加**：导入的任务加在现有任务之后；
- **替换**：先删除全部现有任务，要求所有任务都已停止。

导入的任务都处于停止状态，可点击“全部开始”启动。

## API

页面使用的都是 JSON 接口，也可以直接调用：

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/state` | 任务列表和实时网速 |
| POST | `/api/tasks` | 添加任务（`urls` 可以是多行文本或数组） |
| PUT | `/api/tasks/{id}` | 编辑已停止的任务 |
| DELETE | `/api/tasks/{id}` | 删除已停止的任务 |
| POST | `/api/tasks/{id}/start`、`/api/tasks/{id}/stop` | 启动 / 停止单个任务 |
| POST | `/api/tasks/start-all`、`/api/tasks/stop-all` | 全部开始 / 全部停止 |
| GET | `/api/export` | 导出配置 |
| POST | `/api/import?mode=append\|replace` | 导入配置（请求体为导出文件） |
| GET | `/healthz` | 健康检查 |

```sh
curl -X POST http://127.0.0.1:8089/api/tasks -H 'Content-Type: application/json' \
  -d '{"urls":"https://example.com/a.bin","concurrency":8,"repeat":0,"server_ips":"10.0.0.1\n10.0.0.2"}'
```

面板没有登录功能，只应在内网使用。来自其他网站页面的修改请求（跨站请求）会被拒绝，命令行工具和脚本不受影响。

## 发布到 Docker Hub

由 GitHub Actions（`.github/workflows/docker.yml`）自动完成：先运行测试，再构建 amd64 + arm64 镜像并推送。

1. 在 Docker Hub 的 Account settings → Personal access tokens 中，创建一个具有 Read & Write 权限的令牌。
2. 在 GitHub 仓库的 Settings → Secrets and variables → Actions 中，添加 `DOCKERHUB_USERNAME`（Docker Hub 用户名）和 `DOCKERHUB_TOKEN`（上一步的令牌）。
3. 推送版本标签，例如 `git tag v2.0.0 && git push origin v2.0.0`。镜像会发布为 `<用户名>/flow-panel:2.0.0`、`:2.0` 和 `:latest`。

也可以在 GitHub 的 Actions 页面手动运行，这种方式只更新 `:latest`。

## 开发

```sh
cargo test             # 单元测试和集成测试（含本地 HTTP / TLS 测试服务）
cargo run -- --port 8089
cargo build --release
```

Linux 上直接执行 `cargo build --release --target x86_64-unknown-linux-musl`，即可得到静态二进制。
