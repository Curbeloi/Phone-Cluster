// Dashboard de Phone Cluster: carga /api/v1/nodes y se mantiene al día con
// el SSE de /api/v1/events. Sin framework ni build.
"use strict";

const STATES = ["Ready", "Busy", "Worker", "Hot", "LowBattery", "Draining", "Down", "Offline", "Incompatible"];

const state = {
  nodes: new Map(),   // node_id -> { view, seenAt }
  recent: [],         // fin de peticiones (ms, reloj del servidor)
  clockSkew: 0,       // servidor - navegador, en ms
  thresholds: { max_temp_c: 45, resume_temp_c: 40, max_soc_temp_c: 80, resume_soc_temp_c: 70 },
  source: null,
  devices: new Map(), // key -> DeviceView
  joins: new Map(), // device_id -> JoinView
  split: null,      // SplitView
};

const SPLIT_PHASES = {
  off: "Desactivado",
  preparing: "Preparando",
  loading: "Cargando capas",
  ready: "Listo",
  error: "Error",
};

const JOIN_STATES = { pending: "Quiere unirse", approved: "Aceptado", rejected: "Rechazado" };

const PHASES = {
  pairable: "Listo para vincular",
  connecting: "Conectando",
  unauthorized: "Sin autorizar",
  installing: "Instalando",
  running: "En marcha",
  app: "Usa la app",
  error: "Error",
};

const $ = (id) => document.getElementById(id);
const tpl = $("card-tpl");

// ---------- datos ----------

async function loadAll() {
  const res = await fetch("api/v1/nodes", { cache: "no-store" });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  const data = await res.json();
  state.clockSkew = data.now_ms - Date.now();
  state.recent = data.recent_requests_ms;
  if (data.thresholds) state.thresholds = data.thresholds;
  $("endpoint").textContent = data.openai_base_url;
  state.nodes.clear();
  for (const view of data.nodes) upsert(view);
  render();
}

function upsert(view) {
  state.nodes.set(view.node_id, { view, seenAt: Date.now() - view.last_seen_secs * 1000 });
}

function connect() {
  if (state.source) state.source.close();
  const es = new EventSource("api/v1/events");
  state.source = es;
  es.addEventListener("open", () => {
    setConn("en vivo", "live");
    // Al (re)conectar se pide la lista completa: pudo perderse algún evento.
    loadAll().catch((e) => setConn(`error: ${e.message}`, "down"));
    loadDevices().catch(() => {});
    loadJoins().catch(() => {});
    loadSplit().catch(() => {});
  });
  es.addEventListener("split_updated", (e) => {
    state.split = JSON.parse(e.data);
    renderSplit();
    render();
  });
  es.addEventListener("node_updated", (e) => {
    upsert(JSON.parse(e.data));
    render();
  });
  es.addEventListener("node_removed", (e) => {
    state.nodes.delete(JSON.parse(e.data).node_id);
    render();
  });
  es.addEventListener("request_done", (e) => {
    state.recent.push(JSON.parse(e.data).at_ms);
    renderStats();
  });
  es.addEventListener("join_updated", (e) => {
    const j = JSON.parse(e.data);
    state.joins.set(j.device_id, j);
    renderJoins();
  });
  es.addEventListener("join_removed", (e) => {
    state.joins.delete(JSON.parse(e.data).device_id);
    renderJoins();
  });
  es.addEventListener("device_updated", (e) => {
    const d = JSON.parse(e.data);
    state.devices.set(d.key, d);
    renderDevices();
  });
  es.addEventListener("device_removed", (e) => {
    state.devices.delete(JSON.parse(e.data).key);
    renderDevices();
  });
  es.addEventListener("error", () => {
    // EventSource reintenta solo; si el servidor cerró el stream, forzamos.
    setConn("reconectando…", "down");
    if (es.readyState === EventSource.CLOSED) setTimeout(connect, 2000);
  });
}

function setConn(text, cls) {
  const el = $("conn");
  el.textContent = text;
  el.className = `conn ${cls || ""}`;
}

// ---------- render ----------

function sortedNodes() {
  return [...state.nodes.values()].sort((a, b) =>
    a.view.name.localeCompare(b.view.name) || a.view.node_id.localeCompare(b.view.node_id));
}

function render() {
  const grid = $("nodes");
  const list = sortedNodes();
  $("empty").hidden = list.length > 0;
  const seen = new Set();
  for (const n of list) {
    seen.add(n.view.node_id);
    let card = grid.querySelector(`[data-id="${n.view.node_id}"]`);
    if (!card) {
      card = tpl.content.firstElementChild.cloneNode(true);
      card.dataset.id = n.view.node_id;
      card.querySelector(".drain").addEventListener("click", () => toggleDrain(card.dataset.id));
      card.querySelector(".forget").addEventListener("click", (e) => forget(card.dataset.id, e.currentTarget));
      card.querySelector(".restart").addEventListener("click", () => restartLlama(card.dataset.id));
    }
    grid.appendChild(card); // mantiene el orden por nombre
    fillCard(card, n);
  }
  for (const card of [...grid.children]) {
    if (!seen.has(card.dataset.id)) card.remove();
  }
  renderStats();
  renderModels();
}

function fillCard(card, n) {
  const v = n.view;
  const t = state.thresholds;
  card.className = `card s-${v.status}`;
  const q = (sel) => card.querySelector(sel);
  q(".name").textContent = v.name;
  q(".name").title = v.node_id;
  q(".badge").textContent = v.status;
  q(".seen").textContent = seenText(n.seenAt);
  q(".model").textContent = v.llama?.model ?? "modelo desconocido";
  q(".url").textContent = v.advertise_url ?? "";

  const b = v.battery;
  q(".battery").textContent = b
    ? `${b.pct} %${b.charging ? " ⚡" : ""}${b.temp_c != null ? ` · ${b.temp_c.toFixed(1)} °C` : ""}`
    : "—";
  if (b?.charging) q(".battery").title = "cargando";

  const temp = v.thermal?.max_c;
  q(".thermal").textContent = temp != null ? `${temp.toFixed(1)} °C` : "—";
  const fill = q(".fill");
  const pct = temp != null ? Math.min(100, Math.max(0, ((temp - 20) / (t.max_soc_temp_c - 20)) * 100)) : 0;
  fill.style.width = `${pct}%`;
  fill.className = "fill" + (temp == null ? "" : temp >= t.max_soc_temp_c ? " red" : temp >= t.resume_soc_temp_c ? " amber" : "");

  const m = v.memory;
  q(".ram").textContent = m ? `${fmtMb(m.available_mb)} / ${fmtMb(m.total_mb)}` : "—";

  const l = v.llama;
  const busy = l?.slots_busy ?? "?";
  const total = l?.slots_total ?? "?";
  q(".slots").textContent = `${busy} / ${total} · en vuelo ${v.in_flight}`;
  q(".toks").textContent = v.last_tok_s != null ? v.last_tok_s.toFixed(1) : "—";
  q(".served").textContent = v.requests_served;

  const reason = q(".reason");
  const why = v.incompatible_reason
    ? `Incompatible: ${v.incompatible_reason}`
    : v.last_error ? `Último fallo: ${v.last_error}` : "";
  reason.hidden = !why;
  reason.textContent = why;

  const drain = q(".drain");
  drain.textContent = v.draining ? "Undrain" : "Drain";
  drain.hidden = v.status === "Incompatible";
  q(".forget").hidden = v.status !== "Offline";
  // La cabeza del modo dividido no tiene agente: nada que drenar ni reiniciar.
  const isHead = v.node_id === state.split?.head_id;
  if (isHead) drain.hidden = true;
  const restart = q(".restart");
  const pending = (v.pending_commands || []).includes("restart_llama");
  restart.hidden = isHead || v.status === "Offline" || v.status === "Incompatible" || v.status === "Worker";
  restart.disabled = pending;
  restart.textContent = pending ? "Reinicio pendiente…" : "Reiniciar llama";
}

function renderStats() {
  const counts = Object.fromEntries(STATES.map((s) => [s, 0]));
  let free = 0;
  for (const { view } of state.nodes.values()) {
    counts[view.status] = (counts[view.status] || 0) + 1;
    if (view.status === "Ready" || view.status === "Busy") free += view.free_slots;
  }
  const now = Date.now() + state.clockSkew;
  state.recent = state.recent.filter((t) => now - t <= 60_000);

  const stats = $("stats");
  stats.replaceChildren();
  for (const s of STATES) {
    if (!counts[s] && s !== "Ready") continue;
    const span = document.createElement("span");
    span.className = `s-${s}`;
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.style.background = "var(--status)";
    const b = document.createElement("b");
    b.textContent = counts[s];
    span.append(dot, b, ` ${s}`);
    stats.append(span);
  }
  for (const [label, value] of [["slots libres", free], ["pet/min", state.recent.length]]) {
    const span = document.createElement("span");
    const b = document.createElement("b");
    b.textContent = value;
    span.append(b, ` ${label}`);
    stats.append(span);
  }
}

function renderModels() {
  const select = $("model");
  const models = new Set();
  for (const { view } of state.nodes.values()) {
    if ((view.status === "Ready" || view.status === "Busy") && view.llama?.model) models.add(view.llama.model);
  }
  const wanted = ["auto", ...[...models].sort()];
  const current = [...select.options].map((o) => o.value);
  if (wanted.join("\n") === current.join("\n")) return;
  const selected = select.value;
  select.replaceChildren(...wanted.map((m) => new Option(m, m)));
  select.value = wanted.includes(selected) ? selected : "auto";
}

function seenText(seenAt) {
  const s = Math.max(0, Math.round((Date.now() - seenAt) / 1000));
  if (s < 60) return `visto hace ${s} s`;
  if (s < 3600) return `visto hace ${Math.floor(s / 60)} min`;
  return `visto hace ${Math.floor(s / 3600)} h`;
}

function fmtMb(mb) {
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb} MB`;
}

// ---------- modo dividido ----------

async function loadSplit() {
  const res = await fetch("api/v1/split", { cache: "no-store" });
  if (!res.ok) return;
  state.split = await res.json();
  renderSplit();
  render();
}

function renderSplit() {
  const v = state.split;
  if (!v) return;
  const phase = $("split-phase");
  phase.textContent = SPLIT_PHASES[v.phase] ?? v.phase;
  phase.className = `badge sp-${v.phase}`;
  const toggle = $("split-toggle");
  toggle.textContent = v.enabled ? "Desactivar" : "Activar";
  toggle.className = `btn small ${v.enabled ? "" : "primary"}`;
  toggle.disabled = false;

  const msg = $("split-message");
  msg.hidden = !v.message;
  msg.textContent = v.message ?? "";
  msg.className = `split-message${v.phase === "error" ? " error" : ""}`;

  const box = $("split-layers");
  box.hidden = !v.enabled || v.members.length === 0;
  const total = v.n_layer;
  $("split-summary").textContent = total != null
    ? `${v.model ?? "modelo"} · ${total} capas + la de salida, en ${v.members.length} teléfonos`
    : `${v.model ?? "Modelo"} · reparto según la RAM libre de cada teléfono`;
  const blocksOf = (m) => (m.layers?.blocks ? m.layers.blocks[1] - m.layers.blocks[0] + 1 : 0);
  const color = (i) => `var(--c${(i % 4) + 1})`;

  const bar = $("split-bar");
  bar.replaceChildren(...v.members.map((m, i) => {
    const seg = document.createElement("div");
    seg.className = "seg";
    seg.style.setProperty("--seg", color(i));
    // Sin capas conocidas todavía: el ancho es su parte de RAM.
    seg.style.flexGrow = total != null ? blocksOf(m) + (m.layers?.output ? 1 : 0) : m.share_mb;
    seg.textContent = m.layers?.blocks ? `${m.name} · ${m.layers.blocks[0]}–${m.layers.blocks[1]}` : m.name;
    seg.title = seg.textContent;
    return seg;
  }));
  bar.setAttribute("aria-label", v.members.map((m) => `${m.name}: ${blocksOf(m)} capas`).join(", "));

  $("split-members").replaceChildren(...v.members.map((m, i) => {
    const li = document.createElement("li");
    li.style.setProperty("--seg", color(i));
    const sw = document.createElement("span");
    sw.className = "swatch";
    const text = document.createElement("div");
    const name = document.createElement("b");
    name.textContent = m.name;
    const parts = [];
    if (m.layers?.blocks) parts.push(`capas ${m.layers.blocks[0]}–${m.layers.blocks[1]} (${blocksOf(m)})${m.layers.output ? " + salida" : ""}`);
    if (m.model_mib != null) parts.push(`${Math.round(m.model_mib)} MiB del modelo`);
    parts.push(`${fmtMb(m.share_mb)} libres`);
    parts.push(m.endpoint);
    text.append(name, ` · ${parts.join(" · ")}`);
    li.append(sw, text);
    return li;
  }));

  const log = $("split-log");
  log.hidden = !v.log_tail?.length;
  log.textContent = (v.log_tail ?? []).join("\n");
}

$("split-toggle").addEventListener("click", async () => {
  const on = !state.split?.enabled;
  $("split-toggle").disabled = true;
  const res = await fetch(`api/v1/split/${on ? "on" : "off"}`, { method: "POST" });
  if (!res.ok) {
    $("split-toggle").disabled = false;
    toast(`No se pudo: HTTP ${res.status}`);
    return;
  }
  state.split = await res.json();
  renderSplit();
});

// ---------- alta con la app Android ----------

async function loadJoins() {
  const res = await fetch("api/v1/joins", { cache: "no-store" });
  if (!res.ok) return;
  const data = await res.json();
  $("app-url").textContent = data.app_url;
  const warn = $("join-warning");
  let msg = "";
  if (!data.lan_ok) msg = "Este Mac no está conectado a una red Wi-Fi o de área local: los teléfonos no podrán llegar a él.";
  else if (!data.app_ready) msg = "Falta compilar la app del teléfono: ejecuta tools/build-android.sh una vez.";
  else if (!data.model_ready) msg = "Falta el modelo (.gguf) que se copia a los teléfonos: ejecuta tools/build-android.sh.";
  warn.hidden = !msg;
  warn.textContent = msg;
  state.joins.clear();
  for (const j of data.requests) state.joins.set(j.device_id, j);
  renderJoins();
}

function renderJoins() {
  const box = $("joins");
  const list = [...state.joins.values()].sort((a, b) => a.device_id.localeCompare(b.device_id));
  box.replaceChildren(...list.map((j) => {
    const row = document.createElement("div");
    row.className = `device j-${j.state}`;
    const who = document.createElement("div");
    who.className = "who";
    const title = document.createElement("b");
    title.textContent = j.label || j.model;
    const sub = document.createElement("span");
    sub.textContent = `${j.model} · ${j.ip} · nodo ${j.node_name}`;
    who.append(title, sub);
    const badge = document.createElement("span");
    badge.className = "badge";
    badge.textContent = JOIN_STATES[j.state] ?? j.state;
    row.append(who, badge);
    if (j.state !== "approved") {
      const options = j.state === "pending"
        ? [["Aceptar", "approve", "primary"], ["Rechazar", "reject", ""]]
        : [["Aceptar", "approve", "primary"]];
      for (const [label, decision, cls] of options) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = `btn small ${cls}`;
        btn.textContent = label;
        btn.addEventListener("click", async () => {
          btn.disabled = true;
          const res = await fetch(`api/v1/joins/${encodeURIComponent(j.device_id)}/${decision}`, { method: "POST" });
          if (!res.ok) toast(`No se pudo: HTTP ${res.status}`);
        });
        row.append(btn);
      }
    }
    return row;
  }));
}

// ---------- teléfonos por ADB ----------

async function loadDevices() {
  const res = await fetch("api/v1/devices", { cache: "no-store" });
  if (!res.ok) return;
  const data = await res.json();
  const warn = $("devices-warning");
  let msg = "";
  if (!data.enabled) msg = "La instalación por ADB está desactivada ([adb] enabled = false).";
  else if (!data.adb) msg = "No encuentro adb. Instala Android platform-tools o define [adb] adb_path en coordinator.toml.";
  else if (data.artifacts_missing) msg = `Falta compilar lo que se instala en los teléfonos (${data.artifacts_missing}). Ejecuta tools/build-android.sh una vez.`;
  warn.hidden = !msg;
  warn.textContent = msg;
  state.devices.clear();
  for (const d of data.devices) state.devices.set(d.key, d);
  renderDevices();
}

function renderDevices() {
  const box = $("devices");
  const list = [...state.devices.values()].sort((a, b) => a.key.localeCompare(b.key));
  const focused = document.activeElement?.closest?.(".device")?.dataset.key;
  const seen = new Set();
  for (const d of list) {
    seen.add(d.key);
    let row = box.querySelector(`[data-key="${CSS.escape(d.key)}"]`);
    if (!row) {
      row = document.createElement("div");
      row.dataset.key = d.key;
      box.append(row);
    }
    // No rehacer la fila mientras se escribe el código.
    if (focused === d.key && row.dataset.phase === d.phase) {
      row.querySelector(".detail").textContent = d.detail ?? "";
      continue;
    }
    fillDevice(row, d);
  }
  for (const row of [...box.children]) if (!seen.has(row.dataset.key)) row.remove();
}

function fillDevice(row, d) {
  row.className = `device d-${d.phase}`;
  row.dataset.phase = d.phase;
  const who = document.createElement("div");
  who.className = "who";
  const title = document.createElement("b");
  title.textContent = d.model || d.key;
  const sub = document.createElement("span");
  sub.textContent = [d.key, d.node_name && `nodo ${d.node_name}`].filter(Boolean).join(" · ");
  who.append(title, sub);
  const badge = document.createElement("span");
  badge.className = "badge";
  badge.textContent = PHASES[d.phase] ?? d.phase;
  const parts = [who, badge];

  if (d.phase === "pairable") {
    const input = document.createElement("input");
    input.inputMode = "numeric";
    input.maxLength = 6;
    input.placeholder = "código";
    input.setAttribute("aria-label", `Código de vinculación de ${d.model || d.key}`);
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn small primary";
    btn.textContent = "Vincular";
    const go = () => pairDevice(d.key, input.value, btn);
    btn.addEventListener("click", go);
    input.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); });
    parts.push(input, btn);
  } else if (d.phase === "error" || d.phase === "running") {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn small";
    btn.textContent = d.phase === "error" ? "Reintentar" : "Reinstalar";
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      await fetch("api/v1/devices/reinstall", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ key: d.key }),
      });
    });
    parts.push(btn);
  }
  const detail = document.createElement("div");
  detail.className = "detail";
  detail.textContent = d.detail ?? "";
  parts.push(detail);
  row.replaceChildren(...parts);
}

async function pairDevice(key, code, btn) {
  btn.disabled = true;
  btn.textContent = "Vinculando…";
  const res = await fetch("api/v1/devices/pair", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ key, code: code.trim() }),
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    toast(`No se pudo vincular: ${body.error?.message ?? res.status}`);
    btn.disabled = false;
    btn.textContent = "Vincular";
  }
}

// Refresca "visto hace N s" y pet/min sin esperar eventos.
setInterval(() => {
  for (const n of state.nodes.values()) {
    const card = $("nodes").querySelector(`[data-id="${n.view.node_id}"] .seen`);
    if (card) card.textContent = seenText(n.seenAt);
  }
  renderStats();
}, 1000);

// ---------- acciones ----------

function toast(message) {
  const el = $("toast");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => (el.hidden = true), 6000);
}

async function toggleDrain(id) {
  const n = state.nodes.get(id);
  if (!n) return;
  const res = await fetch(`api/v1/nodes/${id}/drain`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ drain: !n.view.draining }),
  });
  if (res.ok) {
    upsert(await res.json());
    render();
  }
}

async function restartLlama(id) {
  const res = await fetch(`api/v1/nodes/${id}/restart_llama`, { method: "POST" });
  if (!res.ok) toast(`No se pudo encolar el reinicio: HTTP ${res.status}`);
}

// Sin confirm(): en la app de escritorio (WebKit) los diálogos del navegador
// pueden no mostrarse. El primer clic arma el botón y el segundo confirma.
async function forget(id, btn) {
  const n = state.nodes.get(id);
  if (!n) return;
  if (btn.dataset.armed !== "1") {
    btn.dataset.armed = "1";
    btn.textContent = "¿Seguro? Pulsa otra vez";
    setTimeout(() => { btn.dataset.armed = ""; btn.textContent = "Olvidar"; }, 3000);
    return;
  }
  const res = await fetch(`api/v1/nodes/${id}`, { method: "DELETE" });
  if (res.status === 204 || res.status === 404) {
    state.nodes.delete(id);
    render();
  } else {
    toast(`No se pudo olvidar: HTTP ${res.status}`);
  }
}

$("copy").addEventListener("click", async () => {
  const text = $("endpoint").textContent;
  try {
    await navigator.clipboard.writeText(text);
    $("copy").textContent = "Copiado";
  } catch {
    $("copy").textContent = "Selecciónalo";
  }
  setTimeout(() => ($("copy").textContent = "Copiar"), 1500);
});

// ---------- panel de prueba ----------

$("test-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const out = $("output");
  const meta = $("test-meta");
  const send = $("send");
  out.textContent = "";
  meta.textContent = "enviando…";
  send.disabled = true;
  const started = performance.now();
  let deltas = 0;
  let firstAt = null;
  let tokS = null;
  try {
    const res = await fetch("v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        model: $("model").value,
        stream: true,
        messages: [{ role: "user", content: $("prompt").value }],
      }),
    });
    const node = res.headers.get("x-pc-node");
    if (!res.ok) {
      const body = await res.text();
      let msg = body;
      try { msg = JSON.parse(body).error?.message ?? body; } catch { /* texto plano */ }
      meta.textContent = `HTTP ${res.status}${node ? ` · nodo ${node}` : ""}`;
      out.textContent = msg;
      return;
    }
    meta.textContent = `nodo ${node ?? "?"} · generando…`;
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let nl;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (data === "[DONE]") continue;
        let chunk;
        try { chunk = JSON.parse(data); } catch { continue; }
        const text = chunk.choices?.[0]?.delta?.content;
        if (text) {
          firstAt ??= performance.now();
          deltas += 1;
          out.textContent += text;
        }
        if (chunk.timings?.predicted_per_second) tokS = chunk.timings.predicted_per_second;
      }
    }
    if (tokS == null && deltas > 1 && firstAt != null) {
      tokS = (deltas - 1) / ((performance.now() - firstAt) / 1000); // aproximado: 1 delta ≈ 1 token
    }
    const secs = ((performance.now() - started) / 1000).toFixed(2);
    meta.textContent = `nodo ${node ?? "?"} · ${tokS != null ? tokS.toFixed(1) : "?"} tok/s · ${secs} s`;
  } catch (e) {
    meta.textContent = `error: ${e.message}`;
  } finally {
    send.disabled = false;
  }
});

connect();
