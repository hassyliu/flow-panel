# Flow Panel

下载任务面板：每条链接是一个独立任务，可设置并发、次数、限速、运行时段，并可指定服务器 IP。下载内容只经过内存，不写入磁盘。

用 Rust 编写，适合低配机器（例如 Celeron J1900）：

- 静态二进制约 3 MB，Docker 镜像基于 `scratch`，只包含这一个文件；
- 空闲时内存约 6 MB；256 个并发下载满载时约 15 MB；
- 每个连接使用固定 32 KiB 读缓冲，内存不会随文件大小增长；
- CPU 不支持 AES-NI 时（J1900 就是这样）自动优先协商 ChaCha20，降低 HTTPS 解密开销。

## Docker 部署

使用 Docker Hub 上的镜像（把 `<用户名>` 换成发布镜像的 Docker Hub 用户名）：

```sh
docker run -d --name flow-panel --restart unless-stopped \
  -p 8089:8089 -v flow-panel-data:/data \
  -e FLOW_PANEL_PASSWORD='换成你的密码' \
  <用户名>/flow-panel:latest
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
| `--utc-offset` | `FLOW_PANEL_UTC_OFFSET` | `8` | 运行时段所用的时区（小时，例如 `8`、`-5`、`5.5`） |
| `--password` | `FLOW_PANEL_PASSWORD` | 无 | 访问密码；不设置则无需登录 |
| `--healthcheck` | | | 检查本机面板是否正常，用于容器健康检查 |

公共 CA 根证书已内置在程序中，不依赖系统证书。

## 使用

- **添加任务**：每行一个链接，每条链接创建一个独立任务，添加后立即开始下载。
- **全部开始 / 全部停止**：启动所有未运行的任务，或停止所有任务。
- **并发预算**：所有运行中任务的并发总和上限为 256；超出时任务显示“排队中”，按列表顺序依次启动，大任务不会被后来的小任务插队。
- **全局定时**：工具栏上的“全局定时”开关。开启后，没有单独设置时段的任务只在选中的小时运行：到点自动开始，其他时间自动暂停（显示“等待时段”）；关闭后这些任务全天运行。点开关旁边的时段文字可以修改时段。
- **单独设置时段**：在任务的“运行时段”里选择“单独设置这个任务的时段”，这个任务就按自己的时段运行，不受全局定时影响；选“全天”表示始终运行。
- **选择小时**：点一下切换一个小时；按住鼠标拖动可以连续选择；也可以直接输入范围，例如 `0-7, 22-23`，或 `22-6`（跨过午夜）。手动停止的任务不会被定时启动。时间按 `--utc-offset` 设置的时区计算（默认北京时间），页面右上角显示当前的服务器时间。
- **复用连接**：默认开启（HTTP Keep-Alive）。多次下载复用同一条连接，省去重复的 TCP 和 TLS 握手，下载小文件时 CPU 开销能降到约四分之一。关闭后每次下载都新建连接。
- **限速**：按每个任务所有并发的总速率计算。
- **流量上限 / 运行时长**：达到上限后任务自动停止。
- **指定服务器 IP**：每行一个 IP，每次下载按顺序轮流使用。链接里的域名、HTTP Host 和 HTTPS 证书校验保持不变；重定向到同一域名时继续使用这次选中的 IP，跳到其他域名时按正常 DNS 解析。
- **重复间隔**：默认 0，即每次下载完立刻开始下一次。
- **失败退避**：请求失败后，等待 1、2、4……最多 30 秒再重试。
- **重启恢复**：重启后，原本运行中或排队中的任务会自动恢复；手动停止的任务和已完成的有限次数任务保持停止。

### 导出 / 导入

“导出”会下载 `flow-panel-tasks-日期.json`，其中包含全局定时，以及每个任务的名称和下载设置。“导入”有两种方式：

- **追加**：导入的任务加在现有任务之后，全局定时不变；
- **替换**：先删除全部现有任务（要求所有任务都已停止），全局定时也换成文件里的设置。

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
| PUT | `/api/schedule` | 设置全局定时，例如 `{"enabled":true,"hours":[0,1,2,3]}` |
| GET | `/api/export` | 导出配置 |
| POST | `/api/import?mode=append\|replace` | 导入配置（请求体为导出文件） |
| POST | `/api/login`、`/api/logout` | 登录（`{"password":"..."}`）/ 退出 |
| GET | `/healthz` | 健康检查（无需密码） |

设置了访问密码时，脚本用 `Authorization: Bearer <密码>` 调用：

```sh
curl -X POST http://127.0.0.1:8089/api/tasks -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer 你的密码' \
  -d '{"urls":"https://example.com/a.bin","concurrency":8,"repeat":0,"server_ips":"10.0.0.1\n10.0.0.2"}'
```

## 访问密码

设置 `FLOW_PANEL_PASSWORD`（或 `--password`）后，打开面板需要先输入密码，登录状态保留 30 天，右上角可以“退出”。

- 修改密码后重启，所有已登录的浏览器都需要重新登录。
- 同一个 IP 连续输错 5 次密码，15 分钟内不能再试。
- 会话密钥保存在数据目录的 `secret.key`；删除它再重启，也会让所有人重新登录。
- 不设置密码时，页面右上角会提示“未设置访问密码”，任何能访问端口的人都能操作面板。

**放到公网时**：密码在 HTTP 上是明文传输的，同一网络里的人有可能截获。建议用 Caddy、Nginx 等反向代理加上 HTTPS（代理需要转发 `X-Forwarded-Proto: https`，面板会给登录 Cookie 加上 `Secure`）。另外，面板会让服务器向你填写的链接发起下载，请使用足够长、不易猜到的密码。

来自其他网站页面的修改请求（跨站请求）会被拒绝，命令行工具和脚本不受影响。

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
