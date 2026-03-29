#!/usr/bin/env bash
# Patch the generated Android project to add BLE permissions, runtime permission
# requests, and the native BLE scanner plugin (Kotlin).
# Run this AFTER `cargo tauri android init` and BEFORE `cargo tauri android build`.
set -euo pipefail

ANDROID_DIR="src-tauri/gen/android"
MANIFEST="$ANDROID_DIR/app/src/main/AndroidManifest.xml"

if [ ! -f "$MANIFEST" ]; then
  echo "ERROR: $MANIFEST not found. Run 'cargo tauri android init' first."
  exit 1
fi

# ── 1. BLE Permissions ───────────────────────────────────────────
echo "Patching AndroidManifest.xml with BLE permissions..."

if ! grep -q "BLUETOOTH_SCAN" "$MANIFEST"; then
  sed -i 's|<application|<!-- BLE permissions for Meshtastic device communication -->\
    <uses-permission android:name="android.permission.BLUETOOTH" />\
    <uses-permission android:name="android.permission.BLUETOOTH_ADMIN" />\
    <uses-permission android:name="android.permission.BLUETOOTH_SCAN" android:usesPermissionFlags="neverForLocation" />\
    <uses-permission android:name="android.permission.BLUETOOTH_CONNECT" />\
    <uses-permission android:name="android.permission.ACCESS_FINE_LOCATION" />\
    <uses-permission android:name="android.permission.ACCESS_COARSE_LOCATION" />\
    <uses-permission android:name="android.permission.POST_NOTIFICATIONS" />\
    <uses-permission android:name="android.permission.VIBRATE" />\
    \n    <uses-feature android:name="android.hardware.bluetooth_le" android:required="true" />\
    \n    <application|' "$MANIFEST"
  echo "  -> BLE + notification permissions added"
else
  echo "  -> BLE permissions already present"
fi

# ── 2. MainActivity — runtime permission request ─────────────────
MAIN_ACTIVITY=$(find "$ANDROID_DIR" -name "MainActivity.kt" -type f | head -1)

if [ -z "$MAIN_ACTIVITY" ]; then
  echo "WARNING: MainActivity.kt not found, skipping runtime permission patch"
  exit 0
fi

PLUGIN_DIR=$(dirname "$MAIN_ACTIVITY")

if ! grep -q "requestBlePermissions" "$MAIN_ACTIVITY"; then
  echo "Patching MainActivity.kt with runtime BLE permission request..."

  cat > "$MAIN_ACTIVITY" << 'KOTLIN'
package com.meshguard.app

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {

    private val BLE_PERMISSION_REQUEST_CODE = 1001

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        requestBlePermissions()
    }

    private fun requestBlePermissions() {
        val permissions = mutableListOf<String>()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.BLUETOOTH_SCAN) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.BLUETOOTH_SCAN)
            }
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.BLUETOOTH_CONNECT)
            }
        }

        if (Build.VERSION.SDK_INT >= 33) {
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        if (ContextCompat.checkSelfPermission(this, Manifest.permission.ACCESS_FINE_LOCATION) != PackageManager.PERMISSION_GRANTED) {
            permissions.add(Manifest.permission.ACCESS_FINE_LOCATION)
        }

        if (permissions.isNotEmpty()) {
            ActivityCompat.requestPermissions(this, permissions.toTypedArray(), BLE_PERMISSION_REQUEST_CODE)
        }
    }
}
KOTLIN

  echo "  -> Runtime permission request added"
else
  echo "  -> Runtime permission request already present"
fi

# ── 3. BlePlugin.kt — native Android BLE scanner ────────────────
echo "Injecting BlePlugin.kt (native Android BLE scanner)..."

cat > "$PLUGIN_DIR/BlePlugin.kt" << 'KOTLIN'
package com.meshguard.app

import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothManager
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.json.JSONObject
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothProfile
import android.util.Base64
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent

@TauriPlugin
class BlePlugin(private val activity: android.app.Activity) : Plugin(activity) {

    companion object {
        private const val TAG = "BlePlugin"
        private val MESHTASTIC_SERVICE_UUID: UUID =
            UUID.fromString("6ba1b218-15a8-461f-9fa8-5dcae273eafd")
        private val FROMRADIO_UUID: UUID =
            UUID.fromString("2c55e69e-4993-11ed-b878-0242ac120002")
        private val TORADIO_UUID: UUID =
            UUID.fromString("f75c76d2-129e-4dad-a1dd-7866124401e7")
        private val MESHTASTIC_NAME_HINTS = listOf(
            "meshtastic", "p1000", "t-beam", "heltec", "rak",
            "sensecap", "t-echo", "lora", "mesh"
        )
    }

    private var bluetoothGatt: BluetoothGatt? = null
    private var fromRadioChar: BluetoothGattCharacteristic? = null
    private var toRadioChar: BluetoothGattCharacteristic? = null

    private val connectQueue = LinkedBlockingQueue<Int>(1)
    private val servicesQueue = LinkedBlockingQueue<Int>(1)
    private val readQueue = LinkedBlockingQueue<ByteArray>(1)
    private val writeQueue = LinkedBlockingQueue<Int>(1)

    private val gattCallback = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(gatt: BluetoothGatt, status: Int, newState: Int) {
            Log.d(TAG, "GATT state: $newState (status=$status)")
            connectQueue.offer(newState)
        }

        override fun onServicesDiscovered(gatt: BluetoothGatt, status: Int) {
            Log.d(TAG, "Services discovered: status=$status")
            servicesQueue.offer(status)
        }

        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int
        ) {
            readQueue.offer(if (status == BluetoothGatt.GATT_SUCCESS) value else ByteArray(0))
        }

        @Deprecated("Deprecated in API 33")
        override fun onCharacteristicRead(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            status: Int
        ) {
            @Suppress("DEPRECATION")
            readQueue.offer(
                if (status == BluetoothGatt.GATT_SUCCESS) characteristic.value ?: ByteArray(0)
                else ByteArray(0)
            )
        }

        override fun onCharacteristicWrite(
            gatt: BluetoothGatt,
            characteristic: BluetoothGattCharacteristic,
            status: Int
        ) {
            writeQueue.offer(status)
        }
    }

    // ── check_bluetooth ─────────────────────────────────────────

    @Command
    fun checkBluetooth(invoke: Invoke) {
        val result = JSObject()

        val btManager = activity.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
        val adapter = btManager?.adapter

        if (adapter == null) {
            result.put("adapter_found", false)
            result.put("powered_on", false)
            result.put("message", "No Bluetooth adapter found on this device.")
            invoke.resolve(result)
            return
        }

        if (!adapter.isEnabled) {
            result.put("adapter_found", true)
            result.put("powered_on", false)
            result.put("message", "Bluetooth is turned off. Please enable Bluetooth in your device settings.")
            invoke.resolve(result)
            return
        }

        if (!hasScanPermission()) {
            result.put("adapter_found", true)
            result.put("powered_on", false)
            result.put("message", "Bluetooth permission not granted. Please allow Bluetooth access.")
            invoke.resolve(result)
            return
        }

        result.put("adapter_found", true)
        result.put("powered_on", true)
        result.put("message", "Bluetooth is ready.")
        invoke.resolve(result)
    }

    // ── scan_devices ────────────────────────────────────────────

    @SuppressLint("MissingPermission")
    @Command
    fun scanDevices(invoke: Invoke) {
        val btManager = activity.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
        val adapter = btManager?.adapter

        if (adapter == null) {
            invoke.reject("No Bluetooth adapter found.")
            return
        }

        if (!adapter.isEnabled) {
            invoke.reject("Bluetooth is turned off. Please enable Bluetooth in your device settings.")
            return
        }

        if (!hasScanPermission()) {
            invoke.reject("Bluetooth permission not granted. Please allow Bluetooth access.")
            return
        }

        val scanner: BluetoothLeScanner? = adapter.bluetoothLeScanner
        if (scanner == null) {
            invoke.reject("BLE scanner not available. Is Bluetooth enabled?")
            return
        }

        val foundDevices = mutableMapOf<String, JSONObject>()

        val callback = object : ScanCallback() {
            override fun onScanResult(callbackType: Int, result: ScanResult) {
                val address = result.device.address ?: return
                val name = try {
                    result.device.name
                } catch (e: SecurityException) {
                    null
                } ?: result.scanRecord?.deviceName ?: ""

                val isMeshtastic = isMeshtasticDevice(name, result)
                if (!isMeshtastic) return

                val displayName = name.ifEmpty { "Meshtastic Device" }

                val device = JSONObject()
                device.put("name", displayName)
                device.put("address", address)
                device.put("rssi", result.rssi)
                device.put("is_meshtastic", true)

                val existing = foundDevices[address]
                if (existing == null || result.rssi > existing.optInt("rssi", -999)) {
                    foundDevices[address] = device
                }
            }

            override fun onScanFailed(errorCode: Int) {
                Log.e(TAG, "BLE scan failed with error code: $errorCode")
            }
        }

        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .build()

        try {
            scanner.startScan(null, settings, callback)
            Log.d(TAG, "BLE scan started")
        } catch (e: SecurityException) {
            invoke.reject("Bluetooth permission denied: ${e.message}")
            return
        }

        Handler(Looper.getMainLooper()).postDelayed({
            try {
                scanner.stopScan(callback)
            } catch (e: SecurityException) {
                Log.w(TAG, "Could not stop scan: ${e.message}")
            }

            val sorted = foundDevices.values.sortedByDescending { it.optInt("rssi", -100) }
            val devicesArray = JSONArray()
            for (d in sorted) {
                devicesArray.put(d)
            }

            val result = JSObject()
            result.put("devices", devicesArray)

            Log.d(TAG, "BLE scan complete: ${sorted.size} Meshtastic device(s) found")
            invoke.resolve(result)
        }, 5000)
    }

    // ── bond_device — trigger system BLE pairing dialog ─────────

    @SuppressLint("MissingPermission")
    @Command
    fun bondDevice(invoke: Invoke) {
        val addressStr: String = invoke.getArgs().getString("address", "") ?: ""
        if (addressStr.isEmpty()) {
            invoke.reject("No device address provided.")
            return
        }

        val btManager = activity.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
        val adapter = btManager?.adapter
        if (adapter == null) {
            invoke.reject("No Bluetooth adapter found.")
            return
        }

        if (!hasConnectPermission()) {
            val result = JSObject()
            result.put("success", false)
            result.put("message", "BLUETOOTH_CONNECT permission not granted.")
            invoke.resolve(result)
            return
        }

        val remoteDevice: BluetoothDevice
        try {
            remoteDevice = adapter.getRemoteDevice(addressStr)
        } catch (e: IllegalArgumentException) {
            invoke.reject("Invalid Bluetooth address: $addressStr")
            return
        }

        // Already bonded?
        if (remoteDevice.bondState == BluetoothDevice.BOND_BONDED) {
            val result = JSObject()
            result.put("success", true)
            result.put("message", "Device is already paired.")
            invoke.resolve(result)
            return
        }

        // Register receiver to listen for bond state changes
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (intent.action != BluetoothDevice.ACTION_BOND_STATE_CHANGED) return
                val device: BluetoothDevice? = if (Build.VERSION.SDK_INT >= 33) {
                    intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE, BluetoothDevice::class.java)
                } else {
                    @Suppress("DEPRECATION")
                    intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE)
                }
                if (device == null) return
                if (device.address != addressStr) return

                val bondState = intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.BOND_NONE)
                val prevState = intent.getIntExtra(BluetoothDevice.EXTRA_PREVIOUS_BOND_STATE, BluetoothDevice.BOND_NONE)

                Log.d(TAG, "Bond state changed: $prevState -> $bondState for ${device.address}")

                when (bondState) {
                    BluetoothDevice.BOND_BONDED -> {
                        try { activity.unregisterReceiver(this) } catch (_: Exception) {}
                        val result = JSObject()
                        result.put("success", true)
                        result.put("message", "Device paired successfully.")
                        invoke.resolve(result)
                    }
                    BluetoothDevice.BOND_NONE -> {
                        if (prevState == BluetoothDevice.BOND_BONDING) {
                            try { activity.unregisterReceiver(this) } catch (_: Exception) {}
                            val result = JSObject()
                            result.put("success", false)
                            result.put("message", "Pairing was rejected or failed.")
                            invoke.resolve(result)
                        }
                    }
                }
            }
        }

        val filter = IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED)
        activity.registerReceiver(receiver, filter)

        // Initiate bonding — this shows the system pairing dialog
        val started = remoteDevice.createBond()
        if (!started) {
            try { activity.unregisterReceiver(receiver) } catch (_: Exception) {}
            val result = JSObject()
            result.put("success", false)
            result.put("message", "Failed to initiate pairing. Try again.")
            invoke.resolve(result)
            return
        }

        Log.d(TAG, "Bonding initiated for $addressStr — waiting for system dialog...")

        // Timeout after 30 seconds
        Handler(Looper.getMainLooper()).postDelayed({
            try { activity.unregisterReceiver(receiver) } catch (_: Exception) {}
            if (remoteDevice.bondState != BluetoothDevice.BOND_BONDED) {
                val result = JSObject()
                result.put("success", false)
                result.put("message", "Pairing timed out. Please try again.")
                invoke.resolve(result)
            }
        }, 30000)
    }

    // ── connect_device — GATT connect + service discovery ─────

    @SuppressLint("MissingPermission")
    @Command
    fun connectDevice(invoke: Invoke) {
        val address = invoke.getArgs().getString("address") ?: ""
        if (address.isEmpty()) {
            invoke.reject("No address provided")
            return
        }

        val btManager = activity.getSystemService(Context.BLUETOOTH_SERVICE) as? BluetoothManager
        val adapter = btManager?.adapter
        if (adapter == null) {
            invoke.reject("No Bluetooth adapter")
            return
        }

        // Close any existing connection
        bluetoothGatt?.let {
            try { it.disconnect() } catch (_: Exception) {}
            try { it.close() } catch (_: Exception) {}
        }
        bluetoothGatt = null
        fromRadioChar = null
        toRadioChar = null

        val device: BluetoothDevice
        try {
            device = adapter.getRemoteDevice(address)
        } catch (e: IllegalArgumentException) {
            invoke.reject("Invalid address: $address")
            return
        }

        // Bond if not already bonded (system pairing dialog will appear)
        if (device.bondState != BluetoothDevice.BOND_BONDED) {
            Log.d(TAG, "Initiating bonding with $address...")
            val bondLatch = CountDownLatch(1)
            val receiver = object : BroadcastReceiver() {
                override fun onReceive(context: Context, intent: Intent) {
                    if (intent.action != BluetoothDevice.ACTION_BOND_STATE_CHANGED) return
                    val d = if (Build.VERSION.SDK_INT >= 33) {
                        intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE, BluetoothDevice::class.java)
                    } else {
                        @Suppress("DEPRECATION")
                        intent.getParcelableExtra(BluetoothDevice.EXTRA_DEVICE)
                    }
                    if (d?.address != address) return
                    val state = intent.getIntExtra(BluetoothDevice.EXTRA_BOND_STATE, BluetoothDevice.BOND_NONE)
                    if (state != BluetoothDevice.BOND_BONDING) {
                        bondLatch.countDown()
                    }
                }
            }
            activity.registerReceiver(receiver, IntentFilter(BluetoothDevice.ACTION_BOND_STATE_CHANGED))

            if (!device.createBond()) {
                try { activity.unregisterReceiver(receiver) } catch (_: Exception) {}
                invoke.reject("Failed to start bonding")
                return
            }

            bondLatch.await(30, TimeUnit.SECONDS)
            try { activity.unregisterReceiver(receiver) } catch (_: Exception) {}

            if (device.bondState != BluetoothDevice.BOND_BONDED) {
                invoke.reject("Bonding failed or was rejected. Enter the PIN shown in MeshGuard when the system dialog appears.")
                return
            }
            Log.d(TAG, "Bonded successfully with $address")
        }

        // Connect GATT
        Log.d(TAG, "Connecting GATT to $address...")
        connectQueue.clear()
        servicesQueue.clear()

        bluetoothGatt = device.connectGatt(activity, false, gattCallback, BluetoothDevice.TRANSPORT_LE)

        val connState = connectQueue.poll(15, TimeUnit.SECONDS)
        if (connState != BluetoothProfile.STATE_CONNECTED) {
            bluetoothGatt?.close()
            bluetoothGatt = null
            invoke.reject("GATT connection timed out")
            return
        }
        Log.d(TAG, "GATT connected, discovering services...")

        bluetoothGatt?.discoverServices()
        val svcStatus = servicesQueue.poll(10, TimeUnit.SECONDS)
        if (svcStatus != BluetoothGatt.GATT_SUCCESS) {
            bluetoothGatt?.disconnect()
            bluetoothGatt?.close()
            bluetoothGatt = null
            invoke.reject("Service discovery failed")
            return
        }

        // Find Meshtastic characteristics
        val gatt = bluetoothGatt!!
        for (service in gatt.services) {
            if (service.uuid == MESHTASTIC_SERVICE_UUID) {
                for (c in service.characteristics) {
                    when (c.uuid) {
                        FROMRADIO_UUID -> fromRadioChar = c
                        TORADIO_UUID -> toRadioChar = c
                    }
                }
            }
        }

        if (fromRadioChar == null || toRadioChar == null) {
            bluetoothGatt?.disconnect()
            bluetoothGatt?.close()
            bluetoothGatt = null
            invoke.reject("Meshtastic GATT characteristics not found")
            return
        }

        Log.d(TAG, "GATT ready — fromradio and toradio found")
        val result = JSObject()
        result.put("success", true)
        invoke.resolve(result)
    }

    // ── read_from_radio ─────────────────────────────────────────

    @SuppressLint("MissingPermission")
    @Command
    fun readFromRadio(invoke: Invoke) {
        val gatt = bluetoothGatt
        val char = fromRadioChar
        if (gatt == null || char == null) {
            invoke.resolve(JSObject().put("data", ""))
            return
        }

        readQueue.clear()
        @Suppress("DEPRECATION")
        if (!gatt.readCharacteristic(char)) {
            invoke.resolve(JSObject().put("data", ""))
            return
        }

        val data = readQueue.poll(5, TimeUnit.SECONDS)
        if (data != null && data.isNotEmpty()) {
            invoke.resolve(JSObject().put("data", Base64.encodeToString(data, Base64.NO_WRAP)))
        } else {
            invoke.resolve(JSObject().put("data", ""))
        }
    }

    // ── write_to_radio ──────────────────────────────────────────

    @SuppressLint("MissingPermission")
    @Command
    fun writeToRadio(invoke: Invoke) {
        val dataB64 = invoke.getArgs().getString("data") ?: ""
        if (dataB64.isEmpty()) {
            invoke.reject("No data provided")
            return
        }

        val gatt = bluetoothGatt
        val char = toRadioChar
        if (gatt == null || char == null) {
            invoke.reject("Not connected")
            return
        }

        val data = Base64.decode(dataB64, Base64.NO_WRAP)
        writeQueue.clear()

        @Suppress("DEPRECATION")
        char.value = data
        @Suppress("DEPRECATION")
        char.writeType = BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
        @Suppress("DEPRECATION")
        if (!gatt.writeCharacteristic(char)) {
            invoke.reject("Write initiation failed")
            return
        }

        val status = writeQueue.poll(5, TimeUnit.SECONDS)
        if (status == BluetoothGatt.GATT_SUCCESS) {
            invoke.resolve(JSObject().put("success", true))
        } else {
            invoke.reject("Write failed: status=$status")
        }
    }

    // ── disconnect_ble_device ───────────────────────────────────

    @SuppressLint("MissingPermission")
    @Command
    fun disconnectBleDevice(invoke: Invoke) {
        bluetoothGatt?.let {
            try { it.disconnect() } catch (_: Exception) {}
            try { it.close() } catch (_: Exception) {}
        }
        bluetoothGatt = null
        fromRadioChar = null
        toRadioChar = null
        Log.d(TAG, "BLE device disconnected")
        invoke.resolve(JSObject().put("success", true))
    }

    // ── show_notification — native Android notification ────────

    @Command
    fun showNotification(invoke: Invoke) {
        val title = invoke.getArgs().getString("title") ?: "MeshGuard"
        val body = invoke.getArgs().getString("body") ?: ""
        val nodeNum = invoke.getArgs().getInt("nodeNum", 0)

        val channelId = "meshguard_messages"
        val notifManager = activity.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

        // Create notification channel (required Android 8.0+)
        if (notifManager.getNotificationChannel(channelId) == null) {
            val channel = NotificationChannel(
                channelId,
                "MeshGuard Messages",
                NotificationManager.IMPORTANCE_HIGH
            ).apply {
                description = "Incoming encrypted mesh messages"
                enableVibration(true)
                vibrationPattern = longArrayOf(0, 250, 100, 250)
                setShowBadge(true)
                lockscreenVisibility = Notification.VISIBILITY_PRIVATE
            }
            notifManager.createNotificationChannel(channel)
        }

        // Build the notification
        val launchIntent = activity.packageManager.getLaunchIntentForPackage(activity.packageName)
        val pendingIntent = if (launchIntent != null) {
            launchIntent.addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
            PendingIntent.getActivity(
                activity, nodeNum, launchIntent,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
            )
        } else null

        val notification = Notification.Builder(activity, channelId)
            .setSmallIcon(android.R.drawable.ic_dialog_email)
            .setContentTitle(title)
            .setContentText(body)
            .setAutoCancel(true)
            .setCategory(Notification.CATEGORY_MESSAGE)
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .apply { if (pendingIntent != null) setContentIntent(pendingIntent) }
            .build()

        // Check permission (Android 13+)
        if (Build.VERSION.SDK_INT >= 33) {
            if (ContextCompat.checkSelfPermission(activity, android.Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED) {
                Log.w(TAG, "POST_NOTIFICATIONS permission not granted")
                invoke.resolve(JSObject().put("shown", false))
                return
            }
        }

        notifManager.notify(nodeNum, notification)
        Log.d(TAG, "Notification shown: $title — $body")
        invoke.resolve(JSObject().put("shown", true))
    }

    // ── Helpers ─────────────────────────────────────────────────

    private fun isMeshtasticDevice(name: String, result: ScanResult): Boolean {
        val serviceUuids = result.scanRecord?.serviceUuids
        if (serviceUuids != null) {
            for (uuid in serviceUuids) {
                if (uuid.uuid == MESHTASTIC_SERVICE_UUID) return true
            }
        }

        val lower = name.lowercase()
        for (hint in MESHTASTIC_NAME_HINTS) {
            if (lower.contains(hint)) return true
        }

        return false
    }

    private fun hasScanPermission(): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            return ContextCompat.checkSelfPermission(
                activity, android.Manifest.permission.BLUETOOTH_SCAN
            ) == PackageManager.PERMISSION_GRANTED
        }
        return ContextCompat.checkSelfPermission(
            activity, android.Manifest.permission.ACCESS_FINE_LOCATION
        ) == PackageManager.PERMISSION_GRANTED
    }

    private fun hasConnectPermission(): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            return ContextCompat.checkSelfPermission(
                activity, android.Manifest.permission.BLUETOOTH_CONNECT
            ) == PackageManager.PERMISSION_GRANTED
        }
        return true
    }
}
KOTLIN

echo "  -> BlePlugin.kt injected at $PLUGIN_DIR/BlePlugin.kt"
echo "Android BLE patches complete."
