package com.meshguard.app

/**
 * The Rust engine, reached through JNI (ADR-001 option D). Loading the
 * library only loads code; it does not start Tauri. The Tauri UI and the
 * service share one engine per process.
 *
 * Results are JSON strings. Decrypted content passes through only for
 * notifications and is never logged.
 */
object CoreBridge {
    init {
        System.loadLibrary("meshguard_lib")
    }

    /** `init` result bits. */
    const val INIT_HISTORY_OPEN = 1
    const val INIT_KEYS_RESTORED = 2

    external fun version(): String

    /**
     * Open the engine on the app's data directory with the vault key (null
     * if the app has not created one yet). Idempotent. Returns INIT_* bits,
     * or -1 on failure.
     */
    external fun init(dataDir: String, vaultKey: ByteArray?): Int

    /** Decode the shared v2 fixture frame: `{"ok":true,"text":...}`. */
    external fun selfTest(): String

    /** Paired contacts with a chat key: `[{"node":n,"name":"..."}]`. */
    external fun peers(): String

    /** Queue a reply in the outbox: `{"ok":true,"messageId","state"}` or `{"ok":false,"error"}`. */
    external fun enqueueReply(peerNode: Int, text: String): String
}
