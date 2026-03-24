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
    \n    <uses-feature android:name="android.hardware.bluetooth_le" android:required="true" />\
    \n    <application|' "$MANIFEST"
  echo "  -> BLE permissions added"
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
            // Android 12+
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.BLUETOOTH_SCAN) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.BLUETOOTH_SCAN)
            }
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.BLUETOOTH_CONNECT) != PackageManager.PERMISSION_GRANTED) {
                permissions.add(Manifest.permission.BLUETOOTH_CONNECT)
            }
        }

        // Location permission is required for BLE scanning on Android < 12
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
import android.bluetooth.BluetoothManager
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.ParcelUuid
import android.util.Log
import androidx.core.content.ContextCompat
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

@TauriPlugin
class BlePlugin(private val activity: android.app.Activity) : Plugin(activity) {

    companion object {
        private const val TAG = "BlePlugin"
        private val MESHTASTIC_SERVICE_UUID: UUID =
            UUID.fromString("6ba1b218-15a8-461f-9fa8-5dcae273eafd")
        private val MESHTASTIC_NAME_HINTS = listOf(
            "meshtastic", "p1000", "t-beam", "heltec", "rak",
            "sensecap", "t-echo", "lora", "mesh"
        )
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

        // Check permissions
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

                // Only include Meshtastic devices
                if (!isMeshtastic) return

                val displayName = name.ifEmpty { "Meshtastic Device" }

                val device = JSONObject()
                device.put("name", displayName)
                device.put("address", address)
                device.put("rssi", result.rssi)
                device.put("is_meshtastic", true)

                // Keep strongest signal per address
                val existing = foundDevices[address]
                if (existing == null || result.rssi > existing.optInt("rssi", -999)) {
                    foundDevices[address] = device
                }
            }

            override fun onScanFailed(errorCode: Int) {
                Log.e(TAG, "BLE scan failed with error code: $errorCode")
            }
        }

        // Scan with both: service UUID filter AND general scan (some devices
        // don't advertise the service UUID in their scan response)
        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .build()

        try {
            // Start general scan (filter by name in callback)
            scanner.startScan(null, settings, callback)
            Log.d(TAG, "BLE scan started")
        } catch (e: SecurityException) {
            invoke.reject("Bluetooth permission denied: ${e.message}")
            return
        }

        // Stop after 5 seconds and return results
        Handler(Looper.getMainLooper()).postDelayed({
            try {
                scanner.stopScan(callback)
            } catch (e: SecurityException) {
                Log.w(TAG, "Could not stop scan: ${e.message}")
            }

            // Sort by RSSI (strongest first)
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

    // ── Helpers ─────────────────────────────────────────────────

    private fun isMeshtasticDevice(name: String, result: ScanResult): Boolean {
        // Check service UUIDs
        val serviceUuids = result.scanRecord?.serviceUuids
        if (serviceUuids != null) {
            for (uuid in serviceUuids) {
                if (uuid.uuid == MESHTASTIC_SERVICE_UUID) return true
            }
        }

        // Check name
        val lower = name.lowercase()
        for (hint in MESHTASTIC_NAME_HINTS) {
            if (lower.contains(hint)) return true
        }

        return false
    }

    private fun hasScanPermission(): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Android 12+
            return ContextCompat.checkSelfPermission(
                activity, android.Manifest.permission.BLUETOOTH_SCAN
            ) == PackageManager.PERMISSION_GRANTED
        }
        // Below Android 12 — location permission needed for BLE scan
        return ContextCompat.checkSelfPermission(
            activity, android.Manifest.permission.ACCESS_FINE_LOCATION
        ) == PackageManager.PERMISSION_GRANTED
    }
}
KOTLIN

echo "  -> BlePlugin.kt injected at $PLUGIN_DIR/BlePlugin.kt"
echo "Android BLE patches complete."
