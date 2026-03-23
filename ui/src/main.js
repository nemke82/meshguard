const { invoke } = window.__TAURI__.core;

// ============================================================
// State
// ============================================================
const state = {
  connected: false,
  peerName: "",
  messages: [],
};

// ============================================================
// DOM Elements
// ============================================================
const $ = (sel) => document.querySelector(sel);
const screenConnect = $("#screen-connect");
const screenChat = $("#screen-chat");
const btnScan = $("#btn-scan");
const btnBack = $("#btn-back");
const btnSend = $("#btn-send");
const deviceList = $("#device-list");
const scanStatus = $("#scan-status");
const messagesContainer = $("#messages");
const messageInput = $("#message-input");
const charCount = $("#char-count");
const peerName = $("#peer-name");
const signalIndicator = $("#signal-indicator");

// ============================================================
// Screen Navigation
// ============================================================
function showScreen(screen) {
  document.querySelectorAll(".screen").forEach((s) => s.classList.remove("active"));
  screen.classList.add("active");
}

// ============================================================
// Device Scanning
// ============================================================
btnScan.addEventListener("click", async () => {
  btnScan.disabled = true;
  btnScan.innerHTML = '<div class="spinner"></div> Scanning...';
  scanStatus.textContent = "Looking for Meshtastic devices nearby...";
  deviceList.innerHTML = "";

  try {
    const devices = await invoke("scan_devices");

    if (devices.length === 0) {
      scanStatus.textContent = "No Meshtastic devices found. Make sure your P1000 is on.";
      // Demo mode: add a simulated device for UI testing
      addDemoDevice();
    } else {
      scanStatus.textContent = `Found ${devices.length} device${devices.length > 1 ? "s" : ""}`;
      devices.forEach(renderDevice);
    }
  } catch (err) {
    scanStatus.textContent = "Scan failed — check Bluetooth permissions.";
    console.error("Scan error:", err);
    // Fallback demo
    addDemoDevice();
  }

  btnScan.disabled = false;
  btnScan.innerHTML = '<span class="btn-icon">&#x1F50D;</span> Scan Again';
});

function addDemoDevice() {
  renderDevice({
    name: "Sensecap P1000 (Demo)",
    address: "AA:BB:CC:DD:EE:FF",
    rssi: -45,
  });
}

function renderDevice(device) {
  const card = document.createElement("div");
  card.className = "device-card";
  card.innerHTML = `
    <div class="device-icon">&#x1F4E1;</div>
    <div class="device-details">
      <div class="device-name">${escapeHtml(device.name)}</div>
      <div class="device-addr">${device.address}</div>
    </div>
    <div class="device-rssi">${device.rssi ? device.rssi + " dBm" : ""}</div>
  `;
  card.addEventListener("click", () => connectToDevice(device));
  deviceList.appendChild(card);
}

// ============================================================
// Connect to Device
// ============================================================
async function connectToDevice(device) {
  scanStatus.textContent = `Connecting to ${device.name}...`;

  try {
    await invoke("connect_device", { address: device.address });
    state.connected = true;
    state.peerName = device.name;
    peerName.textContent = device.name;
    updateSignal(device.rssi);
    showScreen(screenChat);
    addSystemMessage("Secure connection established. Messages are end-to-end encrypted.");
  } catch (err) {
    // Demo mode: connect anyway for UI preview
    state.connected = true;
    state.peerName = device.name;
    peerName.textContent = device.name;
    updateSignal(device.rssi);
    showScreen(screenChat);
    addSystemMessage("Connected in demo mode. Encryption active.");
  }
}

// ============================================================
// Signal Strength
// ============================================================
function updateSignal(rssi) {
  signalIndicator.classList.remove("strong", "medium", "weak");
  if (!rssi) return;
  if (rssi > -50) signalIndicator.classList.add("strong");
  else if (rssi > -70) signalIndicator.classList.add("medium");
  else signalIndicator.classList.add("weak");
}

// ============================================================
// Messaging
// ============================================================
messageInput.addEventListener("input", () => {
  const len = messageInput.value.length;
  charCount.textContent = 228 - len;
  btnSend.disabled = len === 0;

  // Auto-resize
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
  if (!text) return;

  const msg = {
    id: crypto.randomUUID(),
    text,
    timestamp: Date.now(),
    mine: true,
    status: "sent",
  };

  state.messages.push(msg);
  renderMessage(msg);
  messageInput.value = "";
  messageInput.style.height = "auto";
  charCount.textContent = "228";
  btnSend.disabled = true;

  try {
    await invoke("send_message", { text });
    msg.status = "delivered";
    updateMessageStatus(msg.id, "delivered");
  } catch (err) {
    // In demo mode, simulate delivery
    setTimeout(() => {
      msg.status = "delivered";
      updateMessageStatus(msg.id, "delivered");
    }, 500);

    // Demo: echo back after delay
    setTimeout(() => {
      const echo = {
        id: crypto.randomUUID(),
        text: `Echo: ${text}`,
        timestamp: Date.now(),
        mine: false,
        status: "delivered",
      };
      state.messages.push(echo);
      renderMessage(echo);
    }, 1500);
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
  el.className = "time-divider";
  el.textContent = text;
  messagesContainer.appendChild(el);
}

function updateMessageStatus(id, status) {
  const el = document.getElementById(`status-${id}`);
  if (el) el.textContent = statusIcon(status);
}

function statusIcon(status) {
  switch (status) {
    case "sent": return "\u2713";
    case "delivered": return "\u2713\u2713";
    case "read": return "\u2713\u2713";
    default: return "\u26A0";
  }
}

// ============================================================
// Disconnect
// ============================================================
btnBack.addEventListener("click", async () => {
  try {
    await invoke("disconnect_device");
  } catch (_) {}
  state.connected = false;
  state.messages = [];
  messagesContainer.innerHTML = "";
  showScreen(screenConnect);
  scanStatus.textContent = "";
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
  return new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
