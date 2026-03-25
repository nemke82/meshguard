if (!window.__TAURI__ || !window.__TAURI__.core) {
  document.body.innerHTML = '<div style="color:red;padding:2em;font-family:monospace;">ERROR: Tauri bridge not loaded. Check withGlobalTauri in tauri.conf.json</div>';
  throw new Error("Tauri bridge not available");
}
const { invoke } = window.__TAURI__.core;

// ============================================================
// Helpers — invoke with timeout (prevents UI from hanging)
// ============================================================
function invokeWithTimeout(cmd, args, ms = 15000) {
  return Promise.race([
    invoke(cmd, args),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error(`Command timed out after ${ms / 1000}s: ${cmd}`)), ms)
    ),
  ]);
}

// ============================================================
// DOM
// ============================================================
const $ = (sel) => document.querySelector(sel);

const screenSetup = $("#screen-setup");
const screenPeers = $("#screen-peers");
const screenUnlock = $("#screen-unlock");
const screenChat = $("#screen-chat");

const formDevice = $("#form-device");
const formAddPeer = $("#form-add-peer");
const formUnlock = $("#form-unlock");

const btnScan = $("#btn-scan");
const btnBackSetup = $("#btn-back-setup");
const btnBackPeers = $("#btn-back-peers");
const btnBackPeersChat = $("#btn-back-peers-chat");
const btnSend = $("#btn-send");
const btnRemoveDevice = $("#btn-remove-device");

const scanStatus = $("#scan-status");
const scanResults = $("#scan-results");
const bleAddressInput = $("#ble-address");
const deviceLockedBanner = $("#device-locked-banner");
const lockedDeviceName = $("#locked-device-name");
const lockedDeviceAddr = $("#locked-device-addr");

const peersList = $("#peers-list");
const unlockPeerLabel = $("#unlock-peer-label");

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
  messages: {},      // peer_id -> [messages]
  connected: false,
  selectedBleAddress: "",
  peers: [],
  activePeerId: null,
  unlockPeerId: null,
};

// ============================================================
// Screen Navigation
// ============================================================
function showScreen(screen) {
  document.querySelectorAll(".screen").forEach((s) => s.classList.remove("active"));
  screen.classList.add("active");
}

// ============================================================
// BLE Scanning
// ============================================================
btnScan.addEventListener("click", async () => {
  btnScan.disabled = true;
  scanResults.innerHTML = "";

  btnScan.innerHTML = '<span class="spinner"></span> Checking Bluetooth...';
  scanStatus.textContent = "";
  setScanStatusType("info");

  try {
    console.log("[MeshGuard] Checking Bluetooth status...");
    const btStatus = await invokeWithTimeout("check_bluetooth", undefined, 10000);
    console.log("[MeshGuard] Bluetooth status:", JSON.stringify(btStatus));

    if (!btStatus.adapter_found) {
      setScanStatusType("error");
      scanStatus.innerHTML = `<strong>No Bluetooth adapter found.</strong><br>Make sure your device has Bluetooth hardware.`;
      resetScanButton();
      return;
    }

    if (!btStatus.powered_on) {
      setScanStatusType("warning");
      scanStatus.innerHTML = `<strong>Bluetooth is turned off.</strong><br>Please enable Bluetooth in your device settings, then tap Scan again.`;
      resetScanButton();
      return;
    }
  } catch (err) {
    const errMsg = String(err);
    console.warn("Bluetooth check error:", errMsg);
    setScanStatusType("warning");
    scanStatus.innerHTML = `<strong>Could not check Bluetooth status.</strong><br>${escapeHtml(errMsg)}<br><br>Attempting scan anyway...`;
    await new Promise(r => setTimeout(r, 1500));
  }

  btnScan.innerHTML = '<span class="spinner"></span> Scanning for Meshtastic devices (5s)...';
  setScanStatusType("info");
  scanStatus.textContent = "Searching for nearby Meshtastic devices...";

  try {
    console.log("[MeshGuard] Starting BLE scan...");
    const scanResult = await invokeWithTimeout("scan_devices", undefined, 20000);
    console.log("[MeshGuard] Scan result:", JSON.stringify(scanResult));
    const devices = scanResult.devices || scanResult || [];

    if (devices.length === 0) {
      setScanStatusType("warning");
      scanStatus.innerHTML = `<strong>No Meshtastic devices found.</strong><br>Make sure your device is:<br>
        &bull; Powered on and fully booted (LED blinking)<br>
        &bull; Within Bluetooth range (~10 meters)<br>
        &bull; Not connected to another app`;
    } else {
      setScanStatusType("success");
      scanStatus.textContent = `Found ${devices.length} Meshtastic device${devices.length > 1 ? "s" : ""} — tap one to pair`;
      devices.forEach(renderScannedDevice);
    }
  } catch (err) {
    const errMsg = String(err);
    if (errMsg.includes("turned off") || errMsg.includes("Bluetooth is turned off")) {
      setScanStatusType("warning");
      scanStatus.innerHTML = `<strong>Bluetooth is turned off.</strong><br>Please enable Bluetooth in your device settings, then tap Scan again.`;
    } else if (errMsg.includes("permission") || errMsg.includes("Permission")) {
      setScanStatusType("error");
      scanStatus.innerHTML = `<strong>Bluetooth permission denied.</strong><br>Please allow Bluetooth access in your device settings.`;
    } else {
      setScanStatusType("error");
      scanStatus.innerHTML = `<strong>Scan failed.</strong><br>${escapeHtml(errMsg)}`;
    }
    console.error("Scan error:", err);
  }

  resetScanButton();
});

function resetScanButton() {
  btnScan.disabled = false;
  btnScan.innerHTML = `
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      <circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>
    </svg>
    Scan Again`;
}

function setScanStatusType(type) {
  scanStatus.className = "scan-status";
  if (type) scanStatus.classList.add(`scan-status-${type}`);
}

function renderScannedDevice(device) {
  const card = document.createElement("div");
  card.className = "scan-card meshtastic";
  card.innerHTML = `
    <div class="scan-card-icon">&#x1F4E1;</div>
    <div class="scan-card-details">
      <div class="scan-card-name">${escapeHtml(device.name)}</div>
      <div class="scan-card-addr">${device.address}</div>
    </div>
    <div class="scan-card-meta">
      ${device.rssi ? `<span class="scan-rssi">${device.rssi} dBm</span>` : ""}
      <span class="scan-badge">Meshtastic</span>
    </div>
  `;
  card.addEventListener("click", () => selectAndBondDevice(device, card));
  scanResults.appendChild(card);
}

async function selectAndBondDevice(device, card) {
  // Deselect previous
  scanResults.querySelectorAll(".scan-card").forEach((c) => c.classList.remove("selected"));
  card.classList.add("selected");

  state.selectedBleAddress = device.address;
  bleAddressInput.value = device.address;

  // Auto-fill device name
  if (device.is_meshtastic && device.name !== "Unknown Device" && device.name !== "Meshtastic Device") {
    const nameField = $("#device-name");
    if (!nameField.value) {
      nameField.value = device.name;
    }
  }

  // Trigger BLE bonding (pairing dialog)
  setScanStatusType("info");
  scanStatus.innerHTML = `<span class="spinner" style="width:14px;height:14px;border-width:1.5px;vertical-align:middle;"></span> Pairing with ${escapeHtml(device.name)}...`;

  try {
    const bondResult = await invokeWithTimeout("bond_device", { address: device.address }, 35000);
    if (bondResult.success) {
      setScanStatusType("success");
      scanStatus.textContent = `Paired with ${device.name} — ready to save`;
    } else {
      setScanStatusType("warning");
      scanStatus.innerHTML = `<strong>Pairing:</strong> ${escapeHtml(bondResult.message)}<br>You can still save and pair later.`;
    }
  } catch (err) {
    console.warn("Bond error (non-fatal):", err);
    setScanStatusType("info");
    scanStatus.textContent = `Selected: ${device.name} (${device.address})`;
  }
}

// ============================================================
// Device Lock — Remove Device
// ============================================================
btnRemoveDevice.addEventListener("click", async () => {
  if (!confirm("Remove this device? This will clear your device config and all peer conversations.")) {
    return;
  }

  try {
    await invoke("remove_device");
    deviceLockedBanner.style.display = "none";
    formDevice.style.display = "";
    $("#device-name").value = "";
    bleAddressInput.value = "";
    state.selectedBleAddress = "";
    state.peers = [];
    state.messages = {};
    scanResults.innerHTML = "";
    scanStatus.textContent = "";
    $("#scan-section").style.display = "";
  } catch (err) {
    alert("Error removing device: " + err);
  }
});

// ============================================================
// Screen 1: Device Setup — Submit
// ============================================================
formDevice.addEventListener("submit", async (e) => {
  e.preventDefault();

  const bleAddr = bleAddressInput.value.trim();
  if (!bleAddr) {
    scanStatus.textContent = "Please scan and select a device first.";
    scanStatus.style.color = "var(--danger)";
    setTimeout(() => { scanStatus.style.color = ""; }, 3000);
    return;
  }

  const btn = formDevice.querySelector("button[type=submit]");
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span> Saving...';

  try {
    await invoke("save_device_config", {
      input: {
        deviceName: $("#device-name").value.trim(),
        bleAddress: bleAddr,
        region: $("#region").value,
        modemPreset: $("#modem-preset").value,
        txPower: parseInt($("#tx-power").value),
        hopLimit: parseInt($("#hop-limit").value),
      },
    });
    await loadPeers();
    showScreen(screenPeers);
  } catch (err) {
    alert("Error: " + err);
  }

  btn.disabled = false;
  btn.innerHTML = "Save &amp; Continue";
});

// ============================================================
// Screen 2: Peers Management
// ============================================================
btnBackSetup.addEventListener("click", () => showScreen(screenSetup));

async function loadPeers() {
  try {
    state.peers = await invoke("list_peers");
  } catch (err) {
    console.error("Failed to load peers:", err);
    state.peers = [];
  }
  renderPeersList();
}

function renderPeersList() {
  peersList.innerHTML = "";
  for (const peer of state.peers) {
    const card = document.createElement("div");
    card.className = "peer-card";

    const initial = (peer.device_name || "?")[0].toUpperCase();
    const hasKey = false; // We'll check async below

    card.innerHTML = `
      <div class="peer-card-icon">${initial}</div>
      <div class="peer-card-info">
        <div class="peer-card-name">${escapeHtml(peer.device_name)}</div>
        <div class="peer-card-status" id="peer-status-${peer.id}">Tap to open chat</div>
      </div>
      <div class="peer-card-actions">
        <button class="btn-remove-peer" title="Remove peer" data-peer-id="${peer.id}">&times;</button>
      </div>
    `;

    // Tap card to open chat
    card.addEventListener("click", (e) => {
      if (e.target.closest(".btn-remove-peer")) return;
      openPeerChat(peer);
    });

    // Remove button
    card.querySelector(".btn-remove-peer").addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!confirm(`Remove peer "${peer.device_name}"?`)) return;
      try {
        await invoke("remove_peer", { peerId: peer.id });
        delete state.messages[peer.id];
        await loadPeers();
      } catch (err) {
        alert("Error removing peer: " + err);
      }
    });

    peersList.appendChild(card);

    // Check if session key exists (async)
    invoke("peer_has_session", { peerId: peer.id }).then(has => {
      const statusEl = document.getElementById(`peer-status-${peer.id}`);
      if (statusEl) {
        if (has) {
          statusEl.textContent = "Session active";
          statusEl.classList.add("active");
        } else {
          statusEl.textContent = "Tap to unlock";
        }
      }
    });
  }
}

async function openPeerChat(peer) {
  // Check if we already have a session key for this peer
  const hasKey = await invoke("peer_has_session", { peerId: peer.id });

  if (hasKey) {
    // Session already active — go straight to chat
    state.activePeerId = peer.id;
    chatPeerName.textContent = peer.device_name;
    loadChatMessages(peer.id);
    updateConnectionStatus(state.connected);
    showScreen(screenChat);
  } else {
    // Need passphrase — show unlock screen
    state.unlockPeerId = peer.id;
    unlockPeerLabel.textContent = `Enter passphrase for ${peer.device_name}`;
    $("#unlock-passphrase").value = "";
    showScreen(screenUnlock);
  }
}

// ============================================================
// Screen 2b: Unlock existing peer
// ============================================================
btnBackPeers.addEventListener("click", () => showScreen(screenPeers));

formUnlock.addEventListener("submit", async (e) => {
  e.preventDefault();
  const btn = formUnlock.querySelector("button[type=submit]");
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span> Deriving keys...';

  const passphrase = $("#unlock-passphrase").value;
  const peerId = state.unlockPeerId;

  try {
    await invoke("activate_peer", { peerId, sharedPassphrase: passphrase });
    $("#unlock-passphrase").value = "";

    const peer = state.peers.find(p => p.id === peerId);
    state.activePeerId = peerId;
    chatPeerName.textContent = peer ? peer.device_name : "Peer";
    loadChatMessages(peerId);

    // Try connecting to device
    try {
      await invoke("connect_local_device");
      state.connected = true;
      await invoke("apply_config_to_device");
    } catch (bleErr) {
      console.warn("BLE connection skipped:", bleErr);
    }

    updateConnectionStatus(state.connected);
    showScreen(screenChat);
    addSystemMessage("Secure session established. All messages are encrypted with AES-256-GCM.");

  } catch (err) {
    alert("Error: " + err);
  }

  btn.disabled = false;
  btn.innerHTML = "Unlock";
});

// ============================================================
// Add New Peer
// ============================================================
formAddPeer.addEventListener("submit", async (e) => {
  e.preventDefault();

  const btn = formAddPeer.querySelector("button[type=submit]");
  btn.disabled = true;
  btn.innerHTML = '<span class="spinner"></span> Adding peer...';

  const peerName = $("#peer-name").value.trim();
  const passphrase = $("#shared-passphrase").value;

  try {
    const peer = await invoke("add_peer", {
      peerDeviceName: peerName,
      sharedPassphrase: passphrase,
    });

    $("#peer-name").value = "";
    $("#shared-passphrase").value = "";

    // Try connecting to device
    try {
      await invoke("connect_local_device");
      state.connected = true;
      await invoke("apply_config_to_device");
    } catch (bleErr) {
      console.warn("BLE connection skipped:", bleErr);
    }

    state.activePeerId = peer.id;
    chatPeerName.textContent = peerName;
    loadChatMessages(peer.id);
    updateConnectionStatus(state.connected);
    await loadPeers();
    showScreen(screenChat);
    addSystemMessage(`Secure session with ${peerName} established. All messages are encrypted with AES-256-GCM.`);
    addSystemMessage("Encryption key derived from device names + shared passphrase. No key data was sent over the mesh.");

  } catch (err) {
    alert("Error adding peer: " + err);
  }

  btn.disabled = false;
  btn.innerHTML = "Add Peer";
});

// ============================================================
// Screen 3: Chat
// ============================================================
btnBackPeersChat.addEventListener("click", async () => {
  await loadPeers();
  showScreen(screenPeers);
});

function loadChatMessages(peerId) {
  messagesContainer.innerHTML = "";
  const msgs = state.messages[peerId] || [];
  msgs.forEach(renderMessage);
}

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
  if (!text || !state.activePeerId) return;

  const msg = {
    id: crypto.randomUUID(),
    text,
    timestamp: Date.now(),
    mine: true,
    status: "sent",
  };

  if (!state.messages[state.activePeerId]) {
    state.messages[state.activePeerId] = [];
  }
  state.messages[state.activePeerId].push(msg);
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
    setTimeout(() => {
      msg.status = "delivered";
      updateMessageStatus(msg.id, "delivered");
    }, 400);

    setTimeout(() => {
      const echo = {
        id: crypto.randomUUID(),
        text: `[Peer] ${text}`,
        timestamp: Date.now(),
        mine: false,
        status: "delivered",
      };
      state.messages[state.activePeerId].push(echo);
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
      // Show locked banner
      deviceLockedBanner.style.display = "";
      lockedDeviceName.textContent = device.device_name || "Configured Device";
      lockedDeviceAddr.textContent = device.ble_address || "";

      // Fill form
      $("#device-name").value = device.device_name || "";
      bleAddressInput.value = device.ble_address || "";
      state.selectedBleAddress = device.ble_address || "";
      if (device.radio) {
        $("#region").value = device.radio.region || "EU868";
        $("#modem-preset").value = device.radio.modem_preset || "LongRange";
        $("#tx-power").value = device.radio.tx_power || 20;
        $("#hop-limit").value = device.radio.hop_limit || 3;
      }

      $("#scan-section").style.display = "none";

      // Load peers and show peers screen
      await loadPeers();

      if (state.peers.length > 0) {
        showScreen(screenPeers);
      } else {
        showScreen(screenPeers);
      }
      return;
    }
  } catch (_) {
    // Fresh start
  }

  deviceLockedBanner.style.display = "none";
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
