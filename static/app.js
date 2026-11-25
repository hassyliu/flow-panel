"use strict";

const $ = (id) => document.getElementById(id);
const STATE_NAMES = {
  queued: "排队中", idle: "未启动", running: "运行中",
  stopping: "停止中", stopped: "已停止", completed: "已完成",
};

let board = null;
let online = false;
let editingId = null;
let pollTimer = null;
let toastTimer = null;
const pending = new Set(); // task ids (or "all") with a request in flight
const rows = new Map();    // task id -> row elements

function formatRate(value) {
  if (!value || value < 1) return "0 B/s";
  const units = ["B/s", "KiB/s", "MiB/s", "GiB/s"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(value) / Math.log(1024)));
  return `${(value / 1024 ** i).toFixed(i ? 2 : 0)} ${units[i]}`;
}

function toast(message, isError = false) {
  const node = $("toast");
  node.textContent = message;
  node.className = "toast" + (isError ? " error" : "");
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { node.hidden = true; }, 4500);
}

async function api(path, method = "GET", body) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 15000);
  try {
    const options = { method, cache: "no-store", signal: controller.signal };
    if (body !== undefined) {
      options.headers = { "Content-Type": "application/json" };
      options.body = typeof body === "string" ? body : JSON.stringify(body);
    }
    const response = await fetch(path, options);
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || `请求失败（${response.status}）`);
    return data;
  } catch (error) {
    if (error.name === "AbortError") throw new Error("请求超时，请稍后查看任务状态");
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

function apply(data) {
  // Ignore responses older than what is already shown.
  if (board && board.instance === data.instance && data.version < board.version) return;
  board = data;
  online = true;
  render();
}

async function poll() {
  clearTimeout(pollTimer);
  try {
    apply(await api("/api/state"));
  } catch (_) {
    online = false;
    render();
  }
  pollTimer = setTimeout(poll, online ? 1000 : 3000);
}

async function act(key, path, method = "POST", body = {}) {
  if (pending.has(key)) return false;
  pending.add(key);
  render();
  try {
    apply(await api(path, method, body));
    return true;
  } catch (error) {
    toast(error.message, true);
    return false;
  } finally {
    pending.delete(key);
    render();
  }
}

function makeRow(id) {
  const tr = document.createElement("tr");
  const cell = (className) => {
    const td = document.createElement("td");
    if (className) td.className = className;
    tr.append(td);
    return td;
  };
  const div = (parent, className) => {
    const node = document.createElement("div");
    node.className = className;
    parent.append(node);
    return node;
  };
  const task = cell();
  const status = cell();
  const row = {
    tr,
    name: div(task, "task-name"),
    url: div(task, "task-url"),
    route: div(task, "task-route"),
    state: div(status, "state"),
    error: div(status, "task-error"),
    concurrency: cell("mono"),
    repeat: cell("mono"),
    speed: cell("num speed"),
    buttons: {},
  };
  const ops = cell("ops");
  for (const [action, label, className] of [
    ["start", "启动", "primary"], ["stop", "停止", ""], ["edit", "编辑", ""], ["delete", "删除", "danger"],
  ]) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.className = className;
    button.dataset.action = action;
    button.dataset.id = id;
    ops.append(button, " ");
    row.buttons[action] = button;
  }
  $("task-body").append(tr);
  rows.set(id, row);
  return row;
}

function routeText(task) {
  const ips = task.config.server_ips || [];
  if (task.server_ip) return `最近连接 ${task.server_ip}${task.ip_overridden ? "（指定 IP）" : ""}`;
  if (ips.length === 1) return `指定 IP ${ips[0]}`;
  if (ips.length > 1) return `${ips.length} 个 IP 轮询`;
  return "";
}

function render() {
  const tasks = board ? board.tasks : [];
  const query = $("search").value.trim().toLowerCase();
  const present = new Set(tasks.map((t) => t.id));
  for (const [id, row] of rows) {
    if (!present.has(id)) { row.tr.remove(); rows.delete(id); }
  }
  let visible = 0;
  for (const task of tasks) {
    const row = rows.get(task.id) || makeRow(task.id);
    const c = task.config;
    const locked = !online || pending.has(task.id) || pending.has("all");
    row.name.textContent = row.name.title = task.name;
    row.url.textContent = row.url.title = c.url;
    row.route.textContent = routeText(task);
    row.route.title = task.connection_host ? `${task.connection_host} → ${task.server_ip}` : "";
    row.route.hidden = !row.route.textContent;
    row.state.textContent = STATE_NAMES[task.state] || task.state;
    row.state.className = "state " + task.state;
    row.error.textContent = row.error.title = task.last_error || "";
    row.error.hidden = !task.last_error;
    row.concurrency.textContent = c.concurrency;
    row.repeat.textContent = c.repeat === 0 ? "∞" : c.repeat;
    row.speed.textContent = formatRate(task.speed);
    row.speed.classList.toggle("moving", task.speed > 0);
    row.buttons.start.hidden = task.busy;
    row.buttons.stop.hidden = !task.busy;
    row.buttons.start.disabled = locked;
    row.buttons.stop.disabled = locked || task.state === "stopping";
    row.buttons.edit.disabled = locked || task.busy;
    row.buttons.delete.disabled = locked || task.busy;
    row.tr.hidden = query !== "" && !`${task.name} ${c.url}`.toLowerCase().includes(query);
    if (!row.tr.hidden) visible++;
  }
  // Keep table order in sync with the server's list order.
  for (const task of tasks) $("task-body").append(rows.get(task.id).tr);

  $("empty").hidden = tasks.length > 0;
  $("no-results").hidden = tasks.length === 0 || visible > 0;
  $("connection").className = "connection " + (online ? "online" : "offline");
  $("connection").textContent = online ? "已连接" : "连接中断，正在重试";
  $("total-speed").textContent = formatRate(online && board ? board.speed : 0);
  $("version").textContent = board ? `v${board.app_version}` : "";
  $("counts").textContent = board
    ? `运行 ${board.running} · 排队 ${board.queued} · 并发 ${board.allocated}/${board.max_workers}`
    : "";
  const busyAll = !online || pending.has("all");
  $("start-all").disabled = busyAll || !tasks.some((t) => !t.busy);
  $("stop-all").disabled = busyAll || !tasks.some((t) => t.busy);
  $("add-task").disabled = !online;
  $("import").disabled = !online;
  $("export").disabled = !online || tasks.length === 0;
}

/* ---------- task form ---------- */

function setInfinite() {
  $("task-repeat").disabled = $("task-infinite").checked;
}

function openTaskForm(task = null) {
  editingId = task ? task.id : null;
  $("task-form").reset();
  $("form-error").textContent = "";
  $("form-title").textContent = task ? "编辑任务" : "添加任务";
  $("save-task").textContent = task ? "保存" : "添加";
  $("url-note").textContent = task ? "编辑时只能填写一条链接" : "每行一个，每条链接是一个独立任务";
  $("ip-section").open = false;
  $("advanced-section").open = false;
  if (task) {
    const c = task.config;
    $("task-urls").value = c.url;
    $("task-name").value = task.name;
    $("task-concurrency").value = c.concurrency;
    $("task-infinite").checked = c.repeat === 0;
    $("task-repeat").value = c.repeat || 1;
    $("task-ip").value = (c.server_ips || []).join("\n");
    $("task-rate").value = c.rate_mib;
    $("task-timeout").value = c.timeout;
    $("task-interval").value = c.interval;
    $("task-max").value = c.max_gib;
    $("task-duration").value = +(c.duration / 60).toFixed(4);
    $("ip-section").open = c.server_ips.length > 0;
    $("advanced-section").open = c.rate_mib > 0 || c.max_gib > 0 || c.duration > 0;
  }
  setInfinite();
  $("task-dialog").showModal();
  $("task-urls").focus();
}

async function submitTaskForm(event) {
  event.preventDefault();
  const urls = $("task-urls").value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
  if (editingId && urls.length !== 1) {
    $("form-error").textContent = "编辑时只能填写一条下载链接";
    return;
  }
  const number = (id) => Number($(id).value);
  const body = {
    urls,
    name: $("task-name").value,
    concurrency: number("task-concurrency"),
    repeat: $("task-infinite").checked ? 0 : number("task-repeat"),
    server_ips: $("task-ip").value,
    rate_mib: number("task-rate"),
    timeout: number("task-timeout"),
    interval: number("task-interval"),
    max_gib: number("task-max"),
    duration: Math.round(number("task-duration") * 60 * 1000) / 1000,
  };
  $("form-fields").disabled = true;
  $("save-task").disabled = true;
  $("form-error").textContent = "";
  try {
    const path = editingId ? `/api/tasks/${encodeURIComponent(editingId)}` : "/api/tasks";
    apply(await api(path, editingId ? "PUT" : "POST", body));
    $("task-dialog").close();
    toast(editingId ? "任务已保存" : `已添加 ${urls.length} 条任务，开始下载`);
  } catch (error) {
    $("form-error").textContent = error.message;
  } finally {
    $("form-fields").disabled = false;
    $("save-task").disabled = false;
    setInfinite();
  }
}

/* ---------- import ---------- */

function openImport() {
  $("import-form").reset();
  $("import-error").textContent = "";
  $("import-dialog").showModal();
}

async function submitImport(event) {
  event.preventDefault();
  const file = $("import-file").files[0];
  if (!file) return;
  const mode = document.querySelector('input[name="import-mode"]:checked').value;
  $("import-fields").disabled = true;
  $("import-submit").disabled = true;
  $("import-error").textContent = "";
  try {
    const text = await file.text();
    const before = board ? board.tasks.length : 0;
    const data = await api(`/api/import?mode=${mode}`, "POST", text);
    apply(data);
    $("import-dialog").close();
    const added = mode === "replace" ? data.tasks.length : data.tasks.length - before;
    toast(`已导入 ${added} 条任务`);
  } catch (error) {
    $("import-error").textContent = error.message;
  } finally {
    $("import-fields").disabled = false;
    $("import-submit").disabled = false;
  }
}

/* ---------- events ---------- */

$("task-body").addEventListener("click", (event) => {
  const button = event.target.closest("button[data-action]");
  if (!button || button.disabled) return;
  const id = button.dataset.id;
  const task = board && board.tasks.find((t) => t.id === id);
  if (!task) return;
  const path = `/api/tasks/${encodeURIComponent(id)}`;
  switch (button.dataset.action) {
    case "start": act(id, `${path}/start`); break;
    case "stop": act(id, `${path}/stop`); break;
    case "edit": openTaskForm(task); break;
    case "delete": act(id, path, "DELETE"); break;
  }
});

$("search").addEventListener("input", render);
$("add-task").addEventListener("click", () => openTaskForm());
$("start-all").addEventListener("click", () => act("all", "/api/tasks/start-all"));
$("stop-all").addEventListener("click", () => act("all", "/api/tasks/stop-all"));
$("export").addEventListener("click", () => { window.location.href = "/api/export"; });
$("import").addEventListener("click", openImport);
$("task-infinite").addEventListener("change", setInfinite);
$("task-form").addEventListener("submit", submitTaskForm);
$("import-form").addEventListener("submit", submitImport);
for (const button of document.querySelectorAll("dialog .cancel")) {
  button.addEventListener("click", () => button.closest("dialog").close());
}

poll();
