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
const serialPort = $("#serial-port");
const btnRefreshPorts = $("#btn-refresh-ports");
const btnConnectSerial = $("#btn-connect-serial");

const btnDisconnect = $("#btn-disconnect");
const btnRefreshNodes = $("#btn-refresh-nodes");
const myDeviceLabel = $("#my-device-label");
const meshNodesList = $("#mesh-nodes-list");
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

// ============================================================
// State
// ============================================================
const state = {
  meshNodes: [],
  peers: [],
  messages: {},
  activePeerNodeNum: null,
  passphraseMode: null, // "initiate" or "accept"
  passphraseTargetNode: null,
  pendingPairRequests: [],
  bannerPairRequest: null,
};

// ============================================================
// Screen Navigation
// ============================================================
function showScreen(screen) {
  document.querySelectorAll(".screen").forEach((s) => s.classList.remove("active"));
  screen.classList.add("active");
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
btnConnectWifi.addEventListener("click", async () => {
  let addr = wifiAddress.value.trim();
  if (!addr) { alert("Enter a device IP address"); return; }
  if (!addr.includes(":")) addr += ":4403";

  connectProgress.style.display = "flex";
  connectProgressText.textContent = "Connecting via WiFi to " + addr + "...";
  btnConnectWifi.disabled = true;

  try {
    await invokeWithTimeout("connect_tcp", { address: addr }, 30000);
    await loadMeshData();
    showScreen(screenMesh);
  } catch (err) {
    connectProgress.style.display = "none";
    alert("WiFi connection failed: " + err);
  }

  connectProgress.style.display = "none";
  btnConnectWifi.disabled = false;
});

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
    ]);

    state.meshNodes = nodes || [];
    state.peers = peers || [];

    if (deviceInfo) {
      myDeviceLabel.textContent = deviceInfo.device_name + " (connected)";
    }

    renderMeshNodes();
    renderActiveChats();
  } catch (err) {
    console.error("Failed to load mesh data:", err);
  }
}

function renderMeshNodes() {
  meshNodesList.innerHTML = "";

  if (state.meshNodes.length === 0) {
    meshNodesList.innerHTML =
      '<div class="empty-state">No devices found on the mesh yet. Other Meshtastic devices will appear here as they are discovered.</div>';
    return;
  }

  const sorted = [...state.meshNodes].sort(
    (a, b) => b.last_heard - a.last_heard
  );

  for (const node of sorted) {
    const isPeer = state.peers.some((p) => p.node_num === node.node_num);
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
        ${online ? '<span class="online-dot" title="Heard in the last 2 hours"></span>' : '<span class="offline-dot"></span>'}
        ${isPeer ? '<span class="peer-badge">Paired</span>' : ""}
      </div>
    `;
    card.addEventListener("click", () => onNodeTap(node));
    meshNodesList.appendChild(card);
  }
}

function renderActiveChats() {
  if (state.peers.length === 0) {
    activeChatsSection.style.display = "none";
    return;
  }

  activeChatsSection.style.display = "";
  activeChatsList.innerHTML = "";

  for (const peer of state.peers) {
    const card = document.createElement("div");
    card.className = "mesh-node-card peer-chat-card";
    const initial = (peer.device_name || "?")[0].toUpperCase();
    card.innerHTML = `
      <div class="node-avatar">${initial}</div>
      <div class="node-info">
        <div class="node-name">${escapeHtml(peer.device_name)}</div>
        <div class="node-meta"><span class="peer-badge">Encrypted Chat</span></div>
      </div>
      <div class="node-actions">
        <button class="btn-remove-peer" title="Remove" data-node="${peer.node_num}">&times;</button>
      </div>
    `;
    card.addEventListener("click", (e) => {
      if (e.target.closest(".btn-remove-peer")) return;
      openExistingChat(peer);
    });
    card.querySelector(".btn-remove-peer").addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!confirm(`Remove peer "${peer.device_name}"?`)) return;
      await invoke("remove_peer", { peerNodeNum: peer.node_num });
      await loadMeshData();
    });
    activeChatsList.appendChild(card);
  }
}

async function onNodeTap(node) {
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
function enterChat(nodeNum, peerName) {
  state.activePeerNodeNum = nodeNum;
  chatPeerName.textContent = peerName || "Peer";
  loadChatMessages(nodeNum);
  showScreen(screenChat);
  addSystemMessage("Secure session active. All messages are encrypted with AES-256-GCM.");
}

btnBackMeshChat.addEventListener("click", async () => {
  await loadMeshData();
  showScreen(screenMesh);
});

function loadChatMessages(nodeNum) {
  messagesContainer.innerHTML = "";
  const msgs = state.messages[nodeNum] || [];
  msgs.forEach(renderMessage);
}

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
  if (!text || !nodeNum) return;

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
    if (msg.wireId && unmatchedDelivered.delete(msg.wireId)) {
      msg.status = "delivered";
      updateMessageStatus(msg.id, "delivered");
    } else if (msg.status === "sending") {
      msg.status = "sent";
      updateMessageStatus(msg.id, "sent");
    }
  } catch (err) {
    msg.status = "failed";
    updateMessageStatus(msg.id, "failed");
    addSystemMessage(`Not sent: ${err}`);
    console.error("Send failed:", err);
  }
}

function renderMessage(msg) {
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
  messagesContainer.appendChild(el);
  wireMapLinks(el);
  messagesContainer.scrollTop = messagesContainer.scrollHeight;
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

    state.messages[nodeNum].push(msg);

    const onChatScreen =
      state.activePeerNodeNum === nodeNum && screenChat.classList.contains("active");

    if (onChatScreen) {
      renderMessage(msg);
    }

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
    state.messages[nodeNum].push(msg);

    const onChatScreen =
      state.activePeerNodeNum === nodeNum && screenChat.classList.contains("active");
    if (onChatScreen) renderMessage(msg);

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

  await listen("message-delivered", (event) => {
    const { peer_node, message_id } = event.payload;
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
  notifSoundEnabled.checked = NotificationSound.settings.enabled;
  notifVolume.value = Math.round(NotificationSound.settings.volume * 100);
  notifTone.value = NotificationSound.settings.tone;
  notifCustomField.style.display = NotificationSound.settings.tone === "custom" ? "" : "none";
  notifOverlay.style.display = "flex";
});

notifSettingsClose.addEventListener("click", () => {
  notifOverlay.style.display = "none";
});

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
        status: r.message_id && unmatchedDelivered.delete(r.message_id) ? "delivered" : "sent",
      };
      if (!state.messages[r.peer_node]) state.messages[r.peer_node] = [];
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

$("#btn-scan-qr").addEventListener("click", async () => {
  if (!isMobile) {
    openCodeEntry("This device has no camera scanner. Type the code shown on the other phone.");
    return;
  }
  try {
    const perm = await invoke("plugin:barcode-scanner|request_permissions");
    if (perm && perm.camera && perm.camera !== "granted") {
      openCodeEntry("Camera permission was denied. You can type the code instead.");
      return;
    }
    const result = await invoke("plugin:barcode-scanner|scan", {
      windowed: false,
      formats: ["QR_CODE"],
    });
    const content = result && result.content;
    if (content) await pairWithCode(content);
  } catch (err) {
    console.error("Scan failed:", err);
    openCodeEntry("Scanning did not work (" + err + "). You can type the code instead.");
  }
});

// ============================================================
// Init
// ============================================================
async function init() {
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

// Open the privacy policy in the system browser (the webview must not
// navigate away from the app).
document.getElementById("link-privacy")?.addEventListener("click", (e) => {
  e.preventDefault();
  const url = e.currentTarget.href;
  const shell = window.__TAURI__ && window.__TAURI__.shell;
  if (shell && shell.open) {
    shell.open(url).catch((err) => console.error("Could not open link:", err));
  }
});
