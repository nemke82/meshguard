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
    const card = document.createElement("div");
    card.className = "mesh-node-card" + (node.is_online ? " online" : "");
    card.innerHTML = `
      <div class="node-avatar">${(node.short_name || "?")[0].toUpperCase()}</div>
      <div class="node-info">
        <div class="node-name">${escapeHtml(node.long_name || node.user_name || "Unknown")}</div>
        <div class="node-meta">
          ${node.short_name ? `<span class="node-short">${escapeHtml(node.short_name)}</span>` : ""}
          ${node.hw_model && node.hw_model !== "Unset" ? `<span class="node-hw">${escapeHtml(node.hw_model)}</span>` : ""}
          <span class="node-heard">${formatLastHeard(node.last_heard)}</span>
        </div>
      </div>
      <div class="node-actions">
        ${node.is_online ? '<span class="online-dot"></span>' : '<span class="offline-dot"></span>'}
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
btnBackMesh.addEventListener("click", () => showScreen(screenMesh));

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
      });
    }

    passphraseInput.value = "";
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
  if (state.pendingPairRequests.length === 0) return;
  const req = state.pendingPairRequests[0];
  state.passphraseMode = "accept";
  state.passphraseTargetNode = req.from_node;
  passphraseTitle.textContent = "Accept Chat Request";
  passphraseSubtitle.textContent = `from ${req.from_name || "Node " + req.from_node}`;
  btnSubmitPassphrase.textContent = "Accept Chat";
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
  const len = messageInput.value.length;
  charCount.textContent = 160 - len;
  btnSend.disabled = len === 0 || len > 160;
  messageInput.style.height = "auto";
  messageInput.style.height = Math.min(messageInput.scrollHeight, 100) + "px";
});

messageInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (messageInput.value.trim()) sendMessage();
  }
});

btnSend.addEventListener("click", () => {
  if (messageInput.value.trim()) sendMessage();
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
  charCount.textContent = "160";
  btnSend.disabled = true;

  try {
    await invoke("send_message", { peerNodeNum: nodeNum, text });
    msg.status = "sent";
    updateMessageStatus(msg.id, "sent");
  } catch (err) {
    msg.status = "failed";
    updateMessageStatus(msg.id, "failed");
    console.error("Send failed:", err);
  }
}

function renderMessage(msg) {
  const el = document.createElement("div");
  el.className = `message ${msg.mine ? "mine" : "peer"}`;
  el.id = `msg-${msg.id}`;
  el.innerHTML = `
    <div class="msg-text">${escapeHtml(msg.text)}</div>
    <div class="msg-meta">
      <span>${formatTime(msg.timestamp)}</span>
      ${msg.mine ? `<span class="msg-status" id="status-${msg.id}">${statusIcon(msg.status)}</span>` : ""}
    </div>
  `;
  messagesContainer.appendChild(el);
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

  await listen("pair-request", (event) => {
    const data = event.payload;
    state.pendingPairRequests.push(data);

    const name = data.from_name || lookupNodeName(data.from_node);
    pairRequestText.textContent = `Chat request from ${name}`;
    pairRequestBanner.style.display = "flex";

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
  if (node) return node.long_name || node.user_name || "Node " + nodeNum;
  const peer = state.peers.find((p) => p.node_num === nodeNum);
  if (peer) return peer.device_name;
  return "Node " + nodeNum;
}

// ============================================================
// Init
// ============================================================
async function init() {
  await setupEventListeners();

  try {
    const connected = await invoke("is_connected");
    if (connected) {
      await loadMeshData();
      showScreen(screenMesh);
      return;
    }
  } catch (_) {}

  // Try auto-reconnect to last device
  try {
    const lastBle = await invoke("get_last_connection");
    if (lastBle) {
      scanStatus.textContent = `Last device: ${lastBle}`;
      setScanStatusType("info");
    }
  } catch (_) {}

  showScreen(screenConnect);
}

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

function formatLastHeard(epochSecs) {
  if (!epochSecs || epochSecs === 0) return "never";
  const diff = Math.floor(Date.now() / 1000) - epochSecs;
  if (diff < 60) return "just now";
  if (diff < 3600) return Math.floor(diff / 60) + "m ago";
  if (diff < 86400) return Math.floor(diff / 3600) + "h ago";
  return Math.floor(diff / 86400) + "d ago";
}

init();
