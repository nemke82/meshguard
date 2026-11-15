package com.meshguard.app

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Vault key wrapping (ADR-002), shared by the Tauri plugin and the
 * background service.
 *
 * A non-exportable AES-256-GCM key in the Android Keystore wraps the random
 * vault key that protects remembered chat keys and the message history.
 * The wrapped bytes are iv(12) || ciphertext+tag, stored by the Rust side in
 * `vault.key` in the app's data directory. The key is not bound to user
 * authentication, so the app can work in the background.
 */
object Vault {
    // Android Keystore alias of the key that wraps MeshGuard's vault key.
    private const val ALIAS = "meshguard_vault_v1"
    private const val GCM_IV_LEN = 12
    const val FILE_NAME = "vault.key"

    /** Thrown for a missing key or a malformed blob; the message is checked by Rust. */
    class VaultException(message: String) : Exception(message)

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(
                ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    fun wrap(plain: ByteArray): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key())
        return cipher.iv + cipher.doFinal(plain)
    }

    /** Never creates a key: without one, nothing wrapped can be opened. */
    fun unwrap(blob: ByteArray): ByteArray {
        if (blob.size <= GCM_IV_LEN) throw VaultException("blob too short")
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val key = ks.getKey(ALIAS, null) as? SecretKey ?: throw VaultException("no vault key")
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(
            Cipher.DECRYPT_MODE, key,
            GCMParameterSpec(128, blob.copyOfRange(0, GCM_IV_LEN))
        )
        return cipher.doFinal(blob.copyOfRange(GCM_IV_LEN, blob.size))
    }

    /** The vault key from `dataDir/vault.key`, or null if the app has none yet. */
    fun readKey(dataDir: File): ByteArray? {
        val f = File(dataDir, FILE_NAME)
        if (!f.exists()) return null
        return try {
            unwrap(f.readBytes())
        } catch (e: Exception) {
            android.util.Log.w("MeshGuardVault", "Vault key not usable: ${e.javaClass.simpleName}")
            null
        }
    }
}
