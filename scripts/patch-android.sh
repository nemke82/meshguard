#!/usr/bin/env bash
# Patch the generated Android project to add BLE permissions and runtime permission requests.
# Run this AFTER `cargo tauri android init` and BEFORE `cargo tauri android build`.
set -euo pipefail

ANDROID_DIR="src-tauri/gen/android"
MANIFEST="$ANDROID_DIR/app/src/main/AndroidManifest.xml"

if [ ! -f "$MANIFEST" ]; then
  echo "ERROR: $MANIFEST not found. Run 'cargo tauri android init' first."
  exit 1
fi

echo "Patching AndroidManifest.xml with BLE permissions..."

# Add BLE permissions before the <application> tag if not already present
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

# Now patch the MainActivity to request runtime permissions on launch
MAIN_ACTIVITY=$(find "$ANDROID_DIR" -name "MainActivity.kt" -type f | head -1)

if [ -z "$MAIN_ACTIVITY" ]; then
  echo "WARNING: MainActivity.kt not found, skipping runtime permission patch"
  exit 0
fi

if grep -q "requestBlePermissions" "$MAIN_ACTIVITY"; then
  echo "  -> Runtime permission request already present"
  exit 0
fi

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

echo "  -> Runtime permission request added to MainActivity.kt"
echo "Android BLE patches complete."
