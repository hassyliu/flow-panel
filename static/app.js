"use strict";

const $ = (id) => document.getElementById(id);
const STATE_NAMES = {
  queued: "排队中", waiting: "等待时段", idle: "未启动", running: "运行中",
  stopping: "停止中", stopped: "已停止", completed: "已完成",
};

let board = null;
let online = false;
let editingId = null;
let pollTimer = null;
let toastTimer = null;
const pending = new Set(); // task ids (or "all") with a request in flight
const rows = new Map();    // task id -> row elements

/* ---------- formatting ---------- */

function formatRate(value) {
  if (!value || value < 1) return "0 B/s";
  const units = ["B/s", "KiB/s", "MiB/s", "GiB/s"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(value) / Math.log(1024)));
  return `${(value / 1024 ** i).toFixed(i ? 2 : 0)} ${units[i]}`;
}

const pad = (n) => String(n).padStart(2, "0");

/** [8,9,10,20] -> "08:00–11:00, 20:00–21:00" */
function hoursText(hours) {
  if (!hours || hours.length === 0) return "全天";
  const ranges = [];
  for (const h of hours) {
    const last = ranges[ranges.length - 1];
    if (last && last[1] === h - 1) last[1] = h; else ranges.push([h, h]);
  }
  return ranges.map(([a, b]) => `${pad(a)}:00–${pad(b + 1)}:00`).join(", ");
}

/** 24-segment gradient: selected hours in the accent colour. */
function hoursStrip(hours) {
  const set = new Set(hours);
  const stops = [];
  for (let h = 0; h < 24; h++) {
    const color = set.has(h) ? "var(--accent)" : "var(--line)";
    stops.push(`${color} ${(h / 24 * 100).toFixed(2)}% ${((h + 1) / 24 * 100).toFixed(2)}%`);
  }
  return `linear-gradient(to right, ${stops.join(", ")})`;
}

function toast(message, isError = false) {
  const node = $("toast");
  node.textContent = message;
  node.className = "toast" + (isError ? " error" : "");
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { node.hidden = true; }, 4500);
}

/* ---------- API ---------- */

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
  if (pending.has(key)) return;
  pending.add(key);
  render();
  try {
    apply(await api(path, method, body));
  } catch (error) {
    toast(error.message, true);
  } finally {
    pending.delete(key);
    render();
  }
}

/* ---------- task list ---------- */

function el(tag, className, parent) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (parent) parent.append(node);
  return node;
}

function makeRow(id) {
  const tr = el("tr");
  const task = el("td", "", tr);
  const status = el("td", "", tr);
  const row = {
    tr,
    name: el("div", "name", task),
    url: el("div", "url", task),
    meta: el("div", "meta", task),
    state: el("span", "state", status),
    error: el("div", "task-error", status),
    concurrency: el("td", "c mono", tr),
    repeat: el("td", "c mono", tr),
    buttons: {},
  };
  const speedCell = el("td", "r", tr);
  row.speed = el("div", "speed", speedCell);
  row.bar = el("i", "", el("div", "bar", speedCell));
  const ops = el("td", "r ops", tr);
  for (const [action, label, className] of [
    ["start", "启动", "primary"], ["stop", "停止", ""], ["edit", "编辑", "ghost"], ["delete", "删除", "ghost danger"],
  ]) {
    const button = el("button", className, ops);
    button.type = "button";
    button.textContent = label;
    button.dataset.action = action;
    button.dataset.id = id;
    row.buttons[action] = button;
  }
  rows.set(id, row);
  return row;
}

function renderMeta(row, task) {
  const c = task.config;
  const chips = [];
  const ips = c.server_ips || [];
  if (task.server_ip) chips.push([`${task.ip_overridden ? "指定 " : ""}${task.server_ip}`, task.connection_host]);
  else if (ips.length) chips.push([ips.length === 1 ? `指定 ${ips[0]}` : `${ips.length} 个 IP 轮询`, ""]);
  if (task.tls) chips.push([task.tls, "协商到的 TLS 版本和加密套件"]);
  if (!c.keep_alive) chips.push(["不复用连接", ""]);
  const key = JSON.stringify([chips, c.hours]);
  if (row.metaKey === key) return; // avoid rebuilding unchanged chips every second
  row.metaKey = key;
  row.meta.replaceChildren();
  for (const [text, title] of chips) {
    const chip = el("span", "chip", row.meta);
    chip.textContent = text;
    if (title) chip.title = title;
  }
  if (c.hours && c.hours.length) {
    const chip = el("span", "chip hours", row.meta);
    const strip = el("span", "strip", chip);
    strip.style.background = hoursStrip(c.hours);
    chip.append(hoursText(c.hours));
  }
  row.meta.hidden = row.meta.childElementCount === 0;
}

function render() {
  const tasks = board ? board.tasks : [];
  const query = $("search").value.trim().toLowerCase();
  const present = new Set(tasks.map((t) => t.id));
  for (const [id, row] of rows) {
    if (!present.has(id)) { row.tr.remove(); rows.delete(id); }
  }
  const maxSpeed = Math.max(1, ...tasks.map((t) => t.speed));
  const body = $("task-body");
  let visible = 0;
  for (const task of tasks) {
    const row = rows.get(task.id) || makeRow(task.id);
    const c = task.config;
    const locked = !online || pending.has(task.id) || pending.has("all");
    row.name.textContent = row.name.title = task.name;
    row.url.textContent = row.url.title = c.url;
    renderMeta(row, task);
    row.state.textContent = STATE_NAMES[task.state] || task.state;
    row.state.className = "state " + task.state;
    row.error.textContent = row.error.title = task.last_error || "";
    row.error.hidden = !task.last_error;
    row.concurrency.textContent = c.concurrency;
    row.repeat.textContent = c.repeat === 0 ? "∞" : c.repeat;
    row.speed.textContent = formatRate(task.speed);
    row.speed.classList.toggle("moving", task.speed > 0);
    row.bar.style.width = `${Math.min(100, task.speed / maxSpeed * 100)}%`;
    row.buttons.start.hidden = task.busy;
    row.buttons.stop.hidden = !task.busy;
    row.buttons.start.disabled = locked;
    row.buttons.stop.disabled = locked || task.state === "stopping";
    row.buttons.edit.disabled = locked || task.busy;
    row.buttons.delete.disabled = locked || task.busy;
    row.tr.hidden = query !== "" && !`${task.name} ${c.url}`.toLowerCase().includes(query);
    if (!row.tr.hidden) visible++;
    body.append(row.tr); // keeps DOM order in sync with the server's order
  }

  $("empty").hidden = tasks.length > 0;
  $("no-results").hidden = tasks.length === 0 || visible > 0;
  $("connection").className = "pill " + (online ? "online" : "offline");
  $("connection").textContent = online ? "已连接" : "连接中断，重试中";
  $("version").textContent = board ? `v${board.app_version}` : "";
  $("clock").textContent = board ? `${board.clock} ${board.utc_offset}` : "";
  $("total-speed").textContent = formatRate(online && board ? board.speed : 0);
  $("stat-running").textContent = board ? board.running : 0;
  $("stat-total").textContent = board ? `/ ${tasks.length} 条` : "";
  $("stat-waiting").textContent = board ? `${board.queued} / ${board.waiting}` : "0 / 0";
  $("stat-workers").textContent = board ? `${board.allocated} / ${board.max_workers}` : "0 / 256";
  $("worker-meter").style.width = board ? `${board.allocated / board.max_workers * 100}%` : "0";

  const busyAll = !online || pending.has("all");
  $("start-all").disabled = busyAll || !tasks.some((t) => !t.busy);
  $("stop-all").disabled = busyAll || !tasks.some((t) => t.busy);
  $("add-task").disabled = !online;
  $("import").disabled = !online;
  $("export").disabled = !online || tasks.length === 0;
}

/* ---------- task form ---------- */

const hourButtons = [];
for (let h = 0; h < 24; h++) {
  const button = el("button", "", $("hours-grid"));
  button.type = "button";
  button.textContent = h;
  button.title = `${pad(h)}:00–${pad(h + 1)}:00`;
  button.addEventListener("click", () => { button.classList.toggle("on"); updateHoursSummary(); });
  hourButtons.push(button);
}

function selectedHours() {
  return hourButtons.flatMap((b, h) => (b.classList.contains("on") ? [h] : []));
}

function setHours(hours) {
  hourButtons.forEach((b, h) => b.classList.toggle("on", hours.includes(h)));
  updateHoursSummary();
}

function updateHoursSummary() {
  const limited = $("hours-limit").checked;
  $("hours-grid").classList.toggle("off", !limited);
  const hours = selectedHours();
  $("hours-summary").textContent = !limited ? "全天" : hours.length ? hoursText(hours) : "未选择";
}

const PRESETS = {
  all: [...Array(24).keys()],
  none: [],
  day: [...Array(16).keys()].map((h) => h + 8),
  night: [...Array(8).keys()],
};
for (const button of document.querySelectorAll("[data-hours]")) {
  button.addEventListener("click", () => {
    $("hours-limit").checked = true;
    setHours(PRESETS[button.dataset.hours]);
  });
}

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
  $("hours-zone").textContent = board ? `（服务器时间 ${board.utc_offset}）` : "";
  for (const id of ["hours-section", "ip-section", "advanced-section"]) $(id).open = false;
  setHours([]);
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
    $("task-keepalive").checked = c.keep_alive;
    $("hours-limit").checked = c.hours.length > 0;
    setHours(c.hours);
    $("hours-section").open = c.hours.length > 0;
    $("ip-section").open = c.server_ips.length > 0;
    $("advanced-section").open = c.rate_mib > 0 || c.max_gib > 0 || c.duration > 0 || c.interval > 0 || !c.keep_alive;
  }
  updateHoursSummary();
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
  const hours = $("hours-limit").checked ? selectedHours() : [];
  if ($("hours-limit").checked && hours.length === 0) {
    $("form-error").textContent = "请至少选择一个运行的小时，或取消“只在选中的小时运行”";
    $("hours-section").open = true;
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
    keep_alive: $("task-keepalive").checked,
    hours,
  };
  $("form-fields").disabled = true;
  $("save-task").disabled = true;
  $("form-error").textContent = "";
  try {
    const path = editingId ? `/api/tasks/${encodeURIComponent(editingId)}` : "/api/tasks";
    apply(await api(path, editingId ? "PUT" : "POST", body));
    $("task-dialog").close();
    toast(editingId ? "任务已保存" : `已添加 ${urls.length} 条任务`);
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
    const before = board ? board.tasks.length : 0;
    const data = await api(`/api/import?mode=${mode}`, "POST", await file.text());
    apply(data);
    $("import-dialog").close();
    toast(`已导入 ${mode === "replace" ? data.tasks.length : data.tasks.length - before} 条任务`);
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
$("hours-limit").addEventListener("change", updateHoursSummary);
$("task-form").addEventListener("submit", submitTaskForm);
$("import-form").addEventListener("submit", submitImport);
for (const button of document.querySelectorAll("dialog .cancel")) {
  button.addEventListener("click", () => button.closest("dialog").close());
}

poll();
