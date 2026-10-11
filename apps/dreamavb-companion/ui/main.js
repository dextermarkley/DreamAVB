// SPDX-License-Identifier: MIT OR Apache-2.0
// DreamAVB companion UI logic. Talks to the Rust backend via Tauri's invoke.
const invoke = window.__TAURI__.core.invoke;

const el = (id) => document.getElementById(id);
let currentNics = [];
let busy = false;

async function loadNics() {
  currentNics = await invoke("list_nics");
  const sel = el("nic");
  sel.innerHTML = "";
  for (const n of currentNics) {
    const o = document.createElement("option");
    o.value = n.bsd_name;
    o.textContent = `${n.display} · ${n.bsd_name}`;
    o.disabled = !n.supported;
    sel.appendChild(o);
  }
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
    paintStatus(await invoke("status"));
  } catch (e) {
    console.error(e);
  }
}

function fmt(n) {
  return new Intl.NumberFormat().format(n);
}

async function refreshStats() {
  try {
    const s = await invoke("stats");
    const lock = el("s_ascap");
    lock.textContent = s.as_capable ? "locked" : "unlocked";
    lock.className = "v " + (s.as_capable ? "good" : "");
    el("s_pkts").textContent = `${fmt(s.stream_pkts_per_s)}/s`;
    const drift = el("s_drift");
    drift.textContent = `${s.media_drift_ppm.toFixed(1)} ppm`;
    drift.className = "v " + (Math.abs(s.media_drift_ppm) < 100 ? "good" : "warn");
    el("s_under").textContent = fmt(s.ring_underruns);
    el("s_over").textContent = fmt(s.ring_overruns);
    el("s_rx").textContent = fmt(s.rx_frames);
    el("s_hw").textContent = s.hw_rev ? `rev ${s.hw_rev} · ABI ${s.abi_version}` : "—";
  } catch (e) {
    console.error(e);
  }
}

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
    alert(`DreamAVB: ${e}`);
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

el("link").addEventListener("click", () => {
  if (window.__TAURI__.shell) {
    window.__TAURI__.shell.open("https://github.com/dextermarkley/DreamAVB");
  }
});

// Boot.
(async () => {
  await loadNics();
  await refreshStatus();
  // Poll status every 2s, and stats every 1s while the panel is open.
  setInterval(refreshStatus, 2000);
  setInterval(() => {
    if (el("stats").classList.contains("open")) refreshStats();
  }, 1000);
})();
