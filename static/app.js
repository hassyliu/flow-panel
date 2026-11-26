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

/** [0,1,2,8,9] -> [[0,2],[8,9]] */
function hourRanges(hours) {
  const ranges = [];
  for (const h of [...hours].sort((a, b) => a - b)) {
    const last = ranges[ranges.length - 1];
    if (last && last[1] === h - 1) last[1] = h; else ranges.push([h, h]);
  }
  return ranges;
}

/** [8,9,10,20] -> "08:00–11:00, 20:00–21:00" */
function hoursText(hours) {
  if (!hours || hours.length === 0) return "未选择";
  if (hours.length === 24) return "全天";
  return hourRanges(hours).map(([a, b]) => `${pad(a)}:00–${pad(b + 1)}:00`).join(", ");
}

/** [0..7,22,23] -> "0-7, 22-23" (hour numbers, end included) */
function rangesText(hours) {
  return hourRanges(hours).map(([a, b]) => (a === b ? `${a}` : `${a}-${b}`)).join(", ");
}

/** "0-7, 22-23" or "22-6" (through midnight) -> hours, or null when invalid. */
function parseRanges(text) {
  const set = new Set();
  for (const part of text.split(/[,，、;；\s]+/).filter(Boolean)) {
    const match = /^(\d{1,2})(?:[-–~～至到](\d{1,2}))?$/.exec(part);
    if (!match) return null;
    const a = Number(match[1]);
    const b = match[2] === undefined ? a : Number(match[2]);
    if (a > 23 || b > 23) return null;
    for (let h = a; ; h = (h + 1) % 24) {
      set.add(h);
      if (h === b) break;
    }
  }
  return [...set].sort((x, y) => x - y);
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
    if (response.status === 401) {
      window.location.href = "/"; // signed out or the password changed: back to the login page
      throw new Error("请重新登录");
    }
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
  const global = board.schedule.enabled && c.hours.length === 0 ? board.schedule.hours : null;
  const key = JSON.stringify([chips, c.hours, global]);
  if (row.metaKey === key) return; // avoid rebuilding unchanged chips every second
  row.metaKey = key;
  row.meta.replaceChildren();
  for (const [text, title] of chips) {
    const chip = el("span", "chip", row.meta);
    chip.textContent = text;
    if (title) chip.title = title;
  }
  const hours = c.hours.length ? c.hours : global;
  if (hours && hours.length < 24) {
    const chip = el("span", global ? "chip hours follow" : "chip hours", row.meta);
    chip.title = global ? "跟随全局定时" : "这个任务单独设置的时段";
    const strip = el("span", "strip", chip);
    strip.style.background = hoursStrip(hours);
    chip.append((global ? "全局 " : "") + hoursText(hours));
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
  const schedule = board ? board.schedule : { enabled: false, hours: [] };
  $("schedule-toggle").checked = schedule.enabled;
  $("schedule-toggle").disabled = !online || pending.has("schedule");
  $("schedule-edit").textContent = schedule.enabled ? hoursText(schedule.hours) : "设置时段";
  $("schedule-edit").disabled = !online;
  $("no-password").hidden = !board || board.auth;
  $("logout").hidden = !board || !board.auth;
  $("add-task").disabled = !online;
  $("import").disabled = !online;
  $("export").disabled = !online || tasks.length === 0;
}

/* ---------- hour picker ---------- */

const PRESETS = [
  ["全天", () => [...Array(24).keys()]],
  ["白天 8–23", () => [...Array(16).keys()].map((h) => h + 8)],
  ["夜间 0–7", () => [...Array(8).keys()]],
  ["反选", (hours) => [...Array(24).keys()].filter((h) => !hours.includes(h))],
  ["清空", () => []],
];
const NIGHT = PRESETS[2][1];

/** 24 hour buttons (click, or press and drag to paint), a range box and presets. */
function hourPicker(root, onChange) {
  const grid = el("div", "hours", root);
  grid.setAttribute("aria-label", "选择运行的小时");
  const buttons = [];
  for (let h = 0; h < 24; h++) {
    const button = el("button", "", grid);
    button.type = "button";
    button.textContent = h;
    button.title = `${pad(h)}:00–${pad(h + 1)}:00`;
    buttons.push(button);
  }
  const tools = el("div", "picker-tools", root);
  const text = el("input", "", tools);
  text.placeholder = "输入范围，例如 0-7, 22-23";
  text.title = "小时范围，含首尾；22-6 表示晚上 10 点到第二天早上 7 点";
  text.setAttribute("aria-label", "按小时范围输入运行时段");

  const get = () => buttons.flatMap((b, h) => (b.classList.contains("on") ? [h] : []));
  const changed = () => {
    text.value = rangesText(get());
    text.classList.remove("bad");
    onChange();
  };
  const set = (hours) => {
    buttons.forEach((b, h) => b.classList.toggle("on", hours.includes(h)));
    changed();
  };
  for (const [label, pick] of PRESETS) {
    const button = el("button", "", tools);
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", () => set(pick(get())));
  }

  // Press on an hour and drag across others to paint them all the same way.
  // Every hour between the previous and the current position is painted, so a
  // quick drag that skips over buttons still selects a continuous range.
  let paint = null;
  let last = 0;
  grid.addEventListener("pointerdown", (event) => {
    const button = event.target.closest("button");
    if (!button || event.button !== 0) return;
    event.preventDefault();
    paint = !button.classList.contains("on");
    last = buttons.indexOf(button);
    button.classList.toggle("on", paint);
    changed();
  });
  grid.addEventListener("pointermove", (event) => {
    if (paint === null) return;
    const target = document.elementFromPoint(event.clientX, event.clientY);
    const hour = buttons.indexOf(target);
    if (hour < 0 || hour === last) return;
    const step = hour > last ? 1 : -1;
    for (let h = last + step; ; h += step) {
      buttons[h].classList.toggle("on", paint);
      if (h === hour) break;
    }
    last = hour;
    changed();
  });
  const end = () => { paint = null; };
  window.addEventListener("pointerup", end);
  window.addEventListener("pointercancel", end);
  // Enter / Space on a focused hour produces a click without a pointer press.
  grid.addEventListener("click", (event) => {
    const button = event.target.closest("button");
    if (!button || event.detail !== 0) return;
    button.classList.toggle("on");
    changed();
  });
  text.addEventListener("change", () => {
    const hours = parseRanges(text.value);
    if (hours === null) text.classList.add("bad"); else set(hours);
  });

  return { get, set, enable: (on) => root.classList.toggle("off", !on) };
}

/* ---------- global schedule ---------- */

const schedulePicker = hourPicker($("schedule-picker"), () => {});

function openSchedule(enable = null) {
  const schedule = board.schedule;
  $("schedule-error").textContent = "";
  $("schedule-zone").textContent = `服务器时间 ${board.clock} ${board.utc_offset}`;
  $("schedule-enabled").checked = enable ?? schedule.enabled;
  schedulePicker.set(schedule.hours.length ? schedule.hours : NIGHT());
  $("schedule-dialog").showModal();
}

async function submitSchedule(event) {
  event.preventDefault();
  const body = { enabled: $("schedule-enabled").checked, hours: schedulePicker.get() };
  if (body.enabled && body.hours.length === 0) {
    $("schedule-error").textContent = "请至少选择一个小时，或关闭全局定时";
    return;
  }
  $("schedule-fields").disabled = true;
  $("schedule-save").disabled = true;
  $("schedule-error").textContent = "";
  try {
    apply(await api("/api/schedule", "PUT", body));
    $("schedule-dialog").close();
    toast(body.enabled ? `全局定时已开启：${hoursText(body.hours)}` : "全局定时已关闭");
  } catch (error) {
    $("schedule-error").textContent = error.message;
  } finally {
    $("schedule-fields").disabled = false;
    $("schedule-save").disabled = false;
  }
}

function toggleSchedule() {
  const enabled = $("schedule-toggle").checked;
  const hours = board.schedule.hours;
  $("schedule-toggle").checked = board.schedule.enabled; // the server's reply decides
  if (enabled && hours.length === 0) openSchedule(true);
  else act("schedule", "/api/schedule", "PUT", { enabled, hours });
}

/* ---------- task form ---------- */

function hoursMode() {
  return document.querySelector('input[name="hours-mode"]:checked').value;
}

function updateHoursSummary() {
  const custom = hoursMode() === "custom";
  taskPicker.enable(custom);
  const schedule = board ? board.schedule : { enabled: false, hours: [] };
  const global = schedule.enabled ? hoursText(schedule.hours) : "未开启，全天运行";
  $("global-note").textContent = `当前：${global}`;
  $("hours-summary").textContent = custom ? hoursText(taskPicker.get()) : `跟随全局定时（${global}）`;
}

const taskPicker = hourPicker($("task-picker"), () => updateHoursSummary());

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
  for (const id of ["hours-section", "ip-section", "advanced-section"]) $(id).open = false;
  taskPicker.set(board && board.schedule.hours.length ? board.schedule.hours : NIGHT());
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
    if (c.hours.length) {
      document.querySelector('input[name="hours-mode"][value="custom"]').checked = true;
      taskPicker.set(c.hours);
    }
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
  const custom = hoursMode() === "custom";
  const hours = custom ? taskPicker.get() : [];
  if (custom && hours.length === 0) {
    $("form-error").textContent = "请至少选择一个运行的小时，或改为跟随全局定时";
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
for (const radio of document.querySelectorAll('input[name="hours-mode"]')) {
  radio.addEventListener("change", updateHoursSummary);
}
$("schedule-toggle").addEventListener("change", toggleSchedule);
$("schedule-edit").addEventListener("click", () => openSchedule());
$("schedule-form").addEventListener("submit", submitSchedule);
$("logout").addEventListener("click", async () => {
  await fetch("/api/logout", { method: "POST" }).catch(() => {});
  window.location.href = "/";
});
$("task-form").addEventListener("submit", submitTaskForm);
$("import-form").addEventListener("submit", submitImport);
for (const button of document.querySelectorAll("dialog .cancel")) {
  button.addEventListener("click", () => button.closest("dialog").close());
}

poll();
