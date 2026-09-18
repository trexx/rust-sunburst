// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.os.Bundle
import android.text.InputType
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.concurrent.thread

/**
 * Pairing from the TV. The client generates a PIN and shows it; the user arms
 * pairing in the web UI and types the PIN there. The handshake (PairRequest ->
 * PairChallenge -> PairConfirm, deriving the secret) runs in Rust; on success
 * the derived secret and server address are stored in app-private prefs.
 *
 * A plain programmatic layout — no res/layout — so the foundation carries no UI
 * resources it does not need.
 */
class PairActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(64, 64, 64, 64)
        }
        fun label(text: String) = TextView(this).apply {
            this.text = text
            textSize = 20f
        }

        val hostEntry = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
            setText(prefs.getString("server_host", "192.168.1.10"))
            hint = "server host (or host:port)"
        }
        val pinView = label("").apply { textSize = 40f }
        val status = label("Arm pairing in the web UI, then press Pair.")
        val pairButton = Button(this).apply { text = "Pair" }

        root.addView(label("Sunburst — pair this device"))
        root.addView(hostEntry, lp())
        root.addView(pairButton, lp())
        root.addView(pinView, lp())
        root.addView(status, lp())
        setContentView(root)

        pairButton.setOnClickListener {
            val (host, port) = parseHost(hostEntry.text.toString())
            val pin = nativeGenPin()
            if (pin.isEmpty()) {
                status.text = "could not generate a PIN"
                return@setOnClickListener
            }
            pinView.text = "PIN: $pin"
            status.text = "pairing… type the PIN into the web UI"
            pairButton.isEnabled = false
            thread {
                val secret = nativePair(host, port, pin)
                runOnUiThread {
                    pairButton.isEnabled = true
                    if (secret.isNotEmpty()) {
                        prefs.edit()
                            .putString("secret_hex", secret)
                            .putString("server_host", host)
                            .putInt("server_port", port)
                            .apply()
                        status.text = "paired"
                        finish()
                    } else {
                        status.text = "pairing failed — arm the web UI and try again"
                    }
                }
            }
        }
    }

    private fun parseHost(entry: String): Pair<String, Int> {
        val trimmed = entry.trim()
        val idx = trimmed.lastIndexOf(':')
        return if (idx > 0) {
            val port = trimmed.substring(idx + 1).toIntOrNull() ?: 47811
            Pair(trimmed.substring(0, idx), port)
        } else {
            Pair(trimmed, 47811)
        }
    }

    private fun lp() = LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.WRAP_CONTENT,
        ViewGroup.LayoutParams.WRAP_CONTENT,
    ).apply { topMargin = 32 }

    private external fun nativeGenPin(): String
    private external fun nativePair(host: String, port: Int, pin: String): String

    companion object {
        init {
            System.loadLibrary("sunburst_android")
        }
    }
}
