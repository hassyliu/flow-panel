# Flow Panel

局域网下载任务面板：每条链接是一个独立任务，可设置并发、次数、限速、运行时段，并可指定服务器 IP。下载内容只经过内存，**不写入磁盘**。

专为低配机器设计（例如 Celeron J1900）：

- 镜像只有一个约 3 MB 的静态二进制，基于 `scratch`，以非 root 用户运行
- 空闲时内存约 6 MB，256 个并发下载满载时约 15 MB
- 默认复用连接（Keep-Alive），下载小文件时可省掉大部分 TLS 握手开销
- CPU 没有 AES-NI 时自动优先协商 ChaCha20
- 支持 `linux/amd64` 和 `linux/arm64`

## 快速开始

```sh
docker run -d --name flow-panel --restart unless-stopped \
  -p 8089:8089 -v flow-panel-data:/data \
  用户名/flow-panel:latest
```

浏览器打开 `http://服务器IP:8089`。

### docker compose

```yaml
services:
  flow-panel:
    image: 用户名/flow-panel:latest
    container_name: flow-panel
    restart: unless-stopped
    ports:
      - "8089:8089"
    volumes:
      - flow-panel-data:/data
    environment:
      FLOW_PANEL_UTC_OFFSET: "8"   # 运行时段所用的时区，默认北京时间

volumes:
  flow-panel-data:
```

## 升级

```sh
docker pull 用户名/flow-panel:latest
docker rm -f flow-panel
# 再执行一次上面的 docker run
```

任务配置保存在数据卷 `/data/tasks.json` 中，升级后保留。

## 功能

- **独立任务**：每行一个链接，批量添加；每个任务可以单独启动、停止、编辑
- **全部开始 / 全部停止**
- **运行时段**：选择一天中哪些小时运行，到点自动开始，其他时间自动暂停
- **指定服务器 IP**：多个 IP 按顺序轮流使用，保留原域名的 Host 和 HTTPS 证书校验
- **并发、次数、限速、流量上限、运行时长**：每个任务分别设置；所有任务的总并发上限为 256，超出时按顺序排队
- **导出 / 导入配置**：备份任务，或迁移到另一台机器
- **重启恢复**：重启后，原本在运行的任务自动恢复，手动停止的任务保持停止
- **深色模式**：跟随系统自动切换

## 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `FLOW_PANEL_PORT` | `8089` | 监听端口（修改时同时修改 `-p` 映射） |
| `FLOW_PANEL_UTC_OFFSET` | `8` | 运行时段所用的时区（小时，例如 `8`、`-5`、`5.5`） |
| `FLOW_PANEL_THREADS` | CPU 核数，最多 4 | 工作线程数 |
| `FLOW_PANEL_CA_FILE` | 无 | 额外信任的 CA 证书（PEM），用于内网私有 CA |
| `FLOW_PANEL_HOST` | `0.0.0.0` | 监听地址 |

## 数据卷

- `/data`：任务配置。容器以 UID 65534 运行；挂载宿主机目录时，请先执行 `chown 65534:65534 <目录>`。

## 注意

面板没有登录功能，请只在内网使用，不要把端口暴露到公网。来自其他网站页面的修改请求（跨站请求）会被拒绝。
