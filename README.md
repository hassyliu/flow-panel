# Flow Panel 1.7 · 独立下载任务

内网直接打开使用，无需登录或口令。网页已内置，不需要 Python、Docker 或 static 文件夹。

## 下载发布版

[最新 Release](https://github.com/hassyliu/flow-panel/releases/latest) · [v1.7 下载](https://github.com/hassyliu/flow-panel/releases/tag/v1.7)

- [Linux / Alpine x86_64 二进制](https://github.com/hassyliu/flow-panel/releases/download/v1.7/flow-panel-alpine-amd64)
- [Linux / Alpine ARM64 二进制](https://github.com/hassyliu/flow-panel/releases/download/v1.7/flow-panel-alpine-arm64)

下载裸二进制后重命名为 `flow-panel`，按下方步骤启动；也可从 Release 下载带说明的 `.tar.gz` 发布包。`SHA256SUMS.txt` 提供附件校验值。

## 启动

用 `uname -m` 查看架构：`x86_64` 选择 amd64，`aarch64` 选择 arm64。
解压对应发布包后：

```sh
chmod +x flow-panel
./flow-panel
```

浏览器访问 `http://服务器IP:8080`。默认监听 `0.0.0.0:8080`。

修改端口：

```sh
./flow-panel --port 9090
```

后台运行：

```sh
nohup ./flow-panel >/tmp/flow-panel.log 2>&1 &
```

按 Ctrl+C 可结束前台程序；后台程序可用 `kill` 结束对应进程。

## 下载

1. 点击“添加任务”，填写链接和下载参数。一次粘贴多行会创建多个独立任务，并立即自动下载。
2. 不同任务可同时运行；总并发已满时显示“排队中”，有空位后自动开始。
3. 点击对应行的“停止”，只停止该任务，其他任务继续下载。
4. 停止后可编辑、删除，或再次启动。“全部停止”会停止所有任务。

每条任务独立配置 1–64 并发、循环次数、限速、超时、重复间隔和服务器 IP 列表；所有运行任务总并发上限 256，最多 500 条任务。页面只显示任务状态和实时网速，不提供流量统计或图表。关闭网页不会中断任务；重启后自动恢复已开启的任务，手动停止和已完成的任务保持停止。
并发是多个完整下载请求，同一链接可同时下载多次；每条次数是请求尝试次数，失败也计一次。

## 配置保存

Linux 版固定将任务配置保存在 `/abc/tasks.json`，与启动时的工作目录无关。首次添加任务时自动创建 `/abc` 目录和文件；运行用户需要对该目录有写入权限。

添加、修改、删除、启停及自然完成时立即保存，重启或替换二进制后会恢复任务列表、名称、链接、多 IP 列表和下载参数；开启状态一起保存，重启后自动恢复运行或排队中的任务。手动停止、全部停止和自然完成的任务不会自动恢复。旧版重启前已经丢失的任务无法恢复。

只保存任务配置。下载响应正文仍只通过内存，不保存下载文件；网速刷新和循环中的每次请求不写入配置文件。保存使用同目录临时文件，写入完成后替换配置；失败会在网页提示并保留原有任务。配置损坏时程序报错退出，不覆盖原文件。

Docker 请使用 `-v /abc:/abc`，将宿主机的 `/abc` 映射到容器的 `/abc`；替换容器时继续使用同一目录。备份时复制 `/abc/tasks.json`；恢复配置应先停止面板。同一份配置文件供一个面板进程使用。

Windows 本机调试将配置放在可执行文件旁的 `abc/tasks.json`，避免写入系统根目录。

v1.6 配置格式版本为 2，使用 `enabled` 保存启停状态。v1.5 的旧格式没有这项信息，首次升级时会自动开启已有任务；之后手动停止的状态会正常保留。重启前尚未完成且仍开启的有限次数任务，恢复时从该任务设置的次数重新开始，不保存请求计数或部分下载进度；已经完成的任务不会自动重跑。

## 指定服务器 IP

链接填写原域名。展开“指定服务器 IP”，每行填写：

```text
192.168.1.10
192.168.1.20
2001:db8::10
```

只填 IP，不需要“域名=”，也不填写端口。支持 IPv4 / IPv6，空行忽略、重复 IP 自动去重。每次下载按填写顺序轮流使用 IP：第一个、第二个……最后一个之后回到第一个。每条任务独立轮询，并发请求共享该任务的轮询位置；实际到达服务器或下载完成的顺序可能因网络和并发调度不同而变化。停止后再次启动、重启程序恢复任务时，都从第一个 IP 开始，轮询位置只放在内存中。留空使用自动解析。

保留原链接的域名、端口、HTTP Host 和 HTTPS SNI / 证书校验。同一次下载中，原域名的重定向继续使用选中的 IP；跳转到其他域名时使用正常 DNS。选中 IP 失败时按已有下载失败及重试规则处理，下次请求继续使用列表中的下一个 IP。

批量添加时，每条任务各自复制这份 IP 列表，并自动从自己的链接读取域名；停止后可以单独编辑。列表中的“最近连接”是最近一次获得 HTTP 响应的连接 IP，并发下载时可能同时使用多个 IP。

## Alpine 开机启动（可选）

以 root 执行：

```sh
cp flow-panel /usr/local/bin/flow-panel
cp flow-panel.openrc /etc/init.d/flow-panel
chmod +x /usr/local/bin/flow-panel /etc/init.d/flow-panel
rc-update add flow-panel default
rc-service flow-panel start
```

停止：`rc-service flow-panel stop`。

## 程序说明

静态 Linux ELF 可执行文件，不依赖 glibc、musl 动态库或 Python。内置网页及备用根证书，最小 Alpine 环境也可发起 HTTPS 下载；系统已有根证书时优先使用系统证书。

数据以 64 KiB 分块读取后丢弃，内存占用不会随完整文件大小累计。网速为正在下载的响应正文读取速率，每秒刷新。

源码构建见 BUILDING.md，验证范围见 VALIDATION.md。

## 本机调试

在源码目录安装 Go 1.26+ 后运行：

```sh
go run . --host 127.0.0.1 --port 8080
```

打开 `http://127.0.0.1:8080`。网页内置在程序中，修改后需重新运行构建。

## 任务 API

- `GET /api/state`：任务列表和当前网速。
- `POST /api/tasks`：添加任务，`urls` 多行或数组会拆成独立任务并自动下载；总并发已满时排队。
- `POST /api/tasks/{id}/start`：启动指定任务。
- `POST /api/tasks/{id}/stop`：停止指定任务。
- `PUT /api/tasks/{id}`：修改已停止的任务，只允许一条 URL。
- `DELETE /api/tasks/{id}`：删除已停止的任务。
- `POST /api/tasks/stop-all`：停止全部任务。

添加和编辑接收 JSON：`name`（可选）、`urls`、`concurrency`、`repeat`（0 为无限）、`server_ips`（每行一个 IP 的文本或 IP 字符串数组）、`rate_mib`、`timeout`、`interval`。
v1.4 使用 `server_ips` 替代 `host_overrides`，旧字段会返回需刷新页面的提示。
v1.2 的批次 `/api/start` 和 `/api/stop` 已由任务 API 替代。

## 从源码构建 Docker 镜像

```sh
docker build -t flow-panel:1.7 .
docker run -d --name flow-panel --restart unless-stopped -p 8080:8080 -v /abc:/abc flow-panel:1.7
```

Release 中的 `flow-panel-docker.zip` 包含两个架构的预编译二进制，以及推送 Docker Hub 的说明，构建时不需要安装 Go。
