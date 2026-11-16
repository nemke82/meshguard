import jsQR from "jsqr";

if (!window.__TAURI__ || !window.__TAURI__.core) {
  document.body.innerHTML =
    '<div style="color:red;padding:2em;font-family:monospace;">ERROR: Tauri bridge not loaded.</div>';
  throw new Error("Tauri bridge not available");
}

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

function invokeWithTimeout(cmd, args, ms = 30000) {
  return Promise.race([
    invoke(cmd, args),
    new Promise((_, reject) =>
      setTimeout(
        () => reject(new Error(`Command timed out after ${ms / 1000}s: ${cmd}`)),
        ms
      )
    ),
  ]);
}

// ============================================================
// Notification Sound Engine (Web Audio API)
// ============================================================
const NotificationSound = (() => {
  let audioCtx = null;
  let customSoundBuffer = null;
  let settings = {
    enabled: true,
    volume: 0.7,
    tone: "default",
  };

  function getCtx() {
    if (!audioCtx) audioCtx = new (window.AudioContext || window.webkitAudioContext)();
    if (audioCtx.state === "suspended") audioCtx.resume();
    return audioCtx;
  }

  function playNote(ctx, freq, startTime, duration, gain, type = "sine") {
    const osc = ctx.createOscillator();
    const vol = ctx.createGain();
    osc.type = type;
    osc.frequency.setValueAtTime(freq, startTime);
    vol.gain.setValueAtTime(gain, startTime);
    vol.gain.exponentialRampToValueAtTime(0.001, startTime + duration);
    osc.connect(vol).connect(ctx.destination);
    osc.start(startTime);
    osc.stop(startTime + duration);
  }

  const tones = {
    default(ctx, vol) {
      const t = ctx.currentTime;
      playNote(ctx, 880, t, 0.12, vol * 0.4);
      playNote(ctx, 1108.73, t + 0.12, 0.12, vol * 0.5);
      playNote(ctx, 1318.51, t + 0.24, 0.2, vol * 0.3);
    },
    beacon(ctx, vol) {
      const t = ctx.currentTime;
      playNote(ctx, 523.25, t, 0.08, vol * 0.3, "triangle");
      playNote(ctx, 659.25, t + 0.1, 0.08, vol * 0.4, "triangle");
      playNote(ctx, 783.99, t + 0.2, 0.15, vol * 0.35, "triangle");
      playNote(ctx, 1046.5, t + 0.35, 0.25, vol * 0.25, "triangle");
    },
    chirp(ctx, vol) {
      const t = ctx.currentTime;
      const osc = ctx.createOscillator();
      const g = ctx.createGain();
      osc.type = "sine";
      osc.frequency.setValueAtTime(1200, t);
      osc.frequency.exponentialRampToValueAtTime(600, t + 0.15);
      g.gain.setValueAtTime(vol * 0.5, t);
      g.gain.exponentialRampToValueAtTime(0.001, t + 0.15);
      osc.connect(g).connect(ctx.destination);
      osc.start(t);
      osc.stop(t + 0.15);
    },
  };

  function play() {
    if (!settings.enabled) return;
    const ctx = getCtx();

    if (settings.tone === "custom" && customSoundBuffer) {
      const src = ctx.createBufferSource();
      const g = ctx.createGain();
      src.buffer = customSoundBuffer;
      g.gain.value = settings.volume;
      src.connect(g).connect(ctx.destination);
      src.start();
      return;
    }

    const fn = tones[settings.tone] || tones.default;
    fn(ctx, settings.volume);
  }

  async function loadCustomSound(file) {
    if (!file || file.size > 512000) return false;
    try {
      const ctx = getCtx();
      const buf = await file.arrayBuffer();
      customSoundBuffer = await ctx.decodeAudioData(buf);
      return true;
    } catch {
      return false;
    }
  }

  function loadSettings() {
    try {
      const saved = localStorage.getItem("meshguard_notif");
      if (saved) Object.assign(settings, JSON.parse(saved));
    } catch {}
  }

  function saveSettings() {
    localStorage.setItem("meshguard_notif", JSON.stringify(settings));
  }

  loadSettings();

  return { play, loadCustomSound, settings, saveSettings };
})();

// ============================================================
// Toast Popup System
// ============================================================
const Toast = (() => {
  const container = () => document.getElementById("toast-container");

  function show({ sender, text, onClick, duration = 5000 }) {
    const el = document.createElement("div");
    el.className = "toast";
    const initial = (sender || "?")[0].toUpperCase();
    const time = new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    el.innerHTML = `
      <div class="toast-icon">${initial}</div>
      <div class="toast-body">
        <div class="toast-sender">${escapeHtml(sender || "Unknown")}</div>
        <div class="toast-text">${escapeHtml(text)}</div>
      </div>
      <div class="toast-time">${time}</div>
    `;
    if (onClick) {
      el.addEventListener("click", () => {
        dismiss(el);
        onClick();
      });
    }
    container().appendChild(el);

    const maxToasts = 4;
    const toasts = container().querySelectorAll(".toast:not(.toast-out)");
    if (toasts.length > maxToasts) dismiss(toasts[0]);

    setTimeout(() => dismiss(el), duration);
    return el;
  }

  function dismiss(el) {
    if (!el || el.classList.contains("toast-out")) return;
    el.classList.add("toast-out");
    el.addEventListener("animationend", () => el.remove());
  }

  return { show };
})();

// ============================================================
// DOM
// ============================================================
const $ = (sel) => document.querySelector(sel);

const screenConnect = $("#screen-connect");
const screenMesh = $("#screen-mesh");
const screenPassphrase = $("#screen-passphrase");
const screenChat = $("#screen-chat");

const btnScan = $("#btn-scan");
const scanStatus = $("#scan-status");
const scanResults = $("#scan-results");
const connectProgress = $("#connect-progress");
const linkStatus = $("#link-status");
const linkStatusText = $("#link-status-text");
const btnLinkStop = $("#btn-link-stop");
const connectProgressText = $("#connect-progress-text");

const blePin = $("#ble-pin");
const wifiAddress = $("#wifi-address");
const btnConnectWifi = $("#btn-connect-wifi");
const btnScanWifi = $("#btn-scan-wifi");
const wifiScanStatus = $("#wifi-scan-status");
const wifiScanResults = $("#wifi-scan-results");
const savedLinks = $("#saved-links");
const savedLinksList = $("#saved-links-list");
const savedLinksStatus = $("#saved-links-status");
const connectTitle = $("#connect-title");
const serialPort = $("#serial-port");
const btnRefreshPorts = $("#btn-refresh-ports");
const btnConnectSerial = $("#btn-connect-serial");

const btnDisconnect = $("#btn-disconnect");
const btnRefreshNodes = $("#btn-refresh-nodes");
const myDeviceLabel = $("#my-device-label");
const meshNodesList = $("#mesh-nodes-list");
const btnToggleNodes = $("#btn-toggle-nodes");
const nodesPanel = $("#nodes-panel");
const nodesSearch = $("#nodes-search");
const nodesCount = $("#nodes-count");
const nodesMore = $("#nodes-more");
const activeChatsSection = $("#active-chats-section");
const activeChatsList = $("#active-chats-list");

const pairRequestBanner = $("#pair-request-banner");
const pairRequestText = $("#pair-request-text");
const btnAcceptPair = $("#btn-accept-pair");

const formPassphrase = $("#form-passphrase");
const passphraseInput = $("#passphrase-input");
const passphraseTitle = $("#passphrase-title");
const passphraseSubtitle = $("#passphrase-subtitle");
const btnBackMesh = $("#btn-back-mesh");
const btnSubmitPassphrase = $("#btn-submit-passphrase");
const v2PairingField = $("#v2-pairing-field");
const v2PairingHint = $("#v2-pairing-hint");
const v2Pairing = $("#v2-pairing");

// Largest text that fits one LoRa frame, in UTF-8 bytes (v2 envelope with
// the default 228-byte budget; v1 allows 199, so this is safe for both).
const MAX_TEXT_BYTES = 173;
const utf8 = new TextEncoder();
const byteLength = (s) => utf8.encode(s).length;

// Peer ACKs whose message has not been matched yet. An ACK can arrive before
// invoke("send_message") resolves with the wire id, so it is kept here and
// reconciled when the receipt comes back.
const unmatchedDelivered = new Set();

/** The v2 toggle only applies when we initiate pairing. */
function setPairingModeUi(initiating) {
  const display = initiating ? "" : "none";
  v2PairingField.style.display = display;
  v2PairingHint.style.display = display;
}

const btnBackMeshChat = $("#btn-back-mesh-chat");
const btnSend = $("#btn-send");
const messageInput = $("#message-input");
const charCount = $("#char-count");
const messagesContainer = $("#messages");
const chatPeerName = $("#chat-peer-name");
const historyBanner = $("#history-banner");
const btnDeleteHistory = $("#btn-delete-history");
const chatReadonlyNote = $("#chat-readonly-note");

// ============================================================
// State
// ============================================================
const state = {
  meshNodes: [],
  peers: [],
  messages: {},
  activePeerNodeNum: null,
  myNodeNum: null,
  // Message history (ADR-005): saved on this device, or memory only.
  history: { saved: false, reason: null },
  conversations: [],
  chat: { conversationId: null, cursor: null, hasMore: false, readOnly: false, gen: 0 },
  // Delivery updates seen while history pages load (one buffer per load).
  lateBuffers: new Set(),
  passphraseMode: null, // "initiate" or "accept"
  passphraseTargetNode: null,
  pendingPairRequests: [],
  bannerPairRequest: null,
};

// ============================================================
// Screen Navigation
// ============================================================
function showScreen(screen) {
  // Leaving the chat: whatever it was still loading is no longer wanted.
  if (screen !== screenChat) state.chat.gen += 1;
  document.querySelectorAll(".screen").forEach((s) => s.classList.remove("active"));
  screen.classList.add("active");
  if (screen === screenConnect) loadSavedLinks();
  // The node list is not redrawn while another screen is shown.
  if (screen === screenMesh && !nodesPanel.hidden) renderMeshNodes();
}

// ============================================================
// Connection Type Tabs
// ============================================================
document.querySelectorAll(".conn-tab").forEach((tab) => {
  tab.addEventListener("click", () => {
    document.querySelectorAll(".conn-tab").forEach((t) => t.classList.remove("active"));
    document.querySelectorAll(".conn-tab-content").forEach((c) => c.classList.remove("active"));
    tab.classList.add("active");
    const target = document.getElementById("tab-" + tab.dataset.tab);
    if (target) target.classList.add("active");

    if (tab.dataset.tab === "usb") loadSerialPorts();
  });
});

// ============================================================
// BLE Scanning + Connection
// ============================================================
btnScan.addEventListener("click", async () => {
  btnScan.disabled = true;
  btnScan.innerHTML = '<span class="spinner"></span> Scanning (5s)...';
  scanResults.innerHTML = "";
  scanStatus.textContent = "";
  setScanStatusType("info");

  try {
    const result = await invokeWithTimeout("scan_ble_devices", undefined, 20000);
    const devices = result.devices || [];

    if (devices.length === 0) {
      setScanStatusType("warning");
      scanStatus.innerHTML =
        "<strong>No Meshtastic devices found.</strong><br>Make sure your device is powered on and within Bluetooth range.";
    } else {
      setScanStatusType("success");
      scanStatus.textContent = `Found ${devices.length} device${devices.length > 1 ? "s" : ""} — tap to connect`;
      devices.forEach(renderBleDevice);
    }
  } catch (err) {
    setScanStatusType("error");
    scanStatus.innerHTML = `<strong>Scan failed.</strong><br>${escapeHtml(String(err))}`;
  }

  btnScan.disabled = false;
  btnScan.innerHTML = `
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      <circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>
    </svg>
    Scan Again`;
});

function setScanStatusType(type) {
  scanStatus.className = "scan-status";
  if (type) scanStatus.classList.add(`scan-status-${type}`);
}

function renderBleDevice(device) {
  const card = document.createElement("div");
  card.className = "scan-card meshtastic";
  card.innerHTML = `
    <div class="scan-card-icon">&#x1F4E1;</div>
    <div class="scan-card-details">
      <div class="scan-card-name">${escapeHtml(device.name)}</div>
      <div class="scan-card-addr">${escapeHtml(device.address)}</div>
    </div>
    <div class="scan-card-meta">
      <span class="scan-badge">Meshtastic</span>
    </div>
  `;
  card.addEventListener("click", () => connectToDevice(device));
  scanResults.appendChild(card);
}

async function connectToDevice(device) {
  scanResults.innerHTML = "";
  scanStatus.textContent = "";
  hideLinkStatus();
  connectProgress.style.display = "flex";
  connectProgressText.textContent = "Connecting to " + device.name + "...";
  btnScan.disabled = true;

  const pin = parseInt(blePin.value, 10) || 123456;

  try {
    await invokeWithTimeout(
      "connect_device",
      { bleName: device.name, bleAddress: device.address, pin },
      90000
    );
    await loadMeshData();
    showScreen(screenMesh);
  } catch (err) {
    connectProgress.style.display = "none";
    setScanStatusType("error");
    scanStatus.innerHTML = `<strong>Connection failed.</strong><br>${escapeHtml(String(err))}`;
    btnScan.disabled = false;
  }

  connectProgress.style.display = "none";
}

// ============================================================
// WiFi Connection
// ============================================================
function setStatus(el, type, html) {
  el.className = "scan-status";
  if (type) el.classList.add(`scan-status-${type}`);
  el.innerHTML = html;
}

/** Shared connect flow: progress, then the mesh screen or an error. */
async function runConnect(label, cmd, args, timeoutMs, statusEl) {
  hideLinkStatus();
  setStatus(statusEl, null, "");
  connectProgress.style.display = "flex";
  connectProgressText.textContent = `Connecting to ${label}...`;
  try {
    await invokeWithTimeout(cmd, args, timeoutMs);
    await loadMeshData();
    showScreen(screenMesh);
  } catch (err) {
    setStatus(statusEl, "error", `<strong>Connection failed.</strong>${escapeHtml(String(err))}`);
  } finally {
    connectProgress.style.display = "none";
  }
}

let savedLinkIds = new Set();

btnScanWifi.addEventListener("click", async () => {
  btnScanWifi.disabled = true;
  btnScanWifi.innerHTML = '<span class="spinner"></span> Scanning network...';
  wifiScanResults.innerHTML = "";
  setStatus(wifiScanStatus, "info", "");

  try {
    const radios = await invokeWithTimeout("scan_wifi_radios", {}, 20000);
    if (radios.length === 0) {
      setStatus(
        wifiScanStatus,
        "warning",
        "<strong>No radios found on this network.</strong>Check that the radio has WiFi enabled and is on the same network as this device, or enter its address below."
      );
    } else {
      setStatus(
        wifiScanStatus,
        "success",
        `Found ${radios.length} radio${radios.length > 1 ? "s" : ""} — tap to connect`
      );
      radios.forEach(renderWifiRadio);
    }
  } catch (err) {
    setStatus(wifiScanStatus, "error", `<strong>Scan failed.</strong>${escapeHtml(String(err))}`);
  }

  btnScanWifi.disabled = false;
  btnScanWifi.innerHTML = `
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      <circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>
    </svg>
    Scan Again`;
});

function nodeId(num) {
  return "!" + (num >>> 0).toString(16).padStart(8, "0");
}

function renderWifiRadio(radio) {
  const saved = radio.node_num != null && savedLinkIds.has("tcp:" + nodeId(radio.node_num));
  const title = radio.short_name ? `${radio.name} (${radio.short_name})` : radio.name;
  const card = document.createElement("div");
  card.className = "scan-card meshtastic";
  card.innerHTML = `
    <div class="scan-card-icon">&#x1F4F6;</div>
    <div class="scan-card-details">
      <div class="scan-card-name">${escapeHtml(title)}</div>
      <div class="scan-card-addr">${escapeHtml(radio.address)}</div>
    </div>
    <div class="scan-card-meta">
      <span class="scan-badge">${saved ? "Saved" : "WiFi"}</span>
    </div>
  `;
  card.addEventListener("click", () => {
    wifiScanResults.innerHTML = "";
    runConnect(title, "connect_tcp", { address: radio.address }, 30000, wifiScanStatus);
  });
  wifiScanResults.appendChild(card);
}

btnConnectWifi.addEventListener("click", async () => {
  let addr = wifiAddress.value.trim();
  if (!addr) {
    setStatus(wifiScanStatus, "error", "Enter the radio's IP address.");
    return;
  }
  // Add the default port unless one is given (IPv6 needs brackets for that).
  const hasPort = addr.startsWith("[") ? /\]:\d+$/.test(addr) : /^[^:]+:\d+$/.test(addr);
  if (!hasPort) addr = addr.includes(":") && !addr.startsWith("[") ? `[${addr}]:4403` : `${addr}:4403`;

  btnConnectWifi.disabled = true;
  await runConnect(addr, "connect_tcp", { address: addr }, 30000, wifiScanStatus);
  btnConnectWifi.disabled = false;
});

// ============================================================
// Saved radios
// ============================================================
const LINK_KIND = {
  ble: { icon: "&#x1F4E1;", label: "Bluetooth" },
  tcp: { icon: "&#x1F4F6;", label: "WiFi" },
  serial: { icon: "&#x1F50C;", label: "USB" },
};

function timeAgo(unixSecs) {
  if (!unixSecs) return "";
  const s = Math.max(0, Date.now() / 1000 - unixSecs);
  if (s < 90) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}

function linkAddress(target) {
  return target.address || target.port || target.name || "";
}

async function loadSavedLinks() {
  let links = [];
  try {
    links = await invoke("get_saved_links");
  } catch (err) {
    console.error("Could not load saved radios:", err);
  }
  savedLinkIds = new Set(links.map((l) => l.id));
  savedLinksList.innerHTML = "";
  savedLinks.style.display = links.length ? "block" : "none";
  connectTitle.textContent = links.length ? "Connect Another Device" : "Connect Your Device";
  links.forEach(renderSavedLink);
}

function renderSavedLink(link) {
  const kind = LINK_KIND[link.target.kind] || { icon: "&#x1F4E1;", label: link.target.kind };
  const when = timeAgo(link.last_connected);
  const card = document.createElement("div");
  card.className = "scan-card meshtastic";
  card.innerHTML = `
    <div class="scan-card-icon">${kind.icon}</div>
    <div class="scan-card-details">
      <div class="scan-card-name">${escapeHtml(link.name)}</div>
      <div class="scan-card-addr">${escapeHtml(kind.label)} · ${escapeHtml(linkAddress(link.target))}${when ? " · " + escapeHtml(when) : ""}</div>
    </div>
    <button type="button" class="saved-link-forget" aria-label="Forget ${escapeHtml(link.name)}" title="Forget">&times;</button>
  `;
  card.querySelector(".saved-link-forget").addEventListener("click", async (e) => {
    e.stopPropagation();
    if (!confirm(`Forget ${link.name}? MeshGuard will stop connecting to it on start.`)) return;
    try {
      await invoke("forget_saved_link", { id: link.id });
    } catch (err) {
      setStatus(savedLinksStatus, "error", escapeHtml(String(err)));
    }
    loadSavedLinks();
  });
  card.addEventListener("click", () => {
    const pin = parseInt(blePin.value, 10) || 123456;
    // Bluetooth may need pairing; a moved WiFi radio is looked up first.
    const timeout = link.target.kind === "ble" ? 90000 : 40000;
    runConnect(link.name, "connect_saved_link", { id: link.id, pin }, timeout, savedLinksStatus);
  });
  savedLinksList.appendChild(card);
}

// ============================================================
// USB Serial Connection
// ============================================================
async function loadSerialPorts() {
  try {
    const ports = await invoke("get_serial_ports");
    serialPort.innerHTML = "";
    if (ports.length === 0) {
      serialPort.innerHTML = '<option value="">No serial ports found</option>';
    } else {
      for (const p of ports) {
        const opt = document.createElement("option");
        opt.value = p.name;
        opt.textContent = p.name;
        serialPort.appendChild(opt);
      }
    }
  } catch (err) {
    serialPort.innerHTML = '<option value="">Error listing ports</option>';
  }
}

btnRefreshPorts.addEventListener("click", loadSerialPorts);

btnConnectSerial.addEventListener("click", async () => {
  const port = serialPort.value;
  if (!port) { alert("Select a serial port"); return; }

  connectProgress.style.display = "flex";
  connectProgressText.textContent = "Connecting via USB to " + port + "...";
  btnConnectSerial.disabled = true;

  try {
    await invokeWithTimeout("connect_serial", { portName: port }, 30000);
    await loadMeshData();
    showScreen(screenMesh);
  } catch (err) {
    connectProgress.style.display = "none";
    alert("USB connection failed: " + err);
  }

  connectProgress.style.display = "none";
  btnConnectSerial.disabled = false;
});

// ============================================================
// Mesh Nodes Screen
// ============================================================
async function loadMeshData() {
  try {
    const [nodes, deviceInfo, peers] = await Promise.all([
      invoke("get_mesh_nodes"),
      invoke("get_my_device_info"),
      invoke("list_peers"),
      loadConversations(),
    ]);

    state.meshNodes = nodes || [];
    state.peers = peers || [];

    if (deviceInfo) {
      // Another radio means other conversations: drop this session's bubbles.
      if (state.myNodeNum != null && state.myNodeNum !== deviceInfo.node_num) state.messages = {};
      state.myNodeNum = deviceInfo.node_num;
      myDeviceLabel.textContent = deviceInfo.device_name + " (connected)";
    }

    renderMeshNodes();
    renderActiveChats();
  } catch (err) {
    console.error("Failed to load mesh data:", err);
  }
}

// The node finder is closed by default: a busy mesh lists hundreds of nodes,
// and rebuilding all of them on every packet made the app slow. When open,
// it shows at most NODE_RENDER_LIMIT matches.
const NODE_RENDER_LIMIT = 50;

function nodesOpen() {
  return !nodesPanel.hidden && screenMesh.classList.contains("active");
}

function setNodesOpen(open) {
  nodesPanel.hidden = !open;
  btnToggleNodes.setAttribute("aria-expanded", String(open));
  if (open) {
    renderMeshNodes();
  } else {
    // Free the cards; they are rebuilt from state when opened again.
    meshNodesList.replaceChildren();
    nodesSearch.value = "";
  }
}

btnToggleNodes.addEventListener("click", () => setNodesOpen(nodesPanel.hidden));

let nodesSearchTimer = null;
nodesSearch.addEventListener("input", () => {
  clearTimeout(nodesSearchTimer);
  nodesSearchTimer = setTimeout(renderMeshNodes, 150);
});

// One listener for all cards instead of two per card.
meshNodesList.addEventListener("click", (e) => {
  const card = e.target.closest(".mesh-node-card");
  if (!card) return;
  const nodeNum = Number(card.dataset.node);
  if (e.target.closest(".btn-node-info")) {
    openDiagnostics(nodeNum);
    return;
  }
  const node = state.meshNodes.find((n) => n.node_num === nodeNum);
  if (node) onNodeTap(node);
});

function nodeMatches(node, q) {
  if (!q) return true;
  const hex = hex8(node.node_num);
  const id = q.startsWith("!") ? q.slice(1) : q;
  return (
    (node.long_name || "").toLowerCase().includes(q) ||
    (node.short_name || "").toLowerCase().includes(q) ||
    (id.length > 0 && hex.includes(id))
  );
}

function renderNodesCount() {
  const n = state.meshNodes.length;
  const online = state.meshNodes.filter(isOnline).length;
  nodesCount.textContent = n
    ? `${n} node${n === 1 ? "" : "s"}, ${online} online`
    : "No nodes heard yet";
}

function renderMeshNodes() {
  renderNodesCount();
  if (!nodesOpen()) return;

  if (state.meshNodes.length === 0) {
    meshNodesList.innerHTML =
      '<div class="empty-state">No devices found on the mesh yet. Other Meshtastic devices will appear here as they are discovered.</div>';
    nodesMore.hidden = true;
    return;
  }

  const q = nodesSearch.value.trim().toLowerCase();
  const peerNums = new Set(state.peers.map((p) => p.node_num));
  const matches = state.meshNodes.filter((n) => nodeMatches(n, q));
  matches.sort((a, b) => b.last_heard - a.last_heard);
  const shown = matches.slice(0, NODE_RENDER_LIMIT);

  const frag = document.createDocumentFragment();
  for (const node of shown) {
    const isPeer = peerNums.has(node.node_num);
    const online = isOnline(node);
    const signal = formatSignal(node);
    const card = document.createElement("div");
    card.className = "mesh-node-card" + (online ? " online" : "");
    card.innerHTML = `
      <div class="node-avatar">${escapeHtml((node.short_name || "?")[0].toUpperCase())}</div>
      <div class="node-info">
        <div class="node-name">${escapeHtml(nodeDisplayName(node))}</div>
        <div class="node-meta">
          ${node.short_name ? `<span class="node-short">${escapeHtml(node.short_name)}</span>` : ""}
          ${node.hw_model && node.hw_model !== "Unset" ? `<span class="node-hw">${escapeHtml(node.hw_model)}</span>` : ""}
          <span class="node-heard">${formatLastHeard(node.last_heard)}</span>
        </div>
        ${signal ? `<div class="node-signal">${signal}</div>` : ""}
      </div>
      <div class="node-actions">
        <button type="button" class="btn-node-info" title="Diagnostics" aria-label="Diagnostics">i</button>
        ${online ? '<span class="online-dot" title="Heard in the last 2 hours"></span>' : '<span class="offline-dot"></span>'}
        ${isPeer ? '<span class="peer-badge">Paired</span>' : ""}
        ${isBlocked(node.node_num) ? '<span class="peer-badge blocked-badge">Blocked</span>' : ""}
      </div>
    `;
    card.dataset.node = String(node.node_num);
    frag.appendChild(card);
  }
  if (shown.length === 0) {
    const empty = document.createElement("div");
    empty.className = "empty-state";
    empty.textContent = "No node matches that search.";
    frag.appendChild(empty);
  }
  meshNodesList.replaceChildren(frag);

  const hidden = matches.length - shown.length;
  nodesMore.hidden = hidden <= 0;
  if (hidden > 0) {
    nodesMore.textContent = `Showing the ${shown.length} heard most recently. Search to find the other ${hidden}.`;
  }
}

// ============================================================
// Chats (message history, ADR-005)
// ============================================================
function hex8(n) {
  return (n >>> 0).toString(16).padStart(8, "0");
}

/** Store id of the chat with `peerNode` on the connected radio. */
function conversationIdFor(peerNode) {
  return state.myNodeNum == null ? null : `c:${hex8(state.myNodeNum)}:${hex8(peerNode)}`;
}

async function loadConversations() {
  try {
    const [info, convs] = await Promise.all([
      invoke("get_history_info"),
      invoke("list_conversations"),
    ]);
    state.history = info || { saved: false, reason: null };
    state.conversations = convs || [];
  } catch (err) {
    console.error("Could not load message history:", err);
    state.conversations = [];
  }
}

/** Paired peers and saved chats, newest activity first. */
function chatEntries() {
  const entries = [];
  const seen = new Set();
  const mine = state.conversations.filter(
    (c) => state.myNodeNum == null || c.my_node === state.myNodeNum
  );
  for (const peer of state.peers) {
    const conv = mine.find((c) => c.peer_node === peer.node_num) || null;
    if (conv) seen.add(conv.conversation_id);
    entries.push({ peer, conv, nodeNum: peer.node_num, title: peer.device_name });
  }
  // History kept after the contact was removed: readable and deletable.
  for (const conv of mine) {
    if (seen.has(conv.conversation_id)) continue;
    entries.push({ peer: null, conv, nodeNum: conv.peer_node, title: conv.title });
  }
  const at = (e) => (e.conv ? e.conv.last_message_at : 0);
  entries.sort((a, b) => at(b) - at(a) || a.title.localeCompare(b.title));
  return entries;
}

function chatPreview(conv) {
  if (!conv || conv.preview == null) return "";
  return (conv.preview_outgoing ? "You: " : "") + conv.preview;
}

function renderActiveChats() {
  const entries = chatEntries();
  if (entries.length === 0) {
    activeChatsSection.style.display = "none";
    return;
  }

  activeChatsSection.style.display = "";
  activeChatsList.innerHTML = "";

  for (const { peer, conv, nodeNum, title } of entries) {
    const card = document.createElement("div");
    card.className = "mesh-node-card peer-chat-card" + (peer ? "" : " history-only");
    const initial = escapeHtml((title || "?")[0].toUpperCase());
    const unread = conv ? conv.unread : 0;
    const preview = chatPreview(conv);
    const badge = peer
      ? '<span class="peer-badge">Encrypted Chat</span>'
      : '<span class="peer-badge history-badge">Not paired</span>';
    card.innerHTML = `
      <div class="node-avatar">${initial}</div>
      <div class="node-info">
        <div class="node-name">${escapeHtml(title)}</div>
        ${preview ? `<div class="chat-preview${unread ? " unread" : ""}">${escapeHtml(preview)}</div>` : ""}
        <div class="node-meta">
          ${badge}
          ${conv && conv.last_message_at ? `<span class="node-heard">${timeAgo(conv.last_message_at)}</span>` : ""}
        </div>
      </div>
      <div class="node-actions">
        ${unread ? `<span class="unread-badge" aria-label="${unread} unread">${unread > 99 ? "99+" : unread}</span>` : ""}
        ${peer ? `<button class="btn-remove-peer" title="Remove contact">&times;</button>` : ""}
      </div>
    `;
    card.addEventListener("click", (e) => {
      if (e.target.closest(".btn-remove-peer")) return;
      if (peer) openExistingChat(peer);
      else enterChat(nodeNum, title, { readOnly: true, conversationId: conv.conversation_id });
    });
    const btnRemove = card.querySelector(".btn-remove-peer");
    if (btnRemove) {
      btnRemove.addEventListener("click", async (e) => {
        e.stopPropagation();
        const keep = state.history.saved ? " Its message history stays until you delete it." : "";
        if (!confirm(`Remove contact "${peer.device_name}"?${keep}`)) return;
        await invoke("remove_peer", { peerNodeNum: peer.node_num });
        await loadMeshData();
      });
    }
    activeChatsList.appendChild(card);
  }
}

async function onNodeTap(node) {
  if (isBlocked(node.node_num)) {
    if (confirm(`${nodeDisplayName(node)} is blocked. Unblock it?`)) await unblockNode(node.node_num);
    return;
  }
  const existingPeer = state.peers.find((p) => p.node_num === node.node_num);
  if (existingPeer) {
    await openExistingChat(existingPeer);
    return;
  }

  state.passphraseMode = "initiate";
  state.passphraseTargetNode = node.node_num;
  passphraseTitle.textContent = "Start Secure Chat";
  passphraseSubtitle.textContent = `with ${node.long_name || node.user_name || "Node " + node.node_num}`;
  btnSubmitPassphrase.textContent = "Start Chat";
  setPairingModeUi(true);
  passphraseInput.value = "";
  showScreen(screenPassphrase);
}

async function openExistingChat(peer) {
  const hasKey = await invoke("has_session", { peerNodeNum: peer.node_num });
  if (hasKey) {
    enterChat(peer.node_num, peer.device_name);
    return;
  }

  state.passphraseMode = "initiate";
  state.passphraseTargetNode = peer.node_num;
  passphraseTitle.textContent = "Unlock Chat";
  passphraseSubtitle.textContent = `Enter passphrase for ${peer.device_name}`;
  btnSubmitPassphrase.textContent = "Unlock";
  setPairingModeUi(true);
  v2Pairing.checked = peer.protocol !== "v1";
  passphraseInput.value = "";
  showScreen(screenPassphrase);
}

btnDisconnect.addEventListener("click", async () => {
  if (!confirm("Disconnect from device?")) return;
  await invoke("disconnect_device");
  state.meshNodes = [];
  state.peers = [];
  showScreen(screenConnect);
});

btnRefreshNodes.addEventListener("click", async () => {
  await loadMeshData();
});

// ============================================================
// Passphrase Screen
// ============================================================
btnBackMesh.addEventListener("click", () => {
  showScreen(screenMesh);
  showPendingPairBanner();
});

function showPendingPairBanner() {
  if (state.pendingPairRequests.length === 0) {
    state.bannerPairRequest = null;
    pairRequestBanner.style.display = "none";
    return;
  }
  const req = state.pendingPairRequests[state.pendingPairRequests.length - 1];
  state.bannerPairRequest = req;
  pairRequestText.textContent = `Chat request from ${req.from_name || lookupNodeName(req.from_node)}`;
  pairRequestBanner.style.display = "flex";
}

formPassphrase.addEventListener("submit", async (e) => {
  e.preventDefault();
  const passphrase = passphraseInput.value;
  const nodeNum = state.passphraseTargetNode;
  const mode = state.passphraseMode;

  btnSubmitPassphrase.disabled = true;
  btnSubmitPassphrase.innerHTML = '<span class="spinner"></span> Deriving keys...';

  try {
    let peer;
    if (mode === "accept") {
      peer = await invokeWithTimeout("accept_chat", {
        peerNodeNum: nodeNum,
        passphrase,
      });
    } else {
      peer = await invokeWithTimeout("start_chat", {
        peerNodeNum: nodeNum,
        passphrase,
        protocol: v2Pairing.checked ? "v2" : "v1",
      });
    }

    passphraseInput.value = "";
    if (mode === "accept") {
      state.pendingPairRequests = state.pendingPairRequests.filter(
        (r) => r.from_node !== nodeNum
      );
    }
    state.peers = state.peers.filter((p) => p.node_num !== peer.node_num);
    state.peers.push(peer);
    enterChat(nodeNum, peer.device_name);
  } catch (err) {
    const errStr = String(err);
    if (errStr.includes("assphrase mismatch") || errStr.includes("ecryption")) {
      alert("Wrong passphrase — decryption failed. Make sure both sides use the same passphrase.");
    } else {
      alert("Error: " + errStr);
    }
  }

  btnSubmitPassphrase.disabled = false;
  btnSubmitPassphrase.textContent =
    state.passphraseMode === "accept" ? "Accept Chat" : "Start Chat";
});

// ============================================================
// Pair Request Handling
// ============================================================
$("#btn-block-pair").addEventListener("click", () => {
  const req = state.bannerPairRequest;
  if (req) blockNode(req.from_node, req.from_name || lookupNodeName(req.from_node));
});

btnAcceptPair.addEventListener("click", () => {
  // Accept exactly the request the banner shows.
  const req = state.bannerPairRequest;
  if (!req) return;
  state.passphraseMode = "accept";
  state.passphraseTargetNode = req.from_node;
  passphraseTitle.textContent = "Accept Chat Request";
  passphraseSubtitle.textContent = `from ${req.from_name || "Node " + req.from_node}`;
  btnSubmitPassphrase.textContent = "Accept Chat";
  setPairingModeUi(false);
  passphraseInput.value = "";
  pairRequestBanner.style.display = "none";
  showScreen(screenPassphrase);
});

// ============================================================
// Chat Screen
// ============================================================
function enterChat(nodeNum, peerName, opts = {}) {
  const readOnly = !!opts.readOnly;
  state.activePeerNodeNum = nodeNum;
  state.chat = {
    conversationId: opts.conversationId || conversationIdFor(nodeNum),
    cursor: null,
    hasMore: false,
    readOnly,
    gen: state.chat.gen + 1,
  };
  chatPeerName.textContent = peerName || "Peer";
  screenChat.classList.toggle("read-only", readOnly);
  chatReadonlyNote.hidden = !readOnly;
  renderHistoryBanner();
  showScreen(screenChat);
  loadChatMessages(nodeNum);
}

function renderHistoryBanner() {
  if (state.history.saved) {
    historyBanner.hidden = true;
    return;
  }
  const reason = state.history.reason;
  historyBanner.textContent =
    reason && /kept only/.test(reason)
      ? reason
      : `${reason || "Message history is not saved on this device."} Messages are kept only until the app closes.`;
  historyBanner.hidden = false;
}

btnBackMeshChat.addEventListener("click", async () => {
  await loadMeshData();
  showScreen(screenMesh);
});

const HISTORY_PAGE = 50;

/** A stored message (get_messages) as a chat bubble. */
function fromStored(m) {
  return {
    id: m.message_id,
    wireId: m.outgoing ? m.message_id : undefined,
    text: m.text != null ? m.text : m.status ? m.status.text : "",
    checkin: m.status || undefined,
    // Ours: when we wrote it; theirs: the sender's clock, as when it arrived.
    timestamp: (m.outgoing ? m.stored_at : m.created_at) * 1000,
    mine: m.outgoing,
    status: m.outgoing ? uiStatus(m.state) || "sent" : "delivered",
  };
}

function chatIntro() {
  messagesContainer.innerHTML = "";
  const more = document.createElement("button");
  more.type = "button";
  more.id = "btn-load-earlier";
  more.className = "btn-load-earlier";
  more.textContent = "Load earlier messages";
  more.hidden = true;
  more.addEventListener("click", loadEarlierMessages);
  messagesContainer.appendChild(more);
  addSystemMessage(
    state.chat.readOnly
      ? "Saved history. This contact is no longer paired."
      : "Secure session active. All messages are encrypted with AES-256-GCM."
  );
}

/** One page of saved messages, with delivery updates that arrived while it
 *  loaded applied on top. */
async function fetchPage(conversationId, before) {
  const late = new Map();
  state.lateBuffers.add(late);
  let rows;
  try {
    rows = await invoke("get_messages", { conversationId, before, limit: HISTORY_PAGE });
  } catch (err) {
    console.error("Could not load messages:", err);
    return { ok: false };
  } finally {
    state.lateBuffers.delete(late);
  }
  const messages = rows.map(fromStored);
  for (const m of messages) {
    if (!m.mine || m.status === "delivered") continue;
    if (unmatchedDelivered.delete(m.id)) m.status = "delivered";
    else if (late.has(m.id)) m.status = late.get(m.id);
  }
  return {
    ok: true,
    messages,
    cursor: rows.length ? rows[0].cursor : null,
    full: rows.length === HISTORY_PAGE,
  };
}

async function loadChatMessages(nodeNum) {
  chatIntro();
  const conversationId = state.chat.conversationId;
  if (!state.history.saved || !conversationId) {
    (state.messages[nodeNum] || []).forEach((m) => renderMessage(m));
    return;
  }
  const gen = state.chat.gen;
  const before = new Set(state.messages[nodeNum] || []);
  const pending = new Set([...before].filter((m) => m.mine && !m.wireId));
  const page = await fetchPage(conversationId, null);
  if (gen !== state.chat.gen) return; // left or opened another chat meanwhile
  const current = state.messages[nodeNum] || [];
  if (!page.ok) {
    // Show what this session has rather than an empty chat.
    chatIntro();
    current.forEach((m) => renderMessage(m));
    return;
  }
  const stored = page.messages;
  const ids = new Set(stored.map((m) => m.id));
  // The store has everything received or sent. From memory keep sends that
  // were still waiting for their id and whatever arrived while it loaded.
  const live = current.filter(
    (m) =>
      (!before.has(m) || pending.has(m)) &&
      !ids.has(m.id) &&
      !(m.wireId && ids.has(m.wireId))
  );
  state.messages[nodeNum] = stored.concat(live);
  chatIntro();
  messagesContainer.style.scrollBehavior = "auto"; // open at the newest, no animation
  state.messages[nodeNum].forEach((m) => renderMessage(m));
  messagesContainer.style.scrollBehavior = "";
  state.chat.cursor = page.cursor;
  state.chat.hasMore = page.full;
  $("#btn-load-earlier").hidden = !state.chat.hasMore;
  markActiveChatRead();
}

async function loadEarlierMessages() {
  const { conversationId, cursor, gen } = state.chat;
  const nodeNum = state.activePeerNodeNum;
  if (!cursor || !conversationId) return;
  const btn = $("#btn-load-earlier");
  btn.disabled = true;
  const page = await fetchPage(conversationId, cursor);
  if (gen !== state.chat.gen) return;
  btn.disabled = false;
  if (!page.ok) return;
  const known = new Set((state.messages[nodeNum] || []).map((m) => m.id));
  const older = page.messages.filter((m) => !known.has(m.id));
  state.messages[nodeNum] = older.concat(state.messages[nodeNum] || []);
  // Keep the view where it was while older bubbles go in above it.
  const fromBottom = messagesContainer.scrollHeight - messagesContainer.scrollTop;
  const anchor = btn.nextSibling.nextSibling; // after the intro line
  for (const m of older) messagesContainer.insertBefore(messageElement(m), anchor);
  if (page.cursor) state.chat.cursor = page.cursor;
  state.chat.hasMore = page.full;
  btn.hidden = !state.chat.hasMore;
  messagesContainer.style.scrollBehavior = "auto";
  messagesContainer.scrollTop = messagesContainer.scrollHeight - fromBottom;
  messagesContainer.style.scrollBehavior = "";
}

function otherRadio(conversationId, nodeNum) {
  const mine = conversationIdFor(nodeNum);
  return !!(conversationId && mine && conversationId !== mine);
}

function onActiveChat(nodeNum) {
  return state.activePeerNodeNum === nodeNum && screenChat.classList.contains("active");
}

function markActiveChatRead() {
  const id = state.chat.conversationId;
  if (!state.history.saved || !id || !screenChat.classList.contains("active")) return;
  invoke("mark_conversation_read", { conversationId: id }).catch(() => {});
  const conv = state.conversations.find((c) => c.conversation_id === id);
  if (conv) conv.unread = 0;
}

btnDeleteHistory.addEventListener("click", async () => {
  const nodeNum = state.activePeerNodeNum;
  const { conversationId, readOnly } = state.chat;
  if (nodeNum == null) return;
  const name = chatPeerName.textContent;
  const what = state.history.saved
    ? `Delete all messages with ${name} from this device? Unsent messages are dropped too. This cannot be undone.`
    : `Clear the messages with ${name}?`;
  if (!confirm(what)) return;
  // Drop any page still loading for this chat; it would bring them back.
  const gen = ++state.chat.gen;
  try {
    if (state.history.saved && conversationId) {
      await invoke("delete_conversation", { conversationId });
    }
  } catch (err) {
    alert("Could not delete the history: " + err);
    return;
  }
  delete state.messages[nodeNum];
  state.conversations = state.conversations.filter((c) => c.conversation_id !== conversationId);
  if (gen !== state.chat.gen) return; // the user moved on; leave that screen alone
  if (readOnly) {
    await loadMeshData();
    if (gen === state.chat.gen) showScreen(screenMesh);
    return;
  }
  state.chat.cursor = null;
  state.chat.hasMore = false;
  chatIntro();
});

messageInput.addEventListener("input", () => {
  const len = byteLength(messageInput.value);
  charCount.textContent = MAX_TEXT_BYTES - len;
  btnSend.disabled = messageInput.value.trim().length === 0 || len > MAX_TEXT_BYTES;
  messageInput.style.height = "auto";
  messageInput.style.height = Math.min(messageInput.scrollHeight, 100) + "px";
});

messageInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (!btnSend.disabled) sendMessage();
  }
});

btnSend.addEventListener("click", () => {
  if (!btnSend.disabled) sendMessage();
});

async function sendMessage() {
  const text = messageInput.value.trim();
  const nodeNum = state.activePeerNodeNum;
  if (!text || !nodeNum || state.chat.readOnly) return;

  const msg = {
    id: crypto.randomUUID(),
    text,
    timestamp: Date.now(),
    mine: true,
    status: "sending",
  };

  if (!state.messages[nodeNum]) state.messages[nodeNum] = [];
  state.messages[nodeNum].push(msg);
  renderMessage(msg);
  messageInput.value = "";
  messageInput.style.height = "auto";
  charCount.textContent = String(MAX_TEXT_BYTES);
  btnSend.disabled = true;

  try {
    // "sent" = handed to the local radio. Only an authenticated peer ACK
    // (v2 peers) turns it into "delivered", via the message-delivered event.
    const receipt = await invoke("send_message", { peerNodeNum: nodeNum, text });
    msg.wireId = receipt && receipt.message_id;
    // The history page loaded meanwhile and already shows the saved copy.
    const list = state.messages[nodeNum] || [];
    if (msg.wireId && list.some((m) => m !== msg && m.id === msg.wireId)) {
      state.messages[nodeNum] = list.filter((m) => m !== msg);
      const el = document.getElementById(`msg-${msg.id}`);
      if (el) el.remove();
      return;
    }
    if (msg.wireId && unmatchedDelivered.delete(msg.wireId)) {
      msg.status = "delivered";
      updateMessageStatus(msg.id, "delivered");
    } else if (msg.status === "sending") {
      // Saved and waiting in the outbox (radio away) stays "sending".
      msg.status = uiStatus(receipt && receipt.state) || "sent";
      updateMessageStatus(msg.id, msg.status);
    }
  } catch (err) {
    msg.status = "failed";
    updateMessageStatus(msg.id, "failed");
    addSystemMessage(`Not sent: ${err}`);
    console.error("Send failed:", err);
  }
}

function renderMessage(msg) {
  const el = messageElement(msg);
  messagesContainer.appendChild(el);
  messagesContainer.scrollTop = messagesContainer.scrollHeight;
}

function messageElement(msg) {
  const el = document.createElement("div");
  const urgent = msg.checkin && URGENT_KINDS.has(msg.checkin.kind);
  el.className = `message ${msg.mine ? "mine" : "peer"}${urgent ? " urgent" : ""}`;
  el.id = `msg-${msg.id}`;
  const body = msg.checkin ? renderCheckin(msg.checkin) : escapeHtml(msg.text);
  el.innerHTML = `
    <div class="msg-text">${body}</div>
    <div class="msg-meta">
      <span>${formatTime(msg.timestamp)}</span>
      ${msg.mine ? `<span class="msg-status" id="status-${msg.id}">${statusIcon(msg.status)}</span>` : ""}
    </div>
  `;
  wireMapLinks(el);
  return el;
}

function addSystemMessage(text) {
  const el = document.createElement("div");
  el.className = "system-message";
  el.textContent = text;
  messagesContainer.appendChild(el);
}

function updateMessageStatus(id, status) {
  const el = document.getElementById(`status-${id}`);
  if (el) el.textContent = statusIcon(status);
}

/** Backend delivery state (history outbox) -> the bubble's status. */
function uiStatus(state) {
  switch (state) {
    case "queued":
    case "retrying":
      return "sending";
    case "submitted":
    case "mesh_ack":
      return "sent";
    case "peer_ack":
    case "read":
      return "delivered";
    case "failed":
    case "expired":
      return "failed";
    default:
      return null;
  }
}

function statusIcon(status) {
  switch (status) {
    case "sending": return "\u23F3";
    case "sent": return "\u2713";
    case "delivered": return "\u2713\u2713";
    case "failed": return "\u26A0";
    default: return "";
  }
}

// ============================================================
// Tauri Event Listeners
// ============================================================

/** A saved incoming message: read if its chat is open, else the chat list
 *  shows it as unread. */
let chatListRefresh = null;
function noteIncoming(nodeNum, conversationId, onChatScreen) {
  if (onChatScreen) {
    if (conversationId && conversationId === state.chat.conversationId) markActiveChatRead();
    return;
  }
  if (!screenMesh.classList.contains("active")) return;
  // Several arrivals in a burst: one reload.
  clearTimeout(chatListRefresh);
  chatListRefresh = setTimeout(async () => {
    await loadConversations();
    renderActiveChats();
  }, 300);
}

async function setupEventListeners() {
  await listen("connection-state", (event) => {
    const s = event.payload;
    if (connectProgress.style.display !== "none") {
      const labels = {
        connecting: "Connecting to device...",
        configuring: "Loading mesh configuration...",
        connected: "Connected!",
        disconnected: "Disconnected",
      };
      connectProgressText.textContent = labels[s] || s;
    }

    if (s === "disconnected") {
      showScreen(screenConnect);
      connectProgress.style.display = "none";
      btnScan.disabled = false;
    }
  });

  // The radio dropped (battery, range, reboot): the backend retries on its
  // own. Stay on the current screen and show the status bar.
  await listen("reconnecting", (event) => {
    const { target, attempt, retry_in_secs } = event.payload;
    showLinkStatus(attempt <= 1 ? `Connecting to ${target}…` : `Waiting for ${target}…`);
    linkStatus.title = `Attempt ${attempt}, next retry in ${retry_in_secs}s`;
  });

  await listen("reconnect-stopped", (event) => {
    hideLinkStatus();
    showScreen(screenConnect);
    setScanStatusType("error");
    scanStatus.textContent = String(event.payload);
  });

  await listen("reconnected", async () => {
    hideLinkStatus();
    await loadMeshData();
    if (screenConnect.classList.contains("active")) showScreen(screenMesh);
  });

  await listen("mesh-nodes-updated", async () => {
    try {
      const nodes = await invoke("get_mesh_nodes");
      state.meshNodes = nodes || [];
      renderMeshNodes();
    } catch (_) {}
  });

  await listen("incoming-message", (event) => {
    const data = event.payload;
    const nodeNum = data.from_node;

    if (!state.messages[nodeNum]) state.messages[nodeNum] = [];

    const msg = {
      id: data.message_id || crypto.randomUUID(),
      text: data.text,
      timestamp: data.timestamp * 1000,
      mine: false,
      status: "delivered",
    };

    if (state.messages[nodeNum].some((m) => m.id === msg.id)) return;
    // Saved for a radio we are no longer on: in its history, not this chat.
    const here = !otherRadio(data.conversation_id, nodeNum);
    if (here) state.messages[nodeNum].push(msg);

    const onChatScreen = here && onActiveChat(nodeNum);
    if (onChatScreen) {
      renderMessage(msg);
    }
    if (here) noteIncoming(nodeNum, data.conversation_id, onChatScreen);

    NotificationSound.play();

    const senderName = lookupNodeName(nodeNum);
    Toast.show({
      sender: senderName,
      text: data.text,
      onClick: () => {
        if (!onChatScreen) {
          const peer = state.peers.find((p) => p.node_num === nodeNum);
          if (peer) enterChat(nodeNum, peer.device_name);
        }
      },
    });
  });

  await listen("incoming-status", (event) => {
    const data = event.payload;
    const nodeNum = data.from_node;
    const st = data.status;
    if (!state.messages[nodeNum]) state.messages[nodeNum] = [];
    const msg = {
      id: data.message_id || crypto.randomUUID(),
      text: st.text,
      checkin: st,
      timestamp: data.timestamp * 1000,
      mine: false,
      status: "delivered",
    };
    if (state.messages[nodeNum].some((m) => m.id === msg.id)) return;
    const here = !otherRadio(data.conversation_id, nodeNum);
    if (here) state.messages[nodeNum].push(msg);

    const onChatScreen = here && onActiveChat(nodeNum);
    if (onChatScreen) renderMessage(msg);
    if (here) noteIncoming(nodeNum, data.conversation_id, onChatScreen);

    const name = data.from_name || lookupNodeName(nodeNum);
    NotificationSound.play();
    if (URGENT_KINDS.has(st.kind)) {
      showSosAlert(nodeNum, name, st);
    } else {
      Toast.show({
        sender: name,
        text: `${KIND_ICONS[st.kind] || ""} ${st.text}`.trim(),
        onClick: () => openChatFor(nodeNum),
      });
    }
  });

  await listen("qr-pair-refused", (event) => {
    Toast.show({
      sender: "MeshGuard",
      text: `${lookupNodeName(event.payload)} is already a contact. Remove it first to pair it again with a code.`,
    });
  });

  await listen("qr-pair-failed", (event) => {
    Toast.show({ sender: "MeshGuard", text: `Pairing failed: ${event.payload}` });
  });

  await listen("peer-paired", async (event) => {
    const { name } = event.payload;
    if (qrOverlay.style.display !== "none") {
      const el = $("#qr-paired");
      el.textContent = (el.textContent ? el.textContent + ", " : "Paired with ") + name;
    }
    Toast.show({ sender: "MeshGuard", text: `Paired with ${name}` });
    NotificationSound.play();
    await loadMeshData();
  });

  await listen("path-test-result", (event) => {
    const r = event.payload;
    if (diag.node !== r.node || !diag.pending) return;
    diag.pending = false;
    clearTimeout(diag.timer);
    renderPathResult(r);
  });

  // Outbox progress: resent, timed out, expired, or confirmed.
  await listen("message-state", (event) => {
    const { message_id, state: st } = event.payload;
    const status = uiStatus(st);
    if (!status) return;
    for (const late of state.lateBuffers) {
      if (late.get(message_id) !== "delivered") late.set(message_id, status);
    }
    for (const list of Object.values(state.messages)) {
      const msg = list.find((m) => m.mine && m.wireId === message_id);
      if (msg) {
        // Never step back from "delivered".
        if (msg.status !== "delivered") {
          msg.status = status;
          updateMessageStatus(msg.id, status);
        }
        return;
      }
    }
  });

  await listen("message-delivered", (event) => {
    const { peer_node, message_id } = event.payload;
    for (const late of state.lateBuffers) late.set(message_id, "delivered");
    const msg = (state.messages[peer_node] || []).find(
      (m) => m.mine && m.wireId === message_id
    );
    if (msg) {
      msg.status = "delivered";
      updateMessageStatus(msg.id, "delivered");
    } else if (unmatchedDelivered.size < 256) {
      unmatchedDelivered.add(message_id);
    }
  });

  await listen("peer-protocol", (event) => {
    const { node_num, protocol } = event.payload;
    const peer = state.peers.find((p) => p.node_num === node_num);
    if (peer) peer.protocol = protocol;
  });

  await listen("pair-request", (event) => {
    const data = event.payload;
    if (isBlocked(data.from_node)) return; // blocked a moment ago
    state.pendingPairRequests = state.pendingPairRequests.filter(
      (r) => r.from_node !== data.from_node
    );
    state.pendingPairRequests.push(data);

    const name = data.from_name || lookupNodeName(data.from_node);
    showPendingPairBanner();

    NotificationSound.play();
    Toast.show({ sender: "MeshGuard", text: `Chat request from ${name}` });
  });

  await listen("pair-accepted", (event) => {
    const data = event.payload;
    addSystemMessage(`${data.from_name || "Peer"} accepted the chat. Secure session is now active.`);
  });
}

// ============================================================
// Notification Settings UI
// ============================================================
const notifOverlay = $("#notif-settings-overlay");
const notifSoundEnabled = $("#notif-sound-enabled");
const notifVolume = $("#notif-volume");
const notifTone = $("#notif-tone");
const notifCustomField = $("#custom-sound-field");
const notifCustomFile = $("#notif-custom-file");
const notifTestBtn = $("#notif-test-btn");
const btnNotifSettings = $("#btn-notif-settings");
const notifSettingsClose = $("#notif-settings-close");

btnNotifSettings.addEventListener("click", () => {
  loadBlocked();
  notifSoundEnabled.checked = NotificationSound.settings.enabled;
  notifVolume.value = Math.round(NotificationSound.settings.volume * 100);
  notifTone.value = NotificationSound.settings.tone;
  notifCustomField.style.display = NotificationSound.settings.tone === "custom" ? "" : "none";
  notifOverlay.style.display = "flex";
});

notifSettingsClose.addEventListener("click", () => {
  notifOverlay.style.display = "none";
});

// Android background service gate test (ADR-001 §6).
const bgServiceField = $("#bg-service-field");
const bgServiceStatus = $("#bg-service-status");
bgServiceField.hidden = !/Android/i.test(navigator.userAgent);

async function setBackgroundService(on) {
  bgServiceStatus.textContent = "";
  try {
    await invoke("set_background_service", { on });
    bgServiceStatus.textContent = on
      ? "Running. You can swipe MeshGuard away now."
      : "Stopped.";
  } catch (err) {
    bgServiceStatus.textContent = String(err);
  }
}
$("#bg-service-start").addEventListener("click", () => setBackgroundService(true));
$("#bg-service-stop").addEventListener("click", () => setBackgroundService(false));

notifOverlay.addEventListener("click", (e) => {
  if (e.target === notifOverlay) notifOverlay.style.display = "none";
});

notifSoundEnabled.addEventListener("change", () => {
  NotificationSound.settings.enabled = notifSoundEnabled.checked;
  NotificationSound.saveSettings();
});

notifVolume.addEventListener("input", () => {
  NotificationSound.settings.volume = parseInt(notifVolume.value, 10) / 100;
  NotificationSound.saveSettings();
});

notifTone.addEventListener("change", () => {
  NotificationSound.settings.tone = notifTone.value;
  notifCustomField.style.display = notifTone.value === "custom" ? "" : "none";
  NotificationSound.saveSettings();
});

notifCustomFile.addEventListener("change", async () => {
  const file = notifCustomFile.files[0];
  if (!file) return;
  const ok = await NotificationSound.loadCustomSound(file);
  if (!ok) {
    alert("Could not load sound file. Make sure it's a valid audio file under 500 KB.");
    notifTone.value = "default";
    NotificationSound.settings.tone = "default";
    notifCustomField.style.display = "none";
    NotificationSound.saveSettings();
  }
});

notifTestBtn.addEventListener("click", () => {
  NotificationSound.play();
  Toast.show({ sender: "Test", text: "This is a test notification!" });
});

// ============================================================
// Helpers
// ============================================================
function lookupNodeName(nodeNum) {
  const node = state.meshNodes.find((n) => n.node_num === nodeNum);
  if (node && (node.long_name || node.user_name)) return node.long_name || node.user_name;
  const peer = state.peers.find((p) => p.node_num === nodeNum);
  if (peer) return peer.device_name;
  return "Node " + nodeNum;
}

// ============================================================
// Check-in, SOS and location
// ============================================================
const KIND_ICONS = {
  ok: "\u2705",
  on_my_way: "\uD83D\uDEB6",
  need_help: "\u26A0\uFE0F",
  sos: "\uD83C\uDD98",
  location: "\uD83D\uDCCD",
  custom: "\uD83D\uDCAC",
};
const KIND_LABELS = {
  ok: "OK",
  on_my_way: "On my way",
  need_help: "Need help",
  sos: "SOS",
  location: "Location",
  custom: "Message",
};
const URGENT_KINDS = new Set(["sos", "need_help"]);

const quickBar = $("#quick-bar");
const btnSos = $("#btn-sos");
const sosAlert = $("#sos-alert");
const presetsOverlay = $("#presets-overlay");
const presetsList = $("#presets-list");

function mapUrl(fix) {
  const lat = (fix.lat_e7 / 1e7).toFixed(6);
  const lon = (fix.lon_e7 / 1e7).toFixed(6);
  return `https://www.openstreetmap.org/?mlat=${lat}&mlon=${lon}#map=16/${lat}/${lon}`;
}

// Coordinates, accuracy and how old the fix is. Numbers only, so no
// escaping is needed; the link is opened through the shell plugin.
function renderLocation(fix) {
  if (!fix) return "";
  const lat = (fix.lat_e7 / 1e7).toFixed(5);
  const lon = (fix.lon_e7 / 1e7).toFixed(5);
  const acc = typeof fix.accuracy_m === "number" ? ` \u00B1${fix.accuracy_m} m` : "";
  const age = fix.fix_time ? ` \u00B7 fix ${formatLastHeard(fix.fix_time)}` : " \u00B7 fix time unknown";
  return `\uD83D\uDCCD ${lat}, ${lon}${acc}${age}<a href="${mapUrl(fix)}" class="map-link">Map</a>`;
}

function renderCheckin(st) {
  const icon = KIND_ICONS[st.kind] || KIND_ICONS.custom;
  const text = st.text ? escapeHtml(st.text) : escapeHtml(KIND_LABELS[st.kind] || "");
  const loc = st.location ? `<div class="checkin-loc">${renderLocation(st.location)}</div>` : "";
  return `<div class="checkin"><div class="checkin-head">${icon} ${text}</div>${loc}</div>`;
}

function wireMapLinks(root) {
  root.querySelectorAll("a.map-link").forEach((a) => {
    a.addEventListener("click", (e) => {
      e.preventDefault();
      const shell = window.__TAURI__ && window.__TAURI__.shell;
      if (shell && shell.open) shell.open(a.href).catch((err) => console.error(err));
    });
  });
}

async function loadPresets() {
  try {
    state.presets = (await invoke("get_status_presets")) || [];
  } catch (err) {
    console.error("Could not load quick messages:", err);
    state.presets = state.presets || [];
  }
  renderQuickBar();
}

function renderQuickBar() {
  quickBar.innerHTML = "";
  const chip = (label, urgent, onClick) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "quick-chip" + (urgent ? " urgent" : "");
    b.textContent = label;
    b.addEventListener("click", onClick);
    quickBar.appendChild(b);
    return b;
  };
  for (const p of state.presets || []) {
    // SOS has its own button (with confirmation) on the mesh screen.
    if (p.kind === "sos") continue;
    chip(`${KIND_ICONS[p.kind] || ""} ${p.text}`.trim(), URGENT_KINDS.has(p.kind), () =>
      sendCheckin([state.activePeerNodeNum], p.kind, p.text, p.with_location)
    );
  }
  chip(`${KIND_ICONS.location} Share location`, false, () =>
    sendCheckin([state.activePeerNodeNum], "location", "", true)
  );
  chip("\u270F\uFE0F", false, openPresetsEditor).title = "Edit quick messages";
}

// Send to each recipient and add our own copy to each chat; ✓✓ arrives via
// message-delivered like for text messages.
async function sendCheckin(nodeNums, kind, text, withLocation) {
  quickBar.querySelectorAll("button").forEach((b) => (b.disabled = true));
  try {
    const report = await invoke("send_status", {
      peerNodeNums: nodeNums,
      kind,
      text,
      includeLocation: withLocation,
    });
    const st = { kind, text, location: report.location };
    let failed = [];
    for (const r of report.results) {
      if (r.error) {
        failed.push(`${lookupNodeName(r.peer_node)}: ${r.error}`);
        continue;
      }
      const msg = {
        id: crypto.randomUUID(),
        wireId: r.message_id,
        text,
        checkin: st,
        timestamp: Date.now(),
        mine: true,
        status: r.message_id && unmatchedDelivered.delete(r.message_id) ? "delivered" : uiStatus(r.state) || "sent",
      };
      if (!state.messages[r.peer_node]) state.messages[r.peer_node] = [];
      if (state.messages[r.peer_node].some((m) => m.id === r.message_id)) continue;
      state.messages[r.peer_node].push(msg);
      if (state.activePeerNodeNum === r.peer_node && screenChat.classList.contains("active")) {
        renderMessage(msg);
      }
    }
    if (withLocation && !report.location && kind !== "location") {
      addSystemMessage("Sent without location — your radio has no GPS fix yet.");
    }
    if (failed.length) addSystemMessage("Not sent: " + failed.join("; "));
    return report;
  } catch (err) {
    addSystemMessage(`Not sent: ${err}`);
    throw err;
  } finally {
    quickBar.querySelectorAll("button").forEach((b) => (b.disabled = false));
  }
}

btnSos.addEventListener("click", async () => {
  const recipients = state.peers.map((p) => p.node_num);
  if (recipients.length === 0) {
    alert("Pair with at least one contact first — SOS goes to everyone you have paired.");
    return;
  }
  const preset = (state.presets || []).find((p) => p.kind === "sos");
  const text = preset ? preset.text : "SOS";
  if (!confirm(`Send SOS with your location to ${recipients.length} paired contact(s)?`)) return;
  btnSos.disabled = true;
  try {
    const report = await sendCheckin(recipients, "sos", text, true);
    const ok = report.results.filter((r) => !r.error).length;
    Toast.show({
      sender: "MeshGuard",
      text: `SOS sent to ${ok} of ${report.results.length}${report.location ? " with location" : " (no GPS fix)"}`,
    });
  } catch (err) {
    alert("SOS could not be sent: " + err);
  } finally {
    btnSos.disabled = false;
  }
});

function openChatFor(nodeNum) {
  const peer = state.peers.find((p) => p.node_num === nodeNum);
  if (peer) openExistingChat(peer);
}

function showSosAlert(nodeNum, name, st) {
  $("#sos-title").textContent =
    `${KIND_ICONS[st.kind] || ""} ${st.kind === "sos" ? "SOS" : "Needs help"} \u2014 ${name}`;
  $("#sos-text").textContent = st.text || "";
  const loc = $("#sos-location");
  loc.innerHTML = st.location ? renderLocation(st.location) : "No location attached";
  wireMapLinks(loc);
  sosAlert.dataset.node = String(nodeNum);
  sosAlert.style.display = "flex";
}

$("#sos-dismiss").addEventListener("click", () => {
  sosAlert.style.display = "none";
});
$("#sos-open-chat").addEventListener("click", () => {
  sosAlert.style.display = "none";
  openChatFor(Number(sosAlert.dataset.node));
});

function openPresetsEditor() {
  presetsList.innerHTML = "";
  for (const [i, p] of (state.presets || []).entries()) {
    const row = document.createElement("div");
    row.className = "preset-row";
    row.innerHTML = `
      <span title="${escapeHtml(KIND_LABELS[p.kind] || p.kind)}">${KIND_ICONS[p.kind] || ""}</span>
      <input type="text" data-i="${i}" maxlength="156" />
      <label class="preset-loc"><input type="checkbox" data-loc="${i}" /> Attach location</label>
    `;
    row.querySelector('input[type="text"]').value = p.text;
    row.querySelector('input[type="checkbox"]').checked = !!p.with_location;
    presetsList.appendChild(row);
  }
  presetsOverlay.style.display = "flex";
}

$("#presets-close").addEventListener("click", () => (presetsOverlay.style.display = "none"));
$("#presets-reset").addEventListener("click", async () => {
  try {
    state.presets = await invoke("reset_status_presets");
    renderQuickBar();
    openPresetsEditor();
  } catch (err) {
    alert("Could not reset: " + err);
  }
});
$("#presets-save").addEventListener("click", async () => {
  const presets = (state.presets || []).map((p, i) => ({
    kind: p.kind,
    text: presetsList.querySelector(`input[data-i="${i}"]`).value,
    with_location: presetsList.querySelector(`input[data-loc="${i}"]`).checked,
  }));
  try {
    state.presets = await invoke("set_status_presets", { presets });
    renderQuickBar();
    presetsOverlay.style.display = "none";
  } catch (err) {
    alert(String(err));
  }
});

// ============================================================
// Offline QR pairing
// ============================================================
const qrOverlay = $("#qr-overlay");
const codeOverlay = $("#code-overlay");
let qrTimer = null;

$("#btn-show-qr").addEventListener("click", async () => {
  try {
    const offer = await invoke("create_pair_offer");
    // SVG generated by our own backend from the code (no user input).
    $("#qr-image").innerHTML = offer.svg;
    $("#qr-text").textContent = offer.uri;
    $("#qr-paired").textContent = "";
    const until = Date.now() + offer.expires_in_secs * 1000;
    const tick = () => {
      const left = Math.max(0, Math.round((until - Date.now()) / 1000));
      $("#qr-expiry").textContent = left
        ? `Valid for ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`
        : "Expired \u2014 tap Done and show a new code.";
      if (!left) clearInterval(qrTimer);
    };
    clearInterval(qrTimer);
    tick();
    qrTimer = setInterval(tick, 1000);
    qrOverlay.style.display = "flex";
  } catch (err) {
    alert("Could not create a code: " + err);
  }
});

$("#qr-done").addEventListener("click", async () => {
  clearInterval(qrTimer);
  qrOverlay.style.display = "none";
  try {
    await invoke("cancel_pair_offer");
  } catch (_) {}
});

function openCodeEntry(hint) {
  $("#code-hint").textContent = hint || "Type the code shown under the QR on the other device.";
  $("#code-input").value = "";
  codeOverlay.style.display = "flex";
}

$("#code-cancel").addEventListener("click", () => (codeOverlay.style.display = "none"));
$("#code-pair").addEventListener("click", async () => {
  const code = $("#code-input").value.trim();
  if (!code) return;
  if (await pairWithCode(code)) codeOverlay.style.display = "none";
});

async function pairWithCode(code) {
  try {
    const peer = await invoke("pair_with_code", { code });
    state.peers = state.peers.filter((p) => p.node_num !== peer.node_num);
    state.peers.push(peer);
    renderActiveChats();
    Toast.show({
      sender: "MeshGuard",
      text: `Request sent to ${peer.device_name} \u2014 waiting for confirmation`,
    });
    return true;
  } catch (err) {
    alert("Pairing failed: " + err);
    return false;
  }
}

const isMobile = /Android|iPhone|iPad/i.test(navigator.userAgent);

// QR codes are scanned inside the page: the camera stream (getUserMedia)
// is shown in a <video> and frames are decoded with jsQR. The WebView asks
// for the camera permission itself. No native camera view, so nothing
// outside the page can take the app down.
//
// Each scan is an *attempt* with its own id, camera stream, timer and
// result. Cancelling ends the current attempt at once; a stale attempt
// (its camera arriving after Cancel) only ever releases its own stream.
const qrScan = { active: false, attempt: 0, cleanup: null, finish: null };
let scanAttempts = 0;

$("#btn-scan-qr").addEventListener("click", async () => {
  if (!isMobile || !navigator.mediaDevices?.getUserMedia) {
    openCodeEntry("This device has no camera scanner. Type the code shown on the other phone.");
    return;
  }
  // Reserved before the first await: a second tap must not start another scan.
  if (qrScan.active) return;
  const id = ++scanAttempts;
  qrScan.attempt = id;
  setScanUi(true);
  let result;
  try {
    result = await scanQr(id);
  } catch (err) {
    if (qrScan.attempt !== id) return; // cancelled meanwhile
    endScan(id);
    console.error("Scan failed:", err);
    const denied = err && (err.name === "NotAllowedError" || err.name === "SecurityError");
    openCodeEntry(
      denied
        ? "Camera permission was denied. You can type the code instead."
        : "The camera could not be started. You can type the code instead."
    );
    return;
  }
  if (qrScan.attempt !== id) return; // cancelled: cancelScan handled it
  endScan(id);
  if (result && result.code) await pairWithCode(result.code);
});

/** Run scan attempt `id`: open the camera and resolve with `{code}` once a
 * MeshGuard pairing code is seen. Resolves `{}` if the attempt was
 * cancelled meanwhile (its camera is released). */
async function scanQr(id) {
  const owned = () => qrScan.active && qrScan.attempt === id;
  const stream = await navigator.mediaDevices.getUserMedia({
    video: { facingMode: { ideal: "environment" }, width: { ideal: 1280 } },
    audio: false,
  });
  const video = $("#scan-video");
  let timer = null;
  const release = () => {
    clearTimeout(timer);
    stream.getTracks().forEach((t) => t.stop());
    if (video.srcObject === stream) video.srcObject = null;
  };
  if (!owned()) {
    release();
    return {};
  }
  qrScan.cleanup = release;
  video.srcObject = stream;
  await video.play().catch(() => {});
  if (!owned()) {
    release();
    return {};
  }
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d", { willReadFrequently: true });
  return new Promise((resolve) => {
    qrScan.finish = resolve;
    const tick = () => {
      if (!owned()) return;
      const w = video.videoWidth;
      const h = video.videoHeight;
      if (w && h) {
        // Downscale: fast enough on slow phones, plenty for a QR code.
        const scale = Math.min(1, 720 / Math.max(w, h));
        canvas.width = Math.round(w * scale);
        canvas.height = Math.round(h * scale);
        ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
        const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
        const found = jsQR(img.data, img.width, img.height, { inversionAttempts: "attemptBoth" });
        if (found && found.data && found.data.startsWith("meshguard:")) {
          resolve({ code: found.data });
          return;
        }
      }
      timer = setTimeout(tick, 150);
    };
    tick();
  });
}

/** End scan attempt `id` (if it is still the current one): release its
 * camera and hide the scanner. */
function endScan(id) {
  if (qrScan.attempt !== id) return;
  const cleanup = qrScan.cleanup;
  const finish = qrScan.finish;
  qrScan.cleanup = null;
  qrScan.finish = null;
  qrScan.attempt = 0;
  setScanUi(false);
  if (cleanup) cleanup();
  if (finish) finish({});
}

function setScanUi(on) {
  qrScan.active = on;
  document.documentElement.classList.toggle("qr-scanning", on);
  $("#scan-ui").hidden = !on;
}

/** Leave the scanner (Cancel, Type code or Back), also while the camera or
 * its permission dialog is still opening. */
function cancelScan(typeInstead = false) {
  if (!qrScan.active) return;
  endScan(qrScan.attempt);
  if (typeInstead) openCodeEntry();
}

$("#scan-ui-cancel").addEventListener("click", () => cancelScan());
$("#scan-ui-type").addEventListener("click", () => cancelScan(true));

// ============================================================
// Blocking (Play UGC policy)
// ============================================================
// A blocked node stays on the mesh list, but the backend ignores its
// messages, chat requests, check-ins and QR pair requests.
state.blocked = [];

function isBlocked(nodeNum) {
  return state.blocked.some((b) => b.node_num === nodeNum);
}

async function loadBlocked() {
  try {
    state.blocked = (await invoke("list_blocked")) || [];
  } catch (err) {
    console.error("Could not load blocked nodes:", err);
  }
  renderBlockedList();
}

function renderBlockedList() {
  const list = $("#blocked-list");
  list.innerHTML = "";
  if (!state.blocked.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "No blocked nodes.";
    list.appendChild(li);
    return;
  }
  for (const b of state.blocked) {
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.className = "blocked-name";
    name.textContent = `${b.name} (!${(b.node_num >>> 0).toString(16).padStart(8, "0")})`;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn btn-secondary btn-sm";
    btn.textContent = "Unblock";
    btn.addEventListener("click", () => unblockNode(b.node_num));
    li.append(name, btn);
    list.appendChild(li);
  }
}

async function blockNode(nodeNum, name) {
  if (nodeNum == null) return false;
  const label = name || lookupNodeName(nodeNum);
  if (
    !confirm(
      `Block ${label}? Its messages, chat requests and check-ins will be ignored and the contact is removed. ` +
        "Message history stays until you delete it. You can unblock it in Settings."
    )
  )
    return false;
  try {
    state.blocked = (await invoke("block_node", { nodeNum })) || [];
  } catch (err) {
    alert("Could not block: " + err);
    return false;
  }
  state.peers = state.peers.filter((p) => p.node_num !== nodeNum);
  state.pendingPairRequests = state.pendingPairRequests.filter((r) => r.from_node !== nodeNum);
  renderBlockedList();
  if (diagOverlay.style.display !== "none" && diag.node === nodeNum) $("#diag-close").click();
  if (screenChat.classList.contains("active") && state.activePeerNodeNum === nodeNum) {
    await loadMeshData();
    showScreen(screenMesh);
  } else {
    await loadMeshData();
  }
  showPendingPairBanner();
  Toast.show({ sender: "MeshGuard", text: `${label} is blocked` });
  return true;
}

async function unblockNode(nodeNum) {
  try {
    state.blocked = (await invoke("unblock_node", { nodeNum })) || [];
  } catch (err) {
    alert("Could not unblock: " + err);
    return;
  }
  renderBlockedList();
  if (state.meshNodes.length) renderMeshNodes();
}

// Reports go to a public GitHub issue form (there is no MeshGuard server).
// Only the node is filled in; the user writes the rest, so no message text
// leaves the device unless they choose to type it.
const REPORT_URL = "https://github.com/nemke82/meshguard/issues/new";

function reportNode(nodeNum, name) {
  if (nodeNum == null) return;
  const id = "!" + (nodeNum >>> 0).toString(16).padStart(8, "0");
  const label = name && name !== id ? `${name} (${id})` : id;
  if (
    !confirm(
      `Report ${label}?\n\nThis opens a report form on GitHub in your browser. Reports are public: ` +
        "do not include your name, location or private messages. The developers cannot read messages or ban a radio. " +
        "To stop this node reaching you, block it. For threats or illegal content, contact your local police."
    )
  )
    return;
  const params = new URLSearchParams({
    template: "abuse-report.yml",
    title: `Report: node ${id}`,
    node: label,
  });
  const url = `${REPORT_URL}?${params}`;
  const shell = window.__TAURI__ && window.__TAURI__.shell;
  if (shell && shell.open) {
    shell.open(url).catch((err) => alert("Could not open the report form: " + err));
  } else {
    window.open(url, "_blank", "noopener");
  }
}

$("#btn-report-chat").addEventListener("click", () =>
  reportNode(state.activePeerNodeNum, chatPeerName.textContent)
);

$("#diag-report").addEventListener("click", () => reportNode(diag.node, $("#diag-title").textContent));

$("#btn-block-chat").addEventListener("click", () =>
  blockNode(state.activePeerNodeNum, chatPeerName.textContent)
);

$("#diag-block").addEventListener("click", async () => {
  const nodeNum = diag.node;
  if (nodeNum == null) return;
  if (isBlocked(nodeNum)) {
    await unblockNode(nodeNum);
    $("#diag-block").textContent = "Block";
  } else {
    await blockNode(nodeNum, $("#diag-title").textContent);
  }
});

// ============================================================
// Terms of use (accepted once, before first use)
// ============================================================
async function ensureTermsAccepted() {
  let accepted = false;
  try {
    accepted = await invoke("terms_accepted");
  } catch (err) {
    console.error("Could not read terms status:", err);
  }
  if (accepted) return;
  const overlay = $("#terms-overlay");
  const app = $("#app");
  // Nothing behind the terms can be focused or used until they are accepted.
  app.inert = true;
  overlay.style.display = "flex";
  $("#terms-accept").focus();
  await new Promise((resolve) => {
    $("#terms-accept").addEventListener(
      "click",
      async () => {
        try {
          await invoke("accept_terms");
        } catch (err) {
          console.error("Could not save terms acceptance:", err);
        }
        overlay.style.display = "none";
        app.inert = false;
        resolve();
      },
      { once: true }
    );
  });
}

// ============================================================
// Android Back button
// ============================================================
// Without a listener, Tauri finishes the activity on Back, so Back on the QR
// code, the camera or any sheet closed the whole app. Back now closes what
// is on top, and on the first screen sends the app to the background.
const BACK_CLOSERS = [
  ["#sos-alert", "#sos-dismiss"],
  ["#code-overlay", "#code-cancel"],
  ["#qr-overlay", "#qr-done"],
  ["#diag-overlay", "#diag-close"],
  ["#presets-overlay", "#presets-close"],
  ["#notif-settings-overlay", "#notif-settings-close"],
];

function handleBack() {
  if (qrScan.active) {
    cancelScan();
    return;
  }
  for (const [overlay, closer] of BACK_CLOSERS) {
    const el = $(overlay);
    if (el && el.style.display !== "none") {
      $(closer).click();
      return;
    }
  }
  if (screenChat.classList.contains("active")) {
    btnBackMeshChat.click();
    return;
  }
  if (screenPassphrase.classList.contains("active")) {
    btnBackMesh.click();
    return;
  }
  invoke("move_to_background").catch((err) => console.error("Back:", err));
}

async function setupBackButton() {
  const app = window.__TAURI__ && window.__TAURI__.app;
  if (!isMobile || !app || !app.onBackButtonPress) return;
  try {
    await app.onBackButtonPress(() => handleBack());
  } catch (err) {
    console.error("Back button handler not registered:", err);
  }
}

// ============================================================
// Mesh diagnostics
// ============================================================
const diagOverlay = $("#diag-overlay");
// `panel` changes whenever the sheet is opened for a node; `run` whenever a
// path test starts. Async continuations check them before touching the UI.
const diag = { node: null, timer: null, panel: 0, run: 0, pending: false };

function fmtNum(v, digits, unit) {
  return typeof v === "number" ? `${v.toFixed(digits)}${unit}` : "\u2014";
}

function diagStat(k, v) {
  return `<div class="diag-stat"><div class="k">${k}</div><div class="v">${escapeHtml(String(v))}</div></div>`;
}

async function openDiagnostics(nodeNum) {
  diag.node = nodeNum;
  const panel = ++diag.panel;
  diag.pending = false;
  clearTimeout(diag.timer);
  const node = state.meshNodes.find((n) => n.node_num === nodeNum) || { node_num: nodeNum };
  $("#diag-title").textContent = nodeDisplayName(node);
  $("#diag-sub").textContent = [
    "!" + (nodeNum >>> 0).toString(16).padStart(8, "0"),
    node.short_name,
    node.hw_model && node.hw_model !== "Unset" ? node.hw_model : "",
  ]
    .filter(Boolean)
    .join(" \u00B7 ");
  const battery =
    typeof node.battery_level === "number"
      ? node.battery_level > 100
        ? "Powered"
        : `${node.battery_level}%`
      : "\u2014";
  $("#diag-stats").innerHTML = [
    diagStat("Last heard", formatLastHeard(node.last_heard)),
    diagStat("Hops away", node.hops_away === 0 ? "Direct" : node.hops_away ?? "\u2014"),
    diagStat("RSSI", node.rssi ? `${node.rssi} dBm` : "\u2014"),
    diagStat("SNR", node.snr ? `${Number(node.snr).toFixed(1)} dB` : "\u2014"),
    diagStat("Battery", battery),
    diagStat("Voltage", fmtNum(node.voltage, 2, " V")),
    diagStat("Channel use", fmtNum(node.channel_utilization, 1, "%")),
    diagStat("Airtime (TX)", fmtNum(node.air_util_tx, 1, "%")),
  ].join("");
  $("#diag-path").textContent = "Sends a Meshtastic traceroute and shows each hop with its SNR.";
  $("#diag-run-path").disabled = false;
  $("#diag-block").textContent = isBlocked(nodeNum) ? "Unblock" : "Block";
  diagOverlay.style.display = "flex";
  $("#diag-chart").textContent = "Loading…";
  try {
    const history = await invoke("get_node_history", { nodeNum });
    if (panel === diag.panel) renderSignalChart(history);
  } catch (err) {
    if (panel === diag.panel) $("#diag-chart").textContent = "No history: " + err;
  }
}

// Two lines (RSSI dBm, SNR dB), each scaled to its own range. Values are
// numbers, so the SVG is built from numbers only.
function renderSignalChart(samples) {
  const pts = (samples || []).filter((s) => s.rssi != null || s.snr != null);
  const box = $("#diag-chart");
  if (pts.length < 2) {
    box.textContent =
      pts.length === 1
        ? "One direct packet so far — the chart appears after the next one."
        : "No direct packets heard yet (relayed packets do not show this node's signal).";
    return;
  }
  const W = 300, H = 90, P = 4;
  const t0 = pts[0].t, t1 = pts[pts.length - 1].t || t0 + 1;
  const x = (t) => P + ((t - t0) / Math.max(1, t1 - t0)) * (W - 2 * P);
  const line = (key, color) => {
    const vals = pts.filter((s) => s[key] != null);
    if (vals.length < 2) return "";
    const lo = Math.min(...vals.map((s) => s[key]));
    const hi = Math.max(...vals.map((s) => s[key]));
    const y = (v) => H - P - ((v - lo) / Math.max(1e-6, hi - lo)) * (H - 2 * P);
    const d = vals.map((s, i) => `${i ? "L" : "M"}${x(s.t).toFixed(1)},${y(s[key]).toFixed(1)}`).join(" ");
    return `<path d="${d}" fill="none" stroke="${color}" stroke-width="1.8" stroke-linejoin="round"/>`;
  };
  const last = (key) => {
    for (let i = pts.length - 1; i >= 0; i--) if (pts[i][key] != null) return pts[i][key];
    return null;
  };
  const range = (key, digits) => {
    const v = pts.filter((s) => s[key] != null).map((s) => s[key]);
    return v.length ? `${Math.min(...v).toFixed(digits)} \u2026 ${Math.max(...v).toFixed(digits)}` : "\u2014";
  };
  box.innerHTML = `
    <svg viewBox="0 0 ${W} ${H}" role="img" aria-label="Signal history">
      <rect x="0" y="0" width="${W}" height="${H}" rx="8" fill="var(--bg-input)"/>
      ${line("rssi", "var(--accent)")}
      ${line("snr", "#4da3ff")}
    </svg>
    <div class="diag-legend">
      <span class="rssi">\u25CF RSSI ${last("rssi") ?? "\u2014"} dBm</span>
      <span class="snr">\u25CF SNR ${last("snr") != null ? last("snr").toFixed(1) : "\u2014"} dB</span>
    </div>
    <div>Range: RSSI ${range("rssi", 0)} dBm \u00B7 SNR ${range("snr", 1)} dB</div>
    <div>${pts.length} samples since ${formatTime(t0 * 1000)}</div>`;
}

function hopLabel(nodeNum) {
  const node = state.meshNodes.find((n) => n.node_num === nodeNum);
  if (node) return nodeDisplayName(node);
  return "!" + (nodeNum >>> 0).toString(16).padStart(8, "0");
}

// `endsWithMe`: the last hop of the way back is our own radio.
function renderPath(hops, startLabel, endsWithMe) {
  const parts = [`<span class="hop">${escapeHtml(startLabel)}</span>`];
  hops.forEach((h, i) => {
    const snr = typeof h.snr === "number" ? `${h.snr.toFixed(1)} dB` : "? dB";
    const label = endsWithMe && i === hops.length - 1 ? "You" : hopLabel(h.node);
    parts.push(` \u2192 <small>${snr}</small> \u2192 <span class="hop">${escapeHtml(label)}</span>`);
  });
  return parts.join("");
}

function renderPathResult(r) {
  const relays = Math.max(0, r.towards.length - 1);
  let html = `<div><strong>Reply in ${(r.rtt_ms / 1000).toFixed(1)} s</strong> \u00B7 ${
    relays === 0 ? "direct" : relays + (relays === 1 ? " relay" : " relays")
  }</div>`;
  html += `<div>Out: ${renderPath(r.towards, "You", false)}</div>`;
  if (r.back.length) html += `<div>Back: ${renderPath(r.back, hopLabel(r.node), true)}</div>`;
  $("#diag-path").innerHTML = html;
  $("#diag-run-path").disabled = false;
}

$("#diag-run-path").addEventListener("click", async () => {
  const nodeNum = diag.node;
  const panel = diag.panel;
  const run = ++diag.run;
  const btn = $("#diag-run-path");
  btn.disabled = true;
  $("#diag-path").textContent = "Waiting for the reply (up to 90 s)…";
  // Pending before the request: a fast reply may arrive before invoke()
  // resolves. The timeout starts once the request was actually sent (the
  // command may wait for airtime) and only fires if this run is pending.
  diag.pending = true;
  clearTimeout(diag.timer);
  try {
    await invoke("run_path_test", { nodeNum });
    if (panel !== diag.panel || run !== diag.run || !diag.pending) return;
    diag.timer = setTimeout(() => {
      if (panel !== diag.panel || run !== diag.run || !diag.pending) return;
      diag.pending = false;
      $("#diag-path").textContent =
        "No reply within 90 s. The node may be out of range, asleep, or its firmware may not answer path tests.";
      btn.disabled = false;
    }, 90_000);
  } catch (err) {
    if (panel !== diag.panel || run !== diag.run) return;
    diag.pending = false;
    clearTimeout(diag.timer);
    $("#diag-path").textContent = String(err);
    btn.disabled = false;
  }
});

$("#diag-close").addEventListener("click", () => {
  diagOverlay.style.display = "none";
  diag.node = null;
  diag.panel++;
  diag.pending = false;
  clearTimeout(diag.timer);
});

// ============================================================
// Init
// ============================================================
async function init() {
  await setupBackButton();
  await ensureTermsAccepted();
  await loadBlocked();
  await setupEventListeners();
  await loadPresets();

  try {
    const connected = await invoke("is_connected");
    if (connected) {
      await loadMeshData();
      showScreen(screenMesh);
      return;
    }
  } catch (_) {}

  showScreen(screenConnect);

  // Come back to the last radio on our own; retries until it is reachable.
  try {
    const target = await invoke("get_last_link");
    if (target) {
      // Show the bar before starting the loop: its events (including a
      // terminal "reconnect-stopped") then always win over this.
      showLinkStatus(`Connecting to ${target.name || target.address || target.port}…`);
      if (!(await invoke("auto_connect"))) hideLinkStatus();
    }
  } catch (err) {
    hideLinkStatus();
    console.error("Auto-connect failed to start:", err);
  }
}

// ============================================================
// Link status (auto-connect / auto-reconnect)
// ============================================================
function showLinkStatus(text) {
  linkStatusText.textContent = text;
  linkStatus.style.display = "flex";
  document.getElementById("app").classList.add("link-status-visible");
}

function hideLinkStatus() {
  linkStatus.style.display = "none";
  document.getElementById("app").classList.remove("link-status-visible");
}

btnLinkStop.addEventListener("click", async () => {
  hideLinkStatus();
  try {
    await invoke("disconnect_device");
  } catch (_) {}
  showScreen(screenConnect);
});

// ============================================================
// Utilities
// ============================================================
function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}

function formatTime(ts) {
  return new Date(ts).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });
}

// A node is online when heard within the last 2 hours (as in the Meshtastic
// apps). Computed here so the badge ages without a backend event.
const ONLINE_WINDOW_SECS = 2 * 60 * 60;

function isOnline(node) {
  if (!node.last_heard) return false;
  return Math.floor(Date.now() / 1000) - node.last_heard < ONLINE_WINDOW_SECS;
}

function nodeDisplayName(node) {
  return (
    node.long_name ||
    node.user_name ||
    "!" + (node.node_num >>> 0).toString(16).padStart(8, "0")
  );
}

// Signal, hops and battery, like the Meshtastic node list. Values are
// numbers from the backend, so no escaping is needed.
function formatSignal(node) {
  const parts = [];
  if (node.hops_away === 0) parts.push("direct");
  else if (typeof node.hops_away === "number") {
    parts.push(node.hops_away + (node.hops_away === 1 ? " hop" : " hops"));
  }
  if (node.hops_away === 0 || node.hops_away == null) {
    if (node.rssi) parts.push(Math.round(node.rssi) + " dBm");
    if (node.snr) parts.push("SNR " + Number(node.snr).toFixed(1) + " dB");
  }
  if (typeof node.battery_level === "number") {
    parts.push(node.battery_level > 100 ? "\u26A1 powered" : "\uD83D\uDD0B " + node.battery_level + "%");
  }
  if (node.via_mqtt) parts.push("MQTT");
  return parts.map((p) => `<span>${p}</span>`).join("");
}

function formatLastHeard(epochSecs) {
  if (!epochSecs || epochSecs === 0) return "never";
  const diff = Math.floor(Date.now() / 1000) - epochSecs;
  if (diff < 60) return "just now";
  if (diff < 3600) return Math.floor(diff / 60) + "m ago";
  if (diff < 86400) return Math.floor(diff / 3600) + "h ago";
  return Math.floor(diff / 86400) + "d ago";
}

init();

// Re-render once a minute so "last heard" and online dots stay current.
setInterval(() => {
  if (state.meshNodes.length) renderMeshNodes();
}, 60_000);

// Open web links (privacy policy, terms) in the system browser: the webview
// must not navigate away from the app.
document.addEventListener("click", (e) => {
  const a = e.target.closest && e.target.closest('a[target="_blank"]');
  if (!a) return;
  e.preventDefault();
  const shell = window.__TAURI__ && window.__TAURI__.shell;
  if (shell && shell.open) {
    shell.open(a.href).catch((err) => console.error("Could not open link:", err));
  }
});
