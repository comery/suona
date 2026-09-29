/**
 * suona — the pet's brain.
 *
 * Rust polls every agent and pushes a `Snapshot` on `suona://events`.  This
 * module decides what the pet actually says, so the user is informed without
 * being spammed: one thing at a time, urgent things first, and never a
 * re-run of history on startup.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ── wire format (mirrors src-tauri/src/model.rs) ────────────────────────────

type AgentKey = "hermes" | "codex" | "claude_code";
type Severity = "info" | "success" | "warning" | "error";

interface AgentEvent {
  id: string;
  agent: AgentKey;
  kind: string;
  severity: Severity;
  title: string;
  detail: string;
  project: string | null;
  at: number;
  meta: Record<string, string>;
}

interface AgentSummary {
  agent: AgentKey;
  label: string;
  detected: boolean;
  headline: string;
  healthy: number;
  failing: number;
  paused: number;
  last_activity: number | null;
}

interface Snapshot {
  generated_at: number;
  summaries: AgentSummary[];
  events: AgentEvent[];
}

// ── tiny DOM helpers ───────────────────────────────────────────────────────

function el<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing element #${id}`);
  return node as T;
}

const stage = el("stage");
const pet = el("pet");
const bubble = el("bubble");
const bubbleAgent = el("bubble-agent");
const bubbleTime = el("bubble-time");
const bubbleTitle = el("bubble-title");
const bubbleDetail = el("bubble-detail");
const hud = el("hud");
const notes = el("notes");
const panel = el("panel");
const panelCount = el("panel-count");
const panelList = el("panel-list");
const panelClose = el<HTMLButtonElement>("panel-close");
const autostartBox = el<HTMLInputElement>("autostart");
const soundBox = el<HTMLInputElement>("sound");
const sizePicker = el("size-picker");
const rangePicker = el("range-picker");
const panelTitle = el("panel-title");
const configList = el("config-list");
const configBack = el<HTMLButtonElement>("config-back");
const configRescan = el<HTMLButtonElement>("config-rescan");
const configNote = el("config-note");
const controlsNote = el("controls-note");

// ── presentation metadata ──────────────────────────────────────────────────

const AGENT_LABEL: Record<AgentKey, string> = {
  hermes: "Hermes",
  codex: "Codex",
  claude_code: "Claude Code",
};

const AGENT_CLASS: Record<AgentKey, string> = {
  hermes: "agent-hermes",
  codex: "agent-codex",
  claude_code: "agent-claude",
};

const SEVERITY_COLOR: Record<Severity, string> = {
  error: "var(--error)",
  warning: "var(--warning)",
  success: "var(--success)",
  info: "var(--info)",
};

const SEVERITY_RANK: Record<Severity, number> = {
  error: 3,
  warning: 2,
  success: 1,
  info: 0,
};

// ── state ──────────────────────────────────────────────────────────────────

/** Events already shown, so a poll never repeats itself. */
const spoken = new Set<string>();
/** Everything the pet could still say, most important first. */
let pending: AgentEvent[] = [];
let latest: Snapshot | null = null;
let speaking = false;
let speakTimer: number | undefined;
/** The pet greets once per launch, so it is visibly alive even with no news. */
let greeted = false;

const DWELL_MS = 6500;
const IDLE_GAP_MS = 1400;
/**
 * Only events this recent are worth voicing.  Without a window, the pet would
 * dig up a backlog of yesterday's finished jobs on every launch.
 */
const RECENT_MS = 60 * 60 * 1000;

// ── speech ─────────────────────────────────────────────────────────────────

function puffNotes(): void {
  notes.replaceChildren();
  const glyphs = ["♪", "♫", "♩", "♬"];
  for (let i = 0; i < 5; i++) {
    const span = document.createElement("span");
    span.textContent = glyphs[i % glyphs.length];
    span.style.setProperty("--dx", `${(i - 2) * 17 + (Math.random() * 10 - 5)}px`);
    span.style.setProperty("--rot", `${Math.random() * 60 - 30}deg`);
    span.style.animationDelay = `${i * 0.16}s`;
    span.style.left = `${Math.random() * 22 - 11}px`;
    notes.appendChild(span);
  }
}

function say(opts: {
  chip: string;
  chipClass: string;
  title: string;
  detail: string;
  severity: Severity;
  time?: string;
}): void {
  bubbleAgent.textContent = opts.chip;
  bubbleAgent.className = `chip ${opts.chipClass}`;
  bubbleTime.textContent = opts.time ?? "刚刚";
  bubbleTitle.textContent = opts.title;
  bubbleDetail.textContent = opts.detail;
  bubble.style.setProperty("--accent", SEVERITY_COLOR[opts.severity]);
  bubble.classList.add("visible");

  pet.classList.add("speaking");
  puffNotes();
  speaking = true;

  window.clearTimeout(speakTimer);
  speakTimer = window.setTimeout(() => {
    bubble.classList.remove("visible");
    pet.classList.remove("speaking");
    speaking = false;
    // Chain into the next item, if the user has not already moved on.
    window.setTimeout(speakNext, IDLE_GAP_MS);
  }, DWELL_MS);
}

function sayEvent(ev: AgentEvent): void {
  spoken.add(ev.id);
  playReport(ev.severity);
  say({
    chip: AGENT_LABEL[ev.agent] ?? "agent",
    chipClass: AGENT_CLASS[ev.agent] ?? "agent-suona",
    title: ev.title,
    detail: ev.detail,
    severity: ev.severity,
    time: relativeTime(ev.at),
  });
}

function speakNext(): void {
  if (speaking) return;
  const next = pending.shift();
  if (next) {
    sayEvent(next);
  }
}

/** Stand-in message when there is no news — keeps the pet feeling alive. */
function sayStatus(snapshot: Snapshot): void {
  const detected = snapshot.summaries.filter((s) => s.detected);
  const failing = detected.reduce((n, s) => n + s.failing, 0);
  const quiet = detected.filter((s) => s.detected && s.failing === 0).length;

  if (detected.length === 0) {
    say({
      chip: "suona",
      chipClass: "agent-suona",
      title: "还没找到可监控的 agent",
      detail: "已尝试读取 ~/.hermes、~/.codex 和 ~/.claude",
      severity: "info",
    });
    return;
  }

  const detail = detected.map((s) => `${s.label} ${s.headline}`).join(" · ");
  say({
    chip: "suona",
    chipClass: "agent-suona",
    title:
      failing > 0
        ? `${failing} 项需要关注`
        : `${quiet} 个 agent 一切正常`,
    detail,
    severity: failing > 0 ? "warning" : "success",
  });
}

// ── snapshot handling ──────────────────────────────────────────────────────

function ingest(snapshot: Snapshot): void {
  latest = snapshot;

  const cutoff = Date.now() - RECENT_MS;
  const fresh = snapshot.events.filter((e) => !spoken.has(e.id) && e.at >= cutoff);

  // Remember everything, including events too old to voice, so the backlog
  // never resurfaces on a later poll.
  for (const e of snapshot.events) spoken.add(e.id);

  if (fresh.length > 0) {
    pending = [...pending, ...fresh].sort(
      (a, b) =>
        SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity] || b.at - a.at,
    );
    // Keep the queue short: only the most recent handful matter.
    pending = pending.slice(0, 6);
  }

  updateHud();
  if (expanded) renderPanel();

  if (!greeted) {
    greeted = true;
    // A docked pet is off-screen; the greeting would have nowhere to appear.
    if (petState.dock === "none") sayStatus(snapshot);
    return;
  }

  // While tucked into an edge there is no bubble to speak through, so news is
  // reported with the glow and the sound only — the system notification comes
  // from the Rust side.
  if (petState.dock !== "none") {
    const urgent = fresh.find(
      (e) => e.severity === "error" || e.severity === "warning",
    );
    if (urgent) {
      flashAlert();
      playReport(urgent.severity);
    }
    // Anything still queued stays there and plays when the pet comes back out.
    return;
  }

  if (!speaking) speakNext();
}

function updateHud(): void {
  if (!latest) return;
  const failing = latest.summaries.reduce((n, s) => n + s.failing, 0);
  const detected = latest.summaries.filter((s) => s.detected).length;
  const tone = failing > 0 ? "bad" : detected > 0 ? "ok" : "warn";

  hud.replaceChildren();
  const dot = document.createElement("span");
  dot.className = `dot ${tone}`;
  hud.appendChild(dot);

  const text = document.createElement("span");
  text.textContent =
    failing > 0
      ? `${failing} 项异常`
      : detected > 0
        ? `${detected} 个 agent 正常`
        : "未检测到 agent";
  hud.appendChild(text);

  for (const s of latest.summaries) {
    if (!s.detected) continue;
    const sep = document.createElement("span");
    sep.className = "sep";
    sep.textContent = "|";
    hud.appendChild(sep);

    const item = document.createElement("span");
    item.textContent = `${s.label} ${s.failing > 0 ? "⚠" : "✓"}`;
    hud.appendChild(item);
  }
}

function relativeTime(ms: number): string {
  const delta = Date.now() - ms;
  if (delta < 60_000) return "刚刚";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`;
  return `${Math.floor(delta / 86_400_000)} 天前`;
}

// ── the full event list ────────────────────────────────────────────────────

/** How many rows the list shows before it stops being useful. */
const PANEL_ROWS = 60;

/**
 * Which slice of history the list shows.  Today is the default: the pet's job
 * is to report what just happened, and older finished jobs are noise.
 */
type Range = "today" | "all";
let range: Range = "today";

let expanded = false;

/** Local midnight, or the epoch when showing everything. */
function rangeStart(): number {
  if (range === "all") return 0;
  const midnight = new Date();
  midnight.setHours(0, 0, 0, 0);
  return midnight.getTime();
}

async function setPanel(open: boolean): Promise<void> {
  expanded = open;
  panel.classList.toggle("visible", open);
  syncPeek();
  if (open) renderPanel();

  if (inTauri) {
    // The window itself has to grow; without this the list would be clipped.
    await invoke("set_expanded", { expanded: open }).catch(() => {});
  }
}

function renderPanel(): void {
  if (!latest) return;

  const since = rangeStart();
  const matching = latest.events.filter((e) => e.at >= since);
  const events = matching.slice(0, PANEL_ROWS);

  const failing = latest.summaries.reduce((n, s) => n + s.failing, 0);
  const shown =
    matching.length > events.length
      ? `${events.length}/${matching.length} 条`
      : `${events.length} 条`;
  panelCount.textContent =
    events.length > 0 ? `${shown}${failing > 0 ? ` · ${failing} 项异常` : ""}` : "";

  panelList.replaceChildren();

  if (events.length === 0) {
    const empty = document.createElement("div");
    empty.className = "panel-empty";
    // Name the slice that is empty and how to widen it; a bare "暂无记录"
    // reads as though the pet has nothing to report at all.
    empty.textContent =
      range === "today" && latest.events.length > 0
        ? "今天暂无记录 · 切到「全部」看历史"
        : "暂无记录";
    panelList.appendChild(empty);
    return;
  }

  for (const ev of events) {
    panelList.appendChild(buildRow(ev));
  }
}

function buildRow(ev: AgentEvent): HTMLElement {
  const row = document.createElement("div");
  row.className = `row sev-${ev.severity}`;
  row.title = "点击让唢呐念给我听";

  const dot = document.createElement("span");
  dot.className = "row-dot";
  row.appendChild(dot);

  const main = document.createElement("div");
  main.className = "row-main";

  const title = document.createElement("div");
  title.className = "row-title";
  title.textContent = ev.title;

  const detail = document.createElement("div");
  detail.className = "row-detail";
  detail.textContent = ev.detail;

  main.append(title, detail);
  row.appendChild(main);

  const side = document.createElement("div");
  side.className = "row-side";

  const chip = document.createElement("span");
  chip.className = `row-agent ${AGENT_CLASS[ev.agent] ?? "agent-suona"}`;
  chip.textContent = AGENT_LABEL[ev.agent] ?? "agent";

  const time = document.createElement("span");
  time.className = "row-time";
  time.textContent = relativeTime(ev.at);

  side.append(chip, time);
  row.appendChild(side);

  row.addEventListener("click", () => {
    void setPanel(false);
    window.clearTimeout(speakTimer);
    speaking = false;
    sayEvent(ev);
  });

  return row;
}

// ── pet appearance, driven by the Rust side ────────────────────────────────

type Dock = "none" | "left" | "right" | "top" | "bottom";
type PetSize = "small" | "medium" | "large";

interface PetState {
  dock: Dock;
  size: PetSize;
  sound: boolean;
  angle: number;
  /** How much of the pet is inside the window, in logical px. */
  strip: number;
}

let petState: PetState = {
  dock: "none",
  size: "large",
  sound: false,
  angle: 0,
  strip: 40,
};

function applyPetState(next: PetState): void {
  const wasDocked = petState.dock !== "none";
  petState = next;

  stage.dataset.dock = next.dock;
  stage.dataset.size = next.size;
  stage.style.setProperty("--angle", `${next.angle}deg`);
  // The backend decides how far the pet pokes out, so the window it sized and
  // the pet drawn inside it can never disagree.
  stage.style.setProperty("--strip", `${next.strip}px`);
  syncPeek();

  soundBox.checked = next.sound;
  for (const btn of sizePicker.querySelectorAll("button")) {
    btn.classList.toggle("active", (btn as HTMLElement).dataset.size === next.size);
  }

  // Docked, the bubble has nowhere to appear — but the list does, beside the
  // pet, so an open list is left alone.
  if (next.dock !== "none" && !wasDocked) {
    window.clearTimeout(speakTimer);
    bubble.classList.remove("visible");
    pet.classList.remove("speaking");
    speaking = false;
  }
}

/**
 * Tell the layout whether the pet is currently poking out to make room.
 *
 * Only meaningful while docked; the stylesheet keys the panel's placement and
 * the pet's travel off it.
 */
function syncPeek(): void {
  stage.dataset.peek =
    expanded && petState.dock !== "none" ? "open" : "closed";
}

/** A docked pet cannot speak, so it pulses instead. */
function flashAlert(): void {
  if (petState.dock === "none") return;
  pet.classList.add("alert");
  window.setTimeout(() => pet.classList.remove("alert"), 4000);
}

// ── report sounds ──────────────────────────────────────────────────────────

/**
 * Tones are synthesised rather than loaded, so there are no audio assets to
 * ship and each severity can get its own shape.  The context is created on the
 * user's first toggle, because browsers refuse to start audio without a
 * gesture — which is exactly why the feature defaults to off.
 */
let audio: AudioContext | null = null;

function audioContext(): AudioContext | null {
  if (audio === null) {
    const Ctor =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext?: typeof AudioContext })
        .webkitAudioContext;
    if (!Ctor) return null;
    audio = new Ctor();
  }
  if (audio.state === "suspended") void audio.resume();
  return audio;
}

function blip(
  ctx: AudioContext,
  freq: number,
  at: number,
  dur: number,
  type: OscillatorType,
  peak: number,
): void {
  const t0 = ctx.currentTime + at;
  const osc = ctx.createOscillator();
  const gain = ctx.createGain();
  osc.type = type;
  osc.frequency.setValueAtTime(freq, t0);
  // Short attack, exponential tail: a soft chime rather than a click.
  gain.gain.setValueAtTime(0.0001, t0);
  gain.gain.exponentialRampToValueAtTime(peak, t0 + 0.012);
  gain.gain.exponentialRampToValueAtTime(0.0001, t0 + dur);
  osc.connect(gain).connect(ctx.destination);
  osc.start(t0);
  osc.stop(t0 + dur + 0.02);
}

type Note = [freq: number, at: number, dur: number, type: OscillatorType, peak: number];

/**
 * One motif per severity, chosen to be distinguishable without looking:
 * a falling pair for failures, a repeated pair for warnings, a rising triad
 * for successes, and a single soft note for everything else.
 */
const SOUNDS: Record<Severity, Note[]> = {
  error: [
    [392.0, 0.0, 0.3, "triangle", 0.17],
    [261.6, 0.19, 0.5, "triangle", 0.17],
  ],
  warning: [
    [587.3, 0.0, 0.16, "sine", 0.14],
    [587.3, 0.21, 0.16, "sine", 0.14],
  ],
  success: [
    [523.3, 0.0, 0.2, "sine", 0.12],
    [659.3, 0.12, 0.2, "sine", 0.12],
    [784.0, 0.24, 0.34, "sine", 0.12],
  ],
  info: [[659.3, 0.0, 0.22, "sine", 0.08]],
};

function playReport(severity: Severity): void {
  if (!petState.sound) return;
  const ctx = audioContext();
  if (!ctx) return;
  for (const [freq, at, dur, type, peak] of SOUNDS[severity]) {
    blip(ctx, freq, at, dur, type, peak);
  }
}

// ── notices about suona itself ─────────────────────────────────────────────

/** A message about suona rather than about one of the agents it watches. */
interface Notice {
  title: string;
  detail: string;
  severity: Severity;
}

/** Speak an app-level notice, with the matching chime and a glow if docked. */
function sayNotice(notice: Notice): void {
  playReport(notice.severity);
  flashAlert();
  say({
    chip: "suona",
    chipClass: "agent-suona",
    title: notice.title,
    detail: notice.detail,
    severity: notice.severity,
  });
}

// ── configuration view ─────────────────────────────────────────────────────

type DetectStatus = "detected" | "incomplete" | "missing" | "disabled";

interface AgentConfigView {
  key: string;
  label: string;
  enabled: boolean;
  path: string;
  default_path: string;
  custom: boolean;
  status: DetectStatus;
  detail: string;
}

interface ConfigSnapshot {
  agents: AgentConfigView[];
}

const STATUS_TEXT: Record<DetectStatus, string> = {
  detected: "已检测到",
  incomplete: "目录不匹配",
  missing: "未找到",
  disabled: "已停用",
};

/** Switch the panel between the event list and the settings screen. */
async function showConfig(on: boolean): Promise<void> {
  panel.dataset.view = on ? "config" : "list";
  panelTitle.textContent = on ? "修改配置" : "运行汇报";
  if (!on) {
    renderPanel();
    return;
  }
  await refreshConfig();
  // The settings screen needs the same tall window the list does.
  await setPanel(true);
}

async function refreshConfig(): Promise<void> {
  if (!inTauri) return;
  try {
    renderConfig(await invoke<ConfigSnapshot>("get_agent_config"));
  } catch {
    configNote.textContent = "读取配置失败";
  }
}

/** What the displayed rows currently claim, for diffing a rescan against. */
let lastConfig: AgentConfigView[] = [];

/**
 * Re-scan the machine on demand.
 *
 * Detection already re-runs every time the settings page is rendered, but the
 * page can sit open while new software is installed — so this gives an
 * explicit "look again" trigger, and reports what actually changed.
 */
async function rescanAgents(): Promise<void> {
  if (!inTauri) return;

  const before = lastConfig;
  configRescan.disabled = true;
  const original = configRescan.textContent;
  configRescan.textContent = "检测中…";
  configNote.textContent = "";

  try {
    const snapshot = await invoke<ConfigSnapshot>("get_agent_config");
    renderConfig(snapshot);
    configNote.textContent = describeChanges(before, snapshot.agents);

    // A freshly installed agent may already have data worth listing, so pull
    // a new snapshot too rather than leaving the event list stale.
    void invoke<Snapshot>("refresh").then(ingest).catch(() => {});
  } catch {
    configNote.textContent = "检测失败";
  } finally {
    configRescan.disabled = false;
    configRescan.textContent = original;
  }
}

/** Summarise a rescan, so "nothing happened" is distinguishable from "failed". */
function describeChanges(
  before: AgentConfigView[],
  after: AgentConfigView[],
): string {
  if (before.length === 0) return "检测完成";

  const changed: string[] = [];
  for (const now of after) {
    const was = before.find((a) => a.key === now.key);
    if (was && was.status !== now.status) {
      changed.push(`${now.label} ${STATUS_TEXT[now.status]}`);
    }
  }
  return changed.length > 0 ? changed.join("，") : "检测完成，无变化";
}

function renderConfig(snapshot: ConfigSnapshot): void {
  lastConfig = snapshot.agents;
  configList.replaceChildren();
  // Clear any leftover message; rescanAgents sets its own note after this.
  configNote.textContent = "";

  for (const agent of snapshot.agents) {
    configList.appendChild(buildConfigRow(agent));
  }
}

function buildConfigRow(agent: AgentConfigView): HTMLElement {
  const row = document.createElement("div");
  row.className = "cfg-row" + (agent.enabled ? "" : " off");

  const head = document.createElement("div");
  head.className = "cfg-head";

  const toggle = document.createElement("label");
  toggle.className = "cfg-toggle";
  toggle.title = agent.enabled ? "取消勾选即停止汇报该 agent" : "重新启用该 agent";
  const box = document.createElement("input");
  box.type = "checkbox";
  box.checked = agent.enabled;
  box.addEventListener("change", () => {
    applyConfig("set_agent_enabled", {
      key: agent.key,
      enabled: box.checked,
    }).catch(() => {
      box.checked = !box.checked;
    });
  });
  toggle.appendChild(box);

  const name = document.createElement("span");
  name.className = "cfg-name";
  name.textContent = agent.label;

  const badge = document.createElement("span");
  badge.className = `cfg-badge ${agent.status}`;
  badge.textContent = STATUS_TEXT[agent.status];

  head.append(toggle, name, badge);
  row.appendChild(head);

  const path = document.createElement("input");
  path.className = "cfg-path";
  path.type = "text";
  path.value = agent.path;
  path.spellcheck = false;
  path.title = `默认路径：${agent.default_path}`;
  // Commit on blur or Enter rather than on every keystroke: a path is only
  // meaningful once it is complete, and each commit re-scans every agent.
  path.addEventListener("change", () => {
    applyConfig("set_agent_path", { key: agent.key, path: path.value }).catch(
      (err: unknown) => {
        path.classList.add("bad");
        configNote.textContent = String(err);
      },
    );
  });
  path.addEventListener("input", () => path.classList.remove("bad"));
  row.appendChild(path);

  const detail = document.createElement("div");
  detail.className = "cfg-detail";
  detail.textContent = agent.detail;
  row.appendChild(detail);

  if (agent.custom) {
    const reset = document.createElement("button");
    reset.type = "button";
    reset.className = "cfg-reset";
    reset.textContent = "恢复默认路径";
    reset.addEventListener("click", () => {
      void applyConfig("set_agent_path", { key: agent.key, path: "" });
    });
    row.appendChild(reset);
  }

  return row;
}

/** Run a config command and re-render from whatever it returns. */
async function applyConfig(
  command: string,
  args: Record<string, unknown>,
): Promise<void> {
  const snapshot = await invoke<ConfigSnapshot>(command, args);
  renderConfig(snapshot);
  // Feed the rescan straight back in so the rollup and list reflect the change
  // immediately, rather than up to a poll interval later.
  void invoke<Snapshot>("refresh").then(ingest).catch(() => {});
}

// ── interaction ────────────────────────────────────────────────────────────

function setupInteractions(): void {
  // Dragging is handled by the `data-tauri-drag-region="deep"` attribute on
  // #pet, set in index.html.  It has to be "deep" because the suona is drawn
  // with SVG elements, which Tauri's hit test skips — a bare attribute would
  // only make the transparent margin around the artwork grabbable.
  pet.addEventListener("click", () => void handlePetClick());

  /** Tell a tap apart from a drag, then act. */
  async function handlePetClick(): Promise<void> {
    // The pet is also the window's drag handle, and a drag finishes with a
    // `click` event just like a tap does.  Pointer coordinates cannot separate
    // them — the window follows the cursor, so the pointer barely moves
    // *relative to the window*.  The backend measures the window's own travel,
    // where the difference is unmistakable.
    if (inTauri) {
      try {
        if (await invoke<boolean>("take_was_drag")) return;
      } catch {
        // If the question cannot be asked, fall through and behave as before.
      }
    }

    // Docked: the pet stays on its edge and simply pokes out further while
    // the list sits beside it.  Dragging it away is what undocks it.
    void setPanel(!expanded);
  }

  pet.addEventListener("mouseenter", () => hud.classList.add("visible"));
  pet.addEventListener("mouseleave", () => hud.classList.remove("visible"));

  // Right-click opens a native menu.  It is built in Rust so it can extend
  // past this 380px window and look like every other macOS menu; an HTML menu
  // would be clipped at the window edge.
  stage.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (!inTauri) return;
    void invoke("show_pet_menu").catch(() => {});
  });

  panelClose.addEventListener("click", () => void setPanel(false));
  configBack.addEventListener("click", () => void showConfig(false));
  configRescan.addEventListener("click", () => void rescanAgents());

  sizePicker.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(
      "button[data-size]",
    );
    if (!btn || !inTauri) return;
    invoke<PetState>("set_pet_size", { size: btn.dataset.size })
      .then(applyPetState)
      .catch(() => {});
  });

  rangePicker.addEventListener("click", (e) => {
    const btn = (e.target as HTMLElement).closest<HTMLButtonElement>(
      "button[data-range]",
    );
    const next = btn?.dataset.range;
    if (next !== "today" && next !== "all") return;
    range = next;
    // Pure view state, so it is not worth a round trip through Rust.
    for (const b of rangePicker.querySelectorAll("button")) {
      b.classList.toggle("active", (b as HTMLElement).dataset.range === range);
    }
    renderPanel();
  });

  soundBox.addEventListener("change", () => {
    const wanted = soundBox.checked;
    // Create the audio context inside this gesture, otherwise the browser will
    // keep it suspended and later reports would play silently.
    if (wanted) audioContext();
    if (!inTauri) {
      petState = { ...petState, sound: wanted };
      return;
    }
    invoke<PetState>("set_sound", { enabled: wanted })
      .then(applyPetState)
      .catch(() => {
        soundBox.checked = !wanted;
      });
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && expanded) void setPanel(false);
  });

  void initAutostart();
}

/** Reflect the real login-item state; never show an optimistic toggle. */
async function initAutostart(): Promise<void> {
  if (!inTauri) {
    autostartBox.disabled = true;
    controlsNote.textContent = "仅桌面版可用";
    return;
  }

  try {
    autostartBox.checked = await invoke<boolean>("get_autostart");
  } catch {
    autostartBox.disabled = true;
    controlsNote.textContent = "不可用";
    return;
  }

  autostartBox.addEventListener("change", () => {
    const wanted = autostartBox.checked;
    controlsNote.textContent = "设置中…";
    invoke<boolean>("set_autostart", { enabled: wanted })
      .then((actual) => {
        autostartBox.checked = actual;
        controlsNote.textContent = actual ? "已启用" : "已关闭";
      })
      .catch(() => {
        autostartBox.checked = !wanted;
        controlsNote.textContent = "设置失败";
      });
  });
}

// ── wiring ─────────────────────────────────────────────────────────────────

const inTauri = "__TAURI_INTERNALS__" in window;

async function boot(): Promise<void> {
  setupInteractions();

  if (!inTauri) {
    // Browser preview: exercise the visuals without the desktop shell.
    document.body.appendChild(
      Object.assign(document.createElement("div"), {
        textContent: "browser preview — mock data",
        style:
          "position:absolute;left:8px;top:8px;font-size:9px;color:#999;z-index:99",
      }),
    );
    ingest(mockSnapshot());
    window.setInterval(() => ingest(mockSnapshot()), 12_000);
    return;
  }

  await listen<Snapshot>("suona://events", (event) => ingest(event.payload));
  // The Rust side owns the dock edge, size and bell angle; it pushes them here.
  await listen<PetState>("suona://pet", (event) => applyPetState(event.payload));
  // Docking turns the window into a thin strip, so the list has to fold away.
  await listen("suona://collapse", () => void setPanel(false));
  // Something went wrong inside suona itself, while it is still running.
  await listen<Notice>("suona://notice", (event) => sayNotice(event.payload));
  // "修改配置" in the right-click menu.
  await listen("suona://config", () => void showConfig(true));

  applyPetState(await invoke<PetState>("get_pet_state"));

  // How the previous run ended.  Fetched rather than pushed so it cannot race
  // this boot, and announced ahead of the greeting because it matters more.
  const startup = await invoke<Notice | null>("take_startup_notice");
  if (startup) {
    greeted = true;
    sayNotice(startup);
  }

  const initial = await invoke<Snapshot | null>("get_snapshot");
  if (initial) {
    ingest(initial);
  } else {
    // The poller has not finished its first sweep yet; greet meanwhile and
    // leave `greeted` set so the first real snapshot does not greet twice.
    greeted = true;
    say({
      chip: "suona",
      chipClass: "agent-suona",
      title: "唢呐已就位",
      detail: "正在读取本地 agent 的运行信息…",
      severity: "info",
    });
  }
}

function mockSnapshot(): Snapshot {
  const now = Date.now();
  const samples: AgentEvent[] = [
    {
      id: `mock-${now}-1`,
      agent: "hermes",
      kind: "job_delivery_failed",
      severity: "warning",
      title: "daily-library-sync：任务完成但投递失败",
      detail: "耗时 9m33s · ObsidianNote",
      project: "/Users/carpe/Desktop/Grocery/ObsidianNote",
      at: now - 120_000,
      meta: { job: "daily-library-sync" },
    },
    {
      id: `mock-${now}-2`,
      agent: "claude_code",
      kind: "session_completed",
      severity: "success",
      title: "翻译斑马贻贝基因组学论文摘要：会话已结束",
      detail: "12 次提问 · ObsidianNote · 输出 8431 tokens",
      project: "/Users/carpe/Desktop/Grocery/ObsidianNote/Research",
      at: now - 300_000,
      meta: {},
    },
    {
      id: `mock-${now}-3`,
      agent: "codex",
      kind: "session_completed",
      severity: "success",
      title: "运行 该项目中Fiberseq 数据的分析流程：会话已结束",
      detail: "8 轮 · Cyclone-Fiber-seq-analysis",
      project: null,
      at: now - 900_000,
      meta: {},
    },
  ];
  return {
    generated_at: now,
    summaries: [
      {
        agent: "hermes",
        label: "Hermes",
        detected: true,
        headline: "4 个任务 · 1 个异常 · 2 个暂停",
        healthy: 3,
        failing: 1,
        paused: 2,
        last_activity: now,
      },
      {
        agent: "codex",
        label: "Codex",
        detected: true,
        headline: "9 个会话 · 全部正常",
        healthy: 9,
        failing: 0,
        paused: 0,
        last_activity: now,
      },
      {
        agent: "claude_code",
        label: "Claude Code",
        detected: true,
        headline: "5 个会话 · 全部正常",
        healthy: 5,
        failing: 0,
        paused: 0,
        last_activity: now,
      },
    ],
    events: samples,
  };
}

void boot();
