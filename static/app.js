"use strict";
const $ = id => document.getElementById(id);
const names = {queued:"排队中",idle:"未启动",running:"运行中",stopping:"停止中",stopped:"已停止",completed:"已完成"};
let board = null, online = false, editing = null, formBusy = false, fetching = false, timer, toastTimer;
const pending = new Set(), rows = new Map();
function rate(value) { if (!value || value < 1) return "0 B/s"; const units = ["B/s","KiB/s","MiB/s","GiB/s"]; const i=Math.min(3,Math.floor(Math.log(value)/Math.log(1024))); return `${(value/1024**i).toFixed(i?2:0)} ${units[i]}`; }
function toast(message,error=false){$("toast").textContent=message;$("toast").className="toast"+(error?" error":"");$("toast").hidden=false;clearTimeout(toastTimer);toastTimer=setTimeout(()=>{$("toast").hidden=true;},4500);}
async function api(path,method="GET",data){const controller=new AbortController();const timeout=setTimeout(()=>controller.abort(),12000);try{const response=await fetch(path,{method,cache:"no-store",signal:controller.signal,...(data===undefined?{}:{headers:{"Content-Type":"application/json"},body:JSON.stringify(data)})});const body=await response.json();if(!response.ok)throw new Error(body.error||`请求失败 (${response.status})`);return body;}finally{clearTimeout(timeout);}}
function setConnection(connected){online=connected;$("connection").className="connection "+(connected?"online":"offline");$("connection").textContent=connected?"已连接":"连接中断，正在重试";refresh();}
function apply(data){if(board&&board.instance===data.instance&&data.version<board.version)return;board=data;setConnection(true);}
function cell(cls){const node=document.createElement("td");if(cls)node.className=cls;return node;}
function makeRow(task){
  const tr=document.createElement("tr");tr.dataset.taskId=task.id;
  const identity=cell("identity"),name=document.createElement("div"),url=document.createElement("div"),route=document.createElement("div");
  name.className="task-name";url.className="task-url";route.className="task-route";identity.append(name,url,route);
  const statusCell=cell(),state=document.createElement("span"),error=document.createElement("div");state.className="state";error.className="task-error";statusCell.append(state,error);
  const concurrency=cell("mono"),mode=cell("mono"),speed=cell("speed"),actionsCell=cell(),actions=document.createElement("div");actions.className="row-actions";
  const buttons={};for(const [action,label,cls] of [["start","启动","start-button"],["stop","停止","stop-button"],["edit","编辑","plain-button"],["delete","删除","plain-button"]]){const button=document.createElement("button");button.type="button";button.textContent=label;button.className=cls;button.dataset.action=action;button.dataset.id=task.id;actions.append(button);buttons[action]=button;}
  actionsCell.append(actions);tr.append(identity,statusCell,concurrency,mode,speed,actionsCell);$("task-body").append(tr);
  const row={tr,name,url,route,state,error,concurrency,mode,speed,buttons};rows.set(task.id,row);return row;
}
function refresh(){
  const tasks=board?.tasks||[],present=new Set(tasks.map(t=>t.id)),query=$("search").value.trim().toLowerCase();let visible=0;
  for(const [id,row] of rows){if(!present.has(id)){row.tr.remove();rows.delete(id);}}
  for(const task of tasks){
    const row=rows.get(task.id)||makeRow(task),locked=pending.has(task.id)||pending.has("all"),c=task.config;
    row.name.textContent=task.name;row.name.title=task.name;row.url.textContent=c.urls[0];row.url.title=c.urls[0];
    const ipCount=(c.server_ips||[]).length;
    row.route.textContent=task.server_ip?`最近连接 · ${task.server_ip}${task.ip_overridden?" · 指定 IP":""}`:(ipCount>1?`${ipCount} 个 IP · 按顺序轮询`:ipCount===1?`指定 IP · ${c.server_ips[0]}`:"");row.route.hidden=!row.route.textContent;
    row.route.title=task.connection_host?`${task.connection_host} → ${task.server_ip}`:"";
    row.state.textContent=names[task.state]||task.state;row.state.className="state "+task.state;
    row.error.textContent=task.last_error||"";row.error.title=task.last_error||"";row.error.hidden=!task.last_error;
    row.concurrency.textContent=String(c.concurrency);row.mode.textContent=c.repeat===0?"∞ 循环":`${c.repeat} 次`;
    row.speed.textContent=rate(task.speed);row.speed.className="speed"+(task.speed>0?" moving":"");
    row.buttons.start.disabled=!online||locked||task.busy;row.buttons.stop.disabled=!online||locked||!task.busy||task.state==="stopping";
    row.buttons.start.hidden=task.busy;row.buttons.stop.hidden=!task.busy;
    row.buttons.edit.disabled=!online||locked||task.busy;row.buttons.delete.disabled=!online||locked||task.busy;
    row.tr.hidden=!!query&&!`${task.name} ${c.urls[0]}`.toLowerCase().includes(query);if(!row.tr.hidden)visible++;
  }
  document.querySelector(".task-panel").classList.toggle("empty-panel",tasks.length===0);$("empty").hidden=tasks.length>0;$("no-results").hidden=tasks.length===0||visible>0;
  $("total-speed").textContent=rate(online?board?.speed:0);$("stop-all").disabled=!online||!tasks.some(t=>t.busy)||pending.has("all");
  $("add-task").disabled=!online;$("first-task").disabled=!online;
}
async function poll(){if(fetching)return;fetching=true;clearTimeout(timer);try{apply(await api("/api/state"));}catch(_){setConnection(false);}finally{fetching=false;timer=setTimeout(poll,online?1000:2500);}}
function closeDialog(){if(!formBusy)$("task-dialog").close();}
function modeChanged(){$("task-repeat").disabled=$("task-infinite").checked;}
function openForm(task=null){
  editing=task?.id||null;$("task-form").reset();$("form-error").textContent="";$("ip-section").open=false;$("advanced-section").open=false;
  $("form-title").textContent=task?"编辑下载任务":"添加下载任务";$("save-task").textContent=task?"保存修改":"添加任务";
  $("url-note").textContent=task?"每个任务只对应一条链接。":"每行一个链接，每条链接创建独立任务。";
  if(task){const c=task.config;$("task-urls").value=c.urls[0];$("task-name").value=task.name;$("task-concurrency").value=c.concurrency;$("task-infinite").checked=c.repeat===0;$("task-repeat").value=c.repeat||1;$("task-rate").value=c.rate_mib;$("task-timeout").value=c.timeout;$("task-interval").value=c.interval;$("task-ip").value=(c.server_ips||[]).join("\n");$("ip-section").open=!!$("task-ip").value;}
  modeChanged();$("task-dialog").showModal();$("task-urls").focus();
}
async function taskAction(id,action){
  const task=board?.tasks.find(t=>t.id===id);if(!task||pending.has(id))return;
  if(action==="edit"){openForm(task);return;}
  pending.add(id);refresh();
  try{apply(await api(`/api/tasks/${id}${action==="delete"?"":"/"+action}`,action==="delete"?"DELETE":"POST",{}));}
  catch(error){toast(error.name==="AbortError"?"请求超时，正在查询任务状态":error.message,true);}
  finally{pending.delete(id);refresh();}
}
$("task-body").addEventListener("click",event=>{const button=event.target.closest("button[data-action]");if(button&&!button.disabled)taskAction(button.dataset.id,button.dataset.action);});
$("search").addEventListener("input",refresh);$("add-task").addEventListener("click",()=>openForm());$("first-task").addEventListener("click",()=>openForm());
$("close-dialog").addEventListener("click",closeDialog);$("cancel-dialog").addEventListener("click",closeDialog);$("task-dialog").addEventListener("cancel",event=>{if(formBusy)event.preventDefault();});$("task-infinite").addEventListener("change",modeChanged);
$("stop-all").addEventListener("click",async()=>{pending.add("all");refresh();try{apply(await api("/api/tasks/stop-all","POST",{}));}catch(error){toast(error.message,true);}finally{pending.delete("all");refresh();}});
$("task-form").addEventListener("submit",async event=>{
  event.preventDefault();if(formBusy)return;const urls=$("task-urls").value.split(/\r?\n/).map(s=>s.trim()).filter(Boolean);
  if(editing&&urls.length!==1){$("form-error").textContent="编辑时只能填写一条下载链接";return;}
  const data={urls,name:$("task-name").value,concurrency:Number($("task-concurrency").value),repeat:$("task-infinite").checked?0:Number($("task-repeat").value),server_ips:$("task-ip").value,rate_mib:Number($("task-rate").value),timeout:Number($("task-timeout").value),interval:Number($("task-interval").value)};
  formBusy=true;$("form-fields").disabled=true;$("save-task").disabled=true;$("form-error").textContent="";
  try{apply(await api(editing?`/api/tasks/${editing}`:"/api/tasks",editing?"PUT":"POST",data));$("task-dialog").close();toast(editing?"任务已更新":"任务已添加并自动下载；并发已满时会排队");}
  catch(error){$("form-error").textContent=error.name==="AbortError"?"请求超时，请关闭窗口查看任务是否已添加":error.message;}
  finally{formBusy=false;$("form-fields").disabled=false;$("save-task").disabled=false;modeChanged();}
});
poll();
