// SPDX-License-Identifier: MIT OR Apache-2.0
// DreamAVB companion UI logic. Talks to the Rust backend via Tauri's invoke.
const invoke = window.__TAURI__.core.invoke;

const el = (id) => document.getElementById(id);
let currentNics = [];
let busy = false;
// Driver state the NIC list was last built for. Our dext claiming or
// releasing the I210 removes or restores its enX interface, so the list
// genuinely differs between states and must be rebuilt when one changes.
let nicListState = null;

async function loadNics() {
  const nics = await invoke("list_nics");
  const sel = el("nic");
  const prev = sel.value;
  currentNics = nics;
  sel.innerHTML = "";
  for (const n of nics) {
    const o = document.createElement("option");
    o.value = n.bsd_name;
    // With our dext owning the I210 there is no enX interface, so the ioreg
    // fallback supplies no BSD name — don't render a dangling separator.
    o.textContent = n.bsd_name ? `${n.display} · ${n.bsd_name}` : n.display;
    o.disabled = !n.supported;
    sel.appendChild(o);
  }
  // Rebuilding the <select> drops the selection; restore it when the same
  // NIC is still on offer, so a poll can't move the user's choice.
  if (prev && nics.some((n) => n.bsd_name === prev)) sel.value = prev;
}

/// Re-query the NIC list only when it could actually have changed: we have
/// nothing usable to show, or the driver state moved.
///
/// This used to run exactly once at startup, and `list_nics` depends on
/// `system_profiler` and `ioreg` — both of which can come up empty at login,
/// while the dext is still attaching or under a post-boot disk storm that
/// makes `system_profiler` crawl (it was measured timing out outright). With
/// no retry, an empty list persisted until the app was relaunched, which is
/// exactly what was observed. It stays cheap: once a supported NIC is listed
/// and the state is steady, no subprocess is spawned.
async function maybeReloadNics(state) {
  if (currentNics.some((n) => n.supported) && state === nicListState) return;
  nicListState = state;
  await loadNics();
}

function paintStatus(st) {
  const dot = el("dot");
  dot.className = "status-dot";
  const power = el("power");
  const main = el("stateMain");
  const sub = el("stateSub");
  const headline = el("headline");

  switch (st.state) {
    case "active":
      dot.classList.add("active");
      main.textContent = "Active";
      sub.textContent = st.summary || "DreamAVB owns this NIC";
      power.checked = true;
      break;
    case "pending":
      dot.classList.add("pending");
      main.textContent = "Activating…";
      sub.textContent = "Waiting for approval";
      power.checked = true;
      break;
    case "fault":
      dot.classList.add("fault");
      main.textContent = "Fault";
      sub.textContent = st.summary || "The driver reported a fault";
      power.checked = true;
      break;
    default:
      main.textContent = "Hands off";
      sub.textContent = "The OS default driver owns this NIC";
      power.checked = false;
  }
  headline.textContent = st.summary || sub.textContent;
}

async function refreshStatus() {
  try {
    const st = await invoke("status");
    paintStatus(st);
    // The status poll is where the driver state is known, so it is also
    // where a NIC-list rebuild can be decided.
    await maybeReloadNics(st.state);
  } catch (e) {
    console.error(e);
  }
}

function fmt(n) {
  return new Intl.NumberFormat().format(n);
}

/// Show a transient error in the status sub-line (no modal).
function showError(msg) {
  const sub = el("stateSub");
  sub.textContent = msg;
  sub.style.color = "#ef4444";
  setTimeout(() => {
    sub.style.color = "";
    refreshStatus();
  }, 6000);
}

async function refreshStats() {
  try {
    const s = await invoke("stats");
    const lock = el("s_ascap");
    lock.textContent = s.clock_locked ? "locked" : "unlocked";
    lock.className = "v " + (s.clock_locked ? "good" : "");
    el("s_pkts").textContent = `${fmt(s.stream_frames_per_s)}/s`;
    const drift = el("s_drift");
    drift.textContent = `${s.media_drift_ppm.toFixed(1)} ppm`;
    drift.className = "v " + (Math.abs(s.media_drift_ppm) < 100 ? "good" : "warn");
    el("s_under").textContent = fmt(s.zero_fill_frames);
    el("s_over").textContent = fmt(s.overrun_frames);
    el("s_rx").textContent = fmt(s.rx_frames);
    el("s_hw").textContent = s.hw_rev ? `rev ${s.hw_rev} · ABI ${s.abi_version}` : "—";
  } catch (e) {
    console.error(e);
  }
}

/// Render one get/set_clock_source response: build the options once, then sync
/// the selection, the in-force line and any config warnings.
///
/// The SELECTION and the clock IN FORCE are two different facts and are shown
/// as such. The selection is what is saved on disk; the in-force value is what
/// the daemon published, and with no daemon running there is no clock at all —
/// naming one would claim audio is being sourced when no stream exists.
let clockOptionsBuilt = false;
let clockApplying = false;

function paintClock(info) {
  if (!info) return;
  const sel = el("clockSource");
  if (!clockOptionsBuilt && Array.isArray(info.options)) {
    sel.innerHTML = "";
    for (const o of info.options) {
      const opt = document.createElement("option");
      opt.value = o.value;
      opt.textContent = o.label;
      if (o.description) opt.title = o.description;
      sel.appendChild(opt);
    }
    clockOptionsBuilt = true;
  }
  if (info.selected != null) sel.value = info.selected;

  const eff = el("clockEff");
  const txt = el("clockEffText");
  txt.textContent = `In force: ${info.effective_label} — ${info.effective_reason}`;
  // Not network-disciplined is the drift state: audio will pull away from the
  // 16R, so it reads as a warning rather than as normal.
  eff.classList.toggle("warn", info.network_disciplined === false);

  const warns = Array.isArray(info.warnings) ? info.warnings : [];
  if (warns.length) {
    txt.textContent += ` · ${warns.join("; ")}`;
    eff.classList.add("warn");
  }
}

async function refreshClock() {
  // Don't let a stale poll clobber the user's in-flight change.
  if (clockApplying) return;
  try {
    paintClock(await invoke("get_clock_source"));
  } catch (e) {
    console.error(e);
  }
}

el("clockSource").addEventListener("change", async (ev) => {
  clockApplying = true;
  ev.target.disabled = true;
  try {
    paintClock(await invoke("set_clock_source", { value: ev.target.value }));
  } catch (e) {
    // A rejected token (which the dropdown should never produce) or a save
    // failure: show it on the in-force line and re-sync from disk.
    el("clockEffText").textContent = String(e);
    el("clockEff").classList.add("warn");
    await refreshClock();
  } finally {
    ev.target.disabled = false;
    clockApplying = false;
  }
});

el("power").addEventListener("change", async (ev) => {
  if (busy) return;
  busy = true;
  const want = ev.target.checked;
  ev.target.disabled = true;
  try {
    if (want) {
      const nic = el("nic").value;
      paintStatus({ state: "pending", summary: "Activating…" });
      await invoke("enable_driver", { bsdName: nic });
    } else {
      await invoke("disable_driver");
    }
  } catch (e) {
    // Inline, not alert(): a modal blocks the webview and is wrong for a
    // menu-bar popover.
    showError(String(e));
  } finally {
    ev.target.disabled = false;
    busy = false;
    await refreshStatus();
  }
});

el("statsToggle").addEventListener("click", () => {
  el("statsToggle").classList.toggle("open");
  const open = el("stats").classList.toggle("open");
  if (open) refreshStats();
});

// --- Audio engine: the duplex daemon --------------------------------------
//
// One daemon does BOTH directions. It cannot be two processes: the driver
// allows a single client to own the hardware rings, so a second one is
// refused.

let audioBusy = false;

function paintAudio(info) {
  const dot = el("audioDot");
  const main = el("audioMain");
  const sub = el("audioSub");
  const box = el("audio");
  if (info.running) {
    dot.className = "status-dot ok";
    main.textContent = "Audio running";
    sub.textContent =
      "Bidirectional audio with the 16R" +
      (info.pid ? ` · daemon ${info.pid}` : "");
    if (!audioBusy) box.checked = true;
  } else {
    dot.className = "status-dot";
    main.textContent = "Audio stopped";
    // Distinguish "never started" from "started and then exited": the second
    // is a fault worth showing, the first is just the resting state.
    sub.textContent =
      info.reason && info.reason !== "not started"
        ? info.reason
        : "Start bidirectional audio with the 16R";
    if (!audioBusy) box.checked = false;
  }
}

async function refreshAudio() {
  try {
    paintAudio(await invoke("audio_status"));
  } catch (e) {
    console.error(e);
  }
}

el("audio").addEventListener("change", async (ev) => {
  if (audioBusy) return;
  audioBusy = true;
  const want = ev.target.checked;
  ev.target.disabled = true;
  try {
    paintAudio({
      running: want,
      reason: "",
      pid: null,
      log_path: "",
    });
    const info = want
      ? await invoke("start_audio")
      : await invoke("stop_audio");
    audioBusy = false;
    paintAudio(info);
  } catch (e) {
    // A refusal here is usually "turn the driver on first", which is
    // actionable, so show it rather than silently reverting the switch.
    showError(String(e));
    audioBusy = false;
    await refreshAudio();
  } finally {
    ev.target.disabled = false;
    audioBusy = false;
  }
});

el("link").addEventListener("click", () => {
  invoke("open_project_page");
});

// --- Preferences: launch at login + auto-select NIC ------------------------

/// The user has made a NIC choice this session (manually or via auto-select),
/// so later polls and a re-run of auto-select must not override it.
let nicChosen = false;

// A manual pick counts as a choice: never let auto-select or a poll move it
// afterwards.
el("nic").addEventListener("change", () => {
  nicChosen = true;
});

async function refreshLaunchAtLogin() {
  try {
    const info = await invoke("get_launch_at_login");
    el("launchAtLogin").checked = !!info.enabled;
    paintLaunchNote(info);
  } catch (e) {
    console.error(e);
  }
}

function paintLaunchNote(info) {
  const sub = el("launchSub");
  if (info && info.note) {
    sub.textContent = info.note;
    sub.style.color = "var(--warn)";
  } else if (info && info.enabled && info.plist_present === false) {
    sub.textContent = "Enabled, but the login item could not be installed";
    sub.style.color = "var(--warn)";
  } else {
    sub.textContent = "Start DreamAVB automatically when you log in";
    sub.style.color = "";
  }
}

el("launchAtLogin").addEventListener("change", async (ev) => {
  const want = ev.target.checked;
  ev.target.disabled = true;
  try {
    const info = await invoke("set_launch_at_login", { enabled: want });
    paintLaunchNote(info);
    // Reflect what actually happened (e.g. a failed load leaves it enabled in
    // config but with a note), not just the click.
    el("launchAtLogin").checked = !!info.enabled;
  } catch (e) {
    el("launchSub").textContent = String(e);
    el("launchSub").style.color = "#ef4444";
    await refreshLaunchAtLogin();
  } finally {
    ev.target.disabled = false;
  }
});

/// Apply an auto-select result to the dropdown. SELECTS ONLY — it sets the
/// <select> value and never touches the power toggle / enable_driver. Respects
/// a choice the user already made this session.
function applyAutoSelect(info) {
  if (!info || !info.enabled) {
    el("autoSelectSub").textContent =
      "Pick the supported NIC automatically on launch";
    el("autoSelectSub").style.color = "";
    return;
  }
  if (info.selected_bsd != null) {
    // Only move the dropdown if the user hasn't already chosen, and the NIC is
    // actually on offer (empty bsd = our dext owns it; match that too).
    const sel = el("nic");
    const has = Array.from(sel.options).some((o) => o.value === info.selected_bsd);
    if (!nicChosen && has) {
      sel.value = info.selected_bsd;
      nicChosen = true;
    }
    el("autoSelectSub").textContent = "Pick the supported NIC automatically on launch";
    el("autoSelectSub").style.color = "";
  } else {
    // Nothing selected: surface why (none / ambiguous), never guess.
    el("autoSelectSub").textContent = info.reason || "No NIC auto-selected";
    el("autoSelectSub").style.color = "var(--warn)";
  }
}

async function refreshAutoSelectSetting() {
  try {
    el("autoSelectNic").checked = !!(await invoke("get_auto_select_nic"));
  } catch (e) {
    console.error(e);
  }
}

el("autoSelectNic").addEventListener("change", async (ev) => {
  const want = ev.target.checked;
  ev.target.disabled = true;
  try {
    const info = await invoke("set_auto_select_nic", { enabled: want });
    // Turning it on now should pick immediately if unambiguous; turning it off
    // leaves the current selection untouched.
    if (want) applyAutoSelect(info);
    else {
      el("autoSelectSub").textContent =
        "Pick the supported NIC automatically on launch";
      el("autoSelectSub").style.color = "";
    }
  } catch (e) {
    el("autoSelectSub").textContent = String(e);
    el("autoSelectSub").style.color = "#ef4444";
    await refreshAutoSelectSetting();
  } finally {
    ev.target.disabled = false;
  }
});

// Boot.
(async () => {
  // No separate loadNics() here: refreshStatus owns the NIC list, so the
  // first population and every later retry go down one path rather than two
  // that can disagree.
  await refreshStatus();
  await refreshClock();
  await refreshLaunchAtLogin();
  await refreshAutoSelectSetting();
  await refreshAudio();
  // Auto-select runs ONCE at startup, after the first NIC list is populated.
  // It only selects (never enables the driver) and only when unambiguous; it
  // defers to a choice the user already made this session.
  try {
    applyAutoSelect(await invoke("eval_auto_select_nic"));
  } catch (e) {
    console.error(e);
  }
  // One 2 s poll drives both the status and the clock line — the clock's
  // in-force value changes when the daemon starts or a stream locks, which is
  // the same cadence. Stats poll faster, but only while that panel is open.
  setInterval(() => {
    refreshStatus();
    refreshClock();
    refreshAudio();
  }, 2000);
  setInterval(() => {
    if (el("stats").classList.contains("open")) refreshStats();
  }, 1000);
})();
