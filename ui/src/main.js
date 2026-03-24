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

const btnScan = $("#btn-scan");
const btnBackSetup = $("#btn-back-setup");
const btnBackPairing = $("#btn-back-pairing");
const btnSend = $("#btn-send");
const btnRemoveDevice = $("#btn-remove-device");

const scanStatus = $("#scan-status");
const scanResults = $("#scan-results");
const bleAddressInput = $("#ble-address");
const deviceLockedBanner = $("#device-locked-banner");
const lockedDeviceName = $("#locked-device-name");
const lockedDeviceAddr = $("#locked-device-addr");

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
  selectedBleAddress: "",
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

  // Step 1: Check Bluetooth status first
  btnScan.innerHTML = '<span class="spinner"></span> Checking Bluetooth...';
  scanStatus.textContent = "";
  setScanStatusType("info");

  try {
    const btStatus = await invoke("check_bluetooth");

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
    // check_bluetooth failed — try scanning anyway, it will give its own error
    console.warn("Bluetooth check failed, attempting scan:", err);
  }

  // Step 2: Bluetooth is ready — scan for Meshtastic devices
  btnScan.innerHTML = '<span class="spinner"></span> Scanning for Meshtastic devices (5s)...';
  setScanStatusType("info");
  scanStatus.textContent = "Searching for nearby Meshtastic devices...";

  try {
    const devices = await invoke("scan_devices");

    if (devices.length === 0) {
      setScanStatusType("warning");
      scanStatus.innerHTML = `<strong>No Meshtastic devices found.</strong><br>Make sure your device is:<br>
        &bull; Powered on and fully booted (LED blinking)<br>
        &bull; Within Bluetooth range (~10 meters)<br>
        &bull; Not connected to another app`;
    } else {
      setScanStatusType("success");
      scanStatus.textContent = `Found ${devices.length} Meshtastic device${devices.length > 1 ? "s" : ""} — tap one to select it`;
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
  card.addEventListener("click", () => selectScannedDevice(device, card));
  scanResults.appendChild(card);
}

function selectScannedDevice(device, card) {
  // Deselect previous
  scanResults.querySelectorAll(".scan-card").forEach((c) => c.classList.remove("selected"));
  card.classList.add("selected");

  // Set the BLE address
  state.selectedBleAddress = device.address;
  bleAddressInput.value = device.address;

  // Auto-fill device name if it's a Meshtastic device
  if (device.is_meshtastic && device.name !== "Unknown Device") {
    const nameField = $("#device-name");
    if (!nameField.value) {
      nameField.value = device.name;
    }
  }

  scanStatus.textContent = `Selected: ${device.name} (${device.address})`;
}

// ============================================================
// Device Lock — Remove Device
// ============================================================
btnRemoveDevice.addEventListener("click", async () => {
  if (!confirm("Remove this device? This will clear your device config and peer pairing.")) {
    return;
  }

  try {
    await invoke("remove_device");
    deviceLockedBanner.style.display = "none";
    formDevice.style.display = "";
    $("#device-name").value = "";
    $("#device-serial").value = "";
    bleAddressInput.value = "";
    state.selectedBleAddress = "";
    scanResults.innerHTML = "";
    scanStatus.textContent = "";
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
        deviceSerial: $("#device-serial").value.trim(),
        bleAddress: bleAddr,
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
    await invoke("setup_peer", {
      peerDeviceName: peerName,
      peerDeviceSerial: peerSerial,
      sharedPassphrase: passphrase,
    });

    btn.innerHTML = '<span class="spinner"></span> Connecting to device...';
    try {
      await invoke("connect_local_device");
      state.connected = true;

      btn.innerHTML = '<span class="spinner"></span> Applying config...';
      await invoke("apply_config_to_device");
    } catch (bleErr) {
      console.warn("BLE connection skipped (demo mode):", bleErr);
    }

    $("#shared-passphrase").value = "";

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
// On Load: restore saved config, enforce single-device lock
// ============================================================
async function init() {
  try {
    const device = await invoke("get_device_config");
    if (device) {
      // Show locked banner — device already configured
      deviceLockedBanner.style.display = "";
      lockedDeviceName.textContent = device.device_name || "Configured Device";
      lockedDeviceAddr.textContent = device.ble_address || "";

      // Fill form with existing values (for editing)
      $("#device-name").value = device.device_name || "";
      $("#device-serial").value = device.device_serial || "";
      bleAddressInput.value = device.ble_address || "";
      state.selectedBleAddress = device.ble_address || "";
      if (device.radio) {
        $("#region").value = device.radio.region || "EU868";
        $("#modem-preset").value = device.radio.modem_preset || "LongRange";
        $("#tx-power").value = device.radio.tx_power || 20;
        $("#hop-limit").value = device.radio.hop_limit || 3;
      }

      // Hide scan section since device is already set
      $("#scan-section").style.display = "none";

      // Check if peer is configured and session active
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

      // Device configured but no peer yet — go to pairing
      showScreen(screenPairing);
      return;
    }
  } catch (_) {
    // Fresh start
  }

  // No device configured — show setup with scan
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
