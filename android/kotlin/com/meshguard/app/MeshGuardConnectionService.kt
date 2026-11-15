package com.meshguard.app

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import androidx.core.app.RemoteInput
import org.json.JSONArray
import org.json.JSONObject

/**
 * MeshGuard's foreground service (ADR-001 option D). It lives in the app
 * process (no android:process), so it shares the Rust engine with the UI.
 *
 * Step 3A, the ADR-001 §6 gate: started from the app, it keeps running when
 * the app is swiped away, and is restarted by the system after the process
 * is killed (START_STICKY). Each start initialises the engine through JNI
 * without Tauri, decodes the shared test frame, and posts a conversation
 * notification with an inline reply. A reply is saved to the outbox through
 * the engine before any radio write. The service does not own the radio
 * link yet (step 3B).
 */
class MeshGuardConnectionService : Service() {

    companion object {
        private const val TAG = "MeshGuardService"
        const val ACTION_START = "com.meshguard.app.action.START"
        const val ACTION_STOP = "com.meshguard.app.action.STOP"
        private const val ACTION_REPLY = "com.meshguard.app.action.REPLY"
        private const val EXTRA_PEER = "peer"
        private const val EXTRA_PEER_NAME = "peerName"
        private const val KEY_REPLY = "reply_text"

        private const val SERVICE_CHANNEL = "meshguard_service"
        private const val MESSAGE_CHANNEL = "meshguard_messages"
        private const val ONGOING_ID = 0x4d47
        private const val TEST_ID = 0x4d48
        private const val TEST_DELAY_MS = 15_000L

        fun start(context: Context) {
            val i = Intent(context, MeshGuardConnectionService::class.java).setAction(ACTION_START)
            androidx.core.content.ContextCompat.startForegroundService(context, i)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, MeshGuardConnectionService::class.java))
        }
    }

    private val handler = Handler(Looper.getMainLooper())
    private var initBits = -1

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopForegroundCompat()
                stopSelf()
                return START_NOT_STICKY
            }
            ACTION_REPLY -> {
                handleReply(intent)
                return START_STICKY
            }
        }
        // ACTION_START, or a restart by the system after the process was
        // killed (null intent): the gate's steps 2-4.
        if (!goForeground()) {
            stopSelf()
            return START_NOT_STICKY
        }
        initEngine()
        handler.removeCallbacksAndMessages(null)
        handler.postDelayed({ postTestConversation() }, TEST_DELAY_MS)
        return START_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        super.onDestroy()
    }

    private fun createChannels() {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (nm.getNotificationChannel(SERVICE_CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    SERVICE_CHANNEL, "Background service", NotificationManager.IMPORTANCE_LOW
                ).apply { description = "Shown while MeshGuard runs in the background" }
            )
        }
        if (nm.getNotificationChannel(MESSAGE_CHANNEL) == null) {
            nm.createNotificationChannel(
                NotificationChannel(
                    MESSAGE_CHANNEL, "MeshGuard Messages", NotificationManager.IMPORTANCE_HIGH
                ).apply {
                    description = "Incoming encrypted mesh messages"
                    lockscreenVisibility = android.app.Notification.VISIBILITY_PRIVATE
                }
            )
        }
    }

    private fun openAppIntent(requestCode: Int): PendingIntent? {
        val launch = packageManager.getLaunchIntentForPackage(packageName) ?: return null
        launch.addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        return PendingIntent.getActivity(
            this, requestCode, launch,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
    }

    /** Start as a connectedDevice foreground service. False if Android refuses. */
    private fun goForeground(): Boolean {
        createChannels()
        val stop = PendingIntent.getService(
            this, 1,
            Intent(this, MeshGuardConnectionService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val notification = NotificationCompat.Builder(this, SERVICE_CHANNEL)
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setContentTitle("MeshGuard is running")
            .setContentText("Background service test")
            .setOngoing(true)
            .setContentIntent(openAppIntent(0))
            .addAction(0, "Stop", stop)
            .build()
        return try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(
                    ONGOING_ID, notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
                )
            } else {
                startForeground(ONGOING_ID, notification)
            }
            true
        } catch (e: Exception) {
            // E.g. Bluetooth permission missing (Android 14), or started
            // from the background where Android does not allow it.
            Log.w(TAG, "Cannot run in the foreground: ${e.javaClass.simpleName}: ${e.message}")
            false
        }
    }

    private fun stopForegroundCompat() {
        stopForeground(STOP_FOREGROUND_REMOVE)
    }

    private fun initEngine() {
        val dir = applicationContext.dataDir
        val vault = Vault.readKey(dir)
        initBits = try {
            CoreBridge.init(dir.absolutePath, vault)
        } catch (e: Throwable) {
            Log.e(TAG, "CoreBridge.init failed: ${e.javaClass.simpleName}: ${e.message}")
            -1
        } finally {
            vault?.fill(0)
        }
        Log.i(TAG, "Engine ready: ${safeVersion()}, init=$initBits")
    }

    private fun safeVersion(): String =
        try { CoreBridge.version() } catch (e: Throwable) { "unavailable (${e.javaClass.simpleName})" }

    private fun canNotify(): Boolean =
        NotificationManagerCompat.from(this).areNotificationsEnabled()

    /** Gate step 3: decode the test frame and post a conversation with a reply box. */
    private fun postTestConversation() {
        if (!canNotify()) {
            Log.w(TAG, "Notifications are off; gate notification not shown")
            return
        }
        val test = try {
            JSONObject(CoreBridge.selfTest())
        } catch (e: Throwable) {
            JSONObject().put("ok", false).put("error", e.javaClass.simpleName)
        }
        val decoded = if (test.optBoolean("ok")) test.optString("text") else null
        val peers = try { JSONArray(CoreBridge.peers()) } catch (e: Throwable) { JSONArray() }
        val peer = peers.optJSONObject(0)

        val summary = when {
            decoded == null -> "Self-test failed: ${test.optString("error")}"
            initBits < 0 -> "Decoded \"$decoded\", but the engine did not start"
            else -> "Decoded \"$decoded\" without the app UI"
        }
        if (peer == null) {
            val n = NotificationCompat.Builder(this, MESSAGE_CHANNEL)
                .setSmallIcon(android.R.drawable.ic_dialog_email)
                .setContentTitle("MeshGuard background test")
                .setContentText("$summary. Pair a contact to test replies.")
                .setStyle(NotificationCompat.BigTextStyle().bigText("$summary. Pair a contact to test replies."))
                .setContentIntent(openAppIntent(TEST_ID))
                .setAutoCancel(true)
                .build()
            notify(TEST_ID, n)
            return
        }
        val node = peer.optInt("node")
        val name = peer.optString("name").ifEmpty { "Contact" }
        val style = NotificationCompat.MessagingStyle(me())
            .setConversationTitle("MeshGuard background test")
            .addMessage("$summary. Reply to queue a message to $name.", System.currentTimeMillis(), sender(name))
        notify(TEST_ID, conversation(node, name, style))
    }

    private fun me(): Person = Person.Builder().setName("You").build()

    private fun sender(name: String): Person = Person.Builder().setName(name).build()

    private fun conversation(
        node: Int,
        name: String,
        style: NotificationCompat.MessagingStyle,
    ): android.app.Notification {
        val replyIntent = Intent(this, MeshGuardConnectionService::class.java)
            .setAction(ACTION_REPLY)
            .putExtra(EXTRA_PEER, node)
            .putExtra(EXTRA_PEER_NAME, name)
        // Mutable: the system adds the typed text to this intent.
        val flags = PendingIntent.FLAG_UPDATE_CURRENT or
            (if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) PendingIntent.FLAG_MUTABLE else 0)
        val replyPending = PendingIntent.getService(this, node, replyIntent, flags)
        val input = RemoteInput.Builder(KEY_REPLY).setLabel("Reply").build()
        val action = NotificationCompat.Action.Builder(
            android.R.drawable.ic_menu_send, "Reply", replyPending
        )
            .addRemoteInput(input)
            .setAllowGeneratedReplies(false)
            .setSemanticAction(NotificationCompat.Action.SEMANTIC_ACTION_REPLY)
            .build()
        return NotificationCompat.Builder(this, MESSAGE_CHANNEL)
            .setSmallIcon(android.R.drawable.ic_dialog_email)
            .setStyle(style)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setContentIntent(openAppIntent(TEST_ID))
            .addAction(action)
            .setOnlyAlertOnce(true)
            .build()
    }

    /** Gate step 3: the inline reply goes to the outbox through the engine. */
    private fun handleReply(intent: Intent) {
        val text = RemoteInput.getResultsFromIntent(intent)?.getCharSequence(KEY_REPLY)?.toString()
        val node = intent.getIntExtra(EXTRA_PEER, 0)
        val name = intent.getStringExtra(EXTRA_PEER_NAME) ?: "Contact"
        if (text.isNullOrBlank() || node == 0) return
        // The process may have been restarted just for this reply.
        initEngine()
        val result = try {
            JSONObject(CoreBridge.enqueueReply(node, text))
        } catch (e: Throwable) {
            JSONObject().put("ok", false).put("error", e.javaClass.simpleName)
        }
        val status = if (result.optBoolean("ok")) {
            "Saved (${result.optString("state")}). It is sent when the app is connected to your radio."
        } else {
            "Not saved: ${result.optString("error")}"
        }
        // Never log the text: it is message content.
        Log.i(TAG, "Reply to $node: ok=${result.optBoolean("ok")}")
        val style = NotificationCompat.MessagingStyle(me())
            .setConversationTitle("MeshGuard background test")
            .addMessage(text, System.currentTimeMillis(), null as Person?)
            .addMessage(status, System.currentTimeMillis(), sender("MeshGuard"))
        // Re-posting ends the reply spinner.
        notify(TEST_ID, conversation(node, name, style))
    }

    @android.annotation.SuppressLint("MissingPermission")
    private fun notify(id: Int, n: android.app.Notification) {
        if (canNotify()) NotificationManagerCompat.from(this).notify(id, n)
    }
}
