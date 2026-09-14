# Flow Panel v1.7

基于 Go 的 Linux / Alpine 下载面板，网页内置，下载内容在内存中读取后丢弃。

- 每条链接一个独立任务，支持并发、批量、循环下载及单独启停。
- 多 IP 每行一个，按填写顺序轮询；保留原域名、端口及 HTTPS SNI。
- 新增任务自动下载；重启自动恢复已开启任务，手动停止和已完成的任务保持停止。
- 任务配置固定保存在 `/abc/tasks.json`；Docker 使用 `-v /abc:/abc`。
- 页面只显示任务状态和实时网速，不保存下载文件。

## 下载与运行

`x86_64` 使用 `flow-panel-alpine-amd64`，`aarch64` 使用 `flow-panel-alpine-arm64`。

```sh
chmod +x flow-panel-alpine-amd64
./flow-panel-alpine-amd64
```

浏览器访问 `http://服务器IP:8080`。裸二进制不需要其他运行文件；`.tar.gz` 附有启动说明和 OpenRC 服务文件。`flow-panel-docker.zip` 提供双架构 Docker 构建材料。

## 验证

39 项 Go 测试通过。两个架构均通过静态 ELF 检查，不依赖 glibc / musl 动态库。当前验证环境为 Windows，Linux / Alpine 实机运行及 Docker 构建尚未验证。附件校验值见 `SHA256SUMS.txt`。
