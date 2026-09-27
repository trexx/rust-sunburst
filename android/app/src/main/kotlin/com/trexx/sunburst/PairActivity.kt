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
 * PairChallenge -> PairConfirm, deriving the secret) runs in Rust, and so does
 * the wait for the server's PairResult: the PIN stays on screen until the
 * server has accepted it, a wrong PIN is reported as it is typed, and only an
 * accepted secret is stored in app-private prefs.
 *
 * Finishes with RESULT_OK once paired; anything else (Back) is RESULT_CANCELED,
 * which the launcher reads as "leave". Opened from Settings to re-pair, the old
 * pairing stays until a new one succeeds.
 *
 * A plain programmatic layout — no res/layout — so the foundation carries no UI
 * resources it does not need.
 */
class PairActivity : Activity() {
    private lateinit var status: TextView
    /** A `nativePair` is running on a worker thread. UI thread only. */
    private var waiting = false

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
        status = label("Arm pairing in the web UI, then press Pair.")
        val pairButton = Button(this).apply { text = "Pair" }

        root.addView(label("Sunburst — pair this device"))
        root.addView(hostEntry, lp())
        root.addView(pairButton, lp())
        root.addView(pinView, lp())
        root.addView(status, lp())
        setContentView(root)

        pairButton.setOnClickListener {
            val (host, port) = Pairing.parseHost(hostEntry.text.toString())
            val pin = nativeGenPin()
            if (pin.isEmpty()) {
                status.text = "could not generate a PIN"
                return@setOnClickListener
            }
            pinView.text = "PIN: $pin"
            status.text = "Type this PIN into the web UI (Clients → pending)."
            pairButton.isEnabled = false
            waiting = true
            thread {
                val result = nativePair(host, port, pin)
                runOnUiThread {
                    waiting = false
                    pairButton.isEnabled = true
                    if (Pairing.isSecret(result)) {
                        prefs.edit()
                            .putString("secret_hex", result)
                            .putString("server_host", host)
                            .putInt("server_port", port)
                            .apply()
                        status.text = "paired"
                        setResult(RESULT_OK)
                        finish()
                    } else {
                        pinView.text = ""
                        status.text = "Pairing failed: $result. Arm the web UI and press Pair again."
                    }
                }
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        // Leaving mid-wait (Back) ends the wait rather than leaving a worker
        // listening for a PIN nobody can see any more.
        if (waiting) nativeCancelPair()
    }

    /** Called by `nativePair`, on its worker thread, for each wrong PIN typed. */
    @Suppress("unused")
    fun onWrongPin(remaining: Int) {
        runOnUiThread {
            status.text = "Wrong PIN typed — $remaining attempt(s) left. Check the digits against this screen."
        }
    }

    private fun lp() = LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.WRAP_CONTENT,
        ViewGroup.LayoutParams.WRAP_CONTENT,
    ).apply { topMargin = 32 }

    private external fun nativeGenPin(): String
    private external fun nativePair(host: String, port: Int, pin: String): String
    private external fun nativeCancelPair()

    companion object {
        init {
            System.loadLibrary("sunburst_android")
        }
    }
}
