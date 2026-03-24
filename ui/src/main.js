const { invoke } = window.__TAURI__.core;

// ============================================================
// DOM
// ============================================================
const $ = (sel) => document.querySelector(sel);

const screenSetup = $("#screen-setup");
const screenPairing = $("#screen-pairing");
const screenChat = $("#screen-chat");

const formDevice = $("#form-device");
const formPairing = $("#form-pairing");

const btnBackSetup = $("#btn-back-setup");
const btnBackPairing = $("#btn-back-pairing");
const btnSend = $("#btn-send");

const messageInput = $("#message-input");
const charCount = $("#char-count");
const messagesContainer = $("#messages");
const chatPeerName = $("#chat-peer-name");
const connectionBadge = $("#connection-badge");
const connectionText = $("#connection-text");

// ============================================================
// State
// ============================================================
const state = {
  messages: [],
  connected: false,
};

// ============================================================
// Screen Navigation
// ============================================================
function showScreen(screen) {
  document.querySelectorAll(".screen").forEach((s) => s.classList.remove("active"));
  screen.classList.add("active");
}

// ============================================================
// Screen 1: Device Setup
// ============================================================
formDevice.addEventListener("submit", async (e) => {
  e.preventDefault();

  const btn = formDevice.querySelector("button[type=submit]");
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span> Saving...';

  try {
    await invoke("save_device_config", {
      input: {
        deviceName: $("#device-name").value.trim(),
        deviceSerial: $("#device-serial").value.trim(),
        bleAddress: $("#ble-address").value.trim(),
        region: $("#region").value,
        modemPreset: $("#modem-preset").value,
        txPower: parseInt($("#tx-power").value),
        hopLimit: parseInt($("#hop-limit").value),
      },
    });
    showScreen(screenPairing);
  } catch (err) {
    alert("Error: " + err);
  }

  btn.disabled = false;
  btn.innerHTML = "Save &amp; Continue to Pairing";
});

// ============================================================
// Screen 2: P2P Pairing
// ============================================================
btnBackSetup.addEventListener("click", () => showScreen(screenSetup));

formPairing.addEventListener("submit", async (e) => {
  e.preventDefault();

  const btn = formPairing.querySelector("button[type=submit]");
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span> Deriving keys...';

  const peerName = $("#peer-name").value.trim();
  const peerSerial = $("#peer-serial").value.trim();
  const passphrase = $("#shared-passphrase").value;

  try {
    // 1. Set up P2P pairing (derives encryption key + channel PSK)
    await invoke("setup_peer", {
      peerDeviceName: peerName,
      peerDeviceSerial: peerSerial,
      sharedPassphrase: passphrase,
    });

    // 2. Connect to local Meshtastic device via BLE
    btn.innerHTML = '<span class="spinner"></span> Connecting to device...';
    try {
      await invoke("connect_local_device");
      state.connected = true;

      // 3. Push config (region, channel, PSK) to the device
      btn.innerHTML = '<span class="spinner"></span> Applying config...';
      await invoke("apply_config_to_device");
    } catch (bleErr) {
      console.warn("BLE connection skipped (demo mode):", bleErr);
      // Continue in demo mode
    }

    // Clear passphrase from DOM
    $("#shared-passphrase").value = "";

    // Switch to chat
    chatPeerName.textContent = peerName;
    updateConnectionStatus(state.connected);
    showScreen(screenChat);
    addSystemMessage("Secure session established. All messages are encrypted with AES-256-GCM.");
    addSystemMessage("Encryption key derived from device identities + shared passphrase. No key data was sent over the mesh.");

  } catch (err) {
    alert("Pairing error: " + err);
  }

  btn.disabled = false;
  btn.innerHTML = "Establish Secure Session";
});

// ============================================================
// Screen 3: Chat
// ============================================================
btnBackPairing.addEventListener("click", () => {
  showScreen(screenPairing);
});

messageInput.addEventListener("input", () => {
  const len = messageInput.value.length;
  charCount.textContent = 200 - len;
  btnSend.disabled = len === 0;
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
  charCount.textContent = "200";
  btnSend.disabled = true;

  try {
    await invoke("send_message", { text });
    msg.status = "delivered";
    updateMessageStatus(msg.id, "delivered");
  } catch (err) {
    // Demo mode: simulate delivery
    setTimeout(() => {
      msg.status = "delivered";
      updateMessageStatus(msg.id, "delivered");
    }, 400);

    // Demo: echo back
    setTimeout(() => {
      const echo = {
        id: crypto.randomUUID(),
        text: `[Peer] ${text}`,
        timestamp: Date.now(),
        mine: false,
        status: "delivered",
      };
      state.messages.push(echo);
      renderMessage(echo);
    }, 1200);
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
    case "sent": return "\u2713";
    case "delivered": return "\u2713\u2713";
    case "read": return "\u2713\u2713";
    default: return "\u26A0";
  }
}

function updateConnectionStatus(connected) {
  if (connected) {
    connectionBadge.classList.add("connected");
    connectionText.textContent = "Connected";
  } else {
    connectionBadge.classList.remove("connected");
    connectionText.textContent = "Demo Mode";
  }
}

// ============================================================
// On Load: restore saved config
// ============================================================
async function init() {
  try {
    const device = await invoke("get_device_config");
    if (device) {
      $("#device-name").value = device.device_name || "";
      $("#device-serial").value = device.device_serial || "";
      $("#ble-address").value = device.ble_address || "";
      if (device.radio) {
        $("#region").value = device.radio.region || "EU868";
        $("#modem-preset").value = device.radio.modem_preset || "LongRange";
        $("#tx-power").value = device.radio.tx_power || 20;
        $("#hop-limit").value = device.radio.hop_limit || 3;
      }

      // If peer is also configured, check if we have a session
      const peer = await invoke("get_peer_config");
      const hasSession = await invoke("has_session");
      if (peer && hasSession) {
        chatPeerName.textContent = peer.device_name;
        showScreen(screenChat);
        addSystemMessage("Session restored. Enter passphrase again to re-derive keys if needed.");
        return;
      }

      if (peer) {
        $("#peer-name").value = peer.device_name || "";
        $("#peer-serial").value = peer.device_serial || "";
        showScreen(screenPairing);
        return;
      }
    }
  } catch (_) {
    // Fresh start
  }
  showScreen(screenSetup);
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
  return new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

init();
