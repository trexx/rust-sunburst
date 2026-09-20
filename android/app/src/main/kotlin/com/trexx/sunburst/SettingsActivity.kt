// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView

/**
 * The TV settings screen: codec, bitrate ceiling, jitter depth, cursor overlay
 * and the performance hint, stored in the shared "sunburst" prefs that
 * [StreamActivity] reads. Codec and bitrate are only *requests* — the server
 * negotiates the codec against what this device can decode and clamps the
 * bitrate to its own ceiling, so nothing set here can produce an undecodable
 * stream.
 *
 * Every control is a single dpad-focusable button that cycles its value on the
 * centre key, which is the only interaction a TV remote does comfortably; there
 * are no text fields to type into. A plain programmatic layout, like
 * [PairActivity], so the foundation carries no UI resources it does not need.
 */
class SettingsActivity : Activity() {
    // Codec request: prefs int -1 = auto, else a StreamCodec discriminant.
    private val codecValues = intArrayOf(-1, 0, 1, 2)
    private val codecLabels = arrayOf("Auto", "HEVC", "AV1", "H.264")
    // Bitrate ceiling in kbps; 0 = no client cap (the server's ceiling applies).
    private val bitrateValues = intArrayOf(0, 50_000, 80_000, 100_000, 120_000, 150_000)
    // Jitter-buffer floor in ms.
    private val jitterValues = intArrayOf(0, 2, 4, 8, 16)
    // Pad-headset audio route: index == the native `audio_route` value.
    private val routeLabels = arrayOf("TV only", "Pad only", "TV + pad")
    // Pad-headset volume, percent.
    private val padVolumeValues = intArrayOf(0, 25, 50, 75, 100)

    private var codecIdx = 0
    private var bitrateIdx = 0
    private var jitterIdx = 1 // 2 ms default
    private var showCursor = true
    private var perfHint = true
    private var audioRouteIdx = 2 // TV + pad
    private var padVolumeIdx = 4 // 100%

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)
        codecIdx = indexOf(codecValues, prefs.getInt("prefer_codec", -1), 0)
        bitrateIdx = indexOf(bitrateValues, prefs.getInt("max_bitrate_kbps", 0), 0)
        jitterIdx = indexOf(jitterValues, prefs.getInt("jitter_min_ms", 2), 1)
        showCursor = prefs.getBoolean("show_cursor", true)
        perfHint = prefs.getBoolean("perf_hint", true)
        audioRouteIdx = prefs.getInt("audio_route", 2).coerceIn(0, 2)
        padVolumeIdx = indexOf(padVolumeValues, prefs.getInt("pad_volume", 100), 4)

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER_HORIZONTAL
            setPadding(64, 48, 64, 48)
        }
        root.addView(TextView(this).apply {
            text = "Sunburst — settings"
            textSize = 24f
        }, lp())

        val codecButton = Button(this)
        val bitrateButton = Button(this)
        val jitterButton = Button(this)
        val cursorButton = Button(this)
        val hintButton = Button(this)
        val routeButton = Button(this)
        val padVolumeButton = Button(this)

        fun refresh() {
            codecButton.text = "Codec: ${codecLabels[codecIdx]}"
            bitrateButton.text = "Max bitrate: ${bitrateLabel(bitrateValues[bitrateIdx])}"
            jitterButton.text = "Jitter buffer: ${jitterValues[jitterIdx]} ms"
            cursorButton.text = "Cursor overlay: ${onOff(showCursor)}"
            hintButton.text = "Performance hint: ${onOff(perfHint)}"
            routeButton.text = "Headset audio: ${routeLabels[audioRouteIdx]}"
            padVolumeButton.text = "Headset volume: ${padVolumeValues[padVolumeIdx]}%"
        }

        codecButton.setOnClickListener {
            codecIdx = (codecIdx + 1) % codecValues.size
            refresh()
        }
        bitrateButton.setOnClickListener {
            bitrateIdx = (bitrateIdx + 1) % bitrateValues.size
            refresh()
        }
        jitterButton.setOnClickListener {
            jitterIdx = (jitterIdx + 1) % jitterValues.size
            refresh()
        }
        cursorButton.setOnClickListener {
            showCursor = !showCursor
            refresh()
        }
        hintButton.setOnClickListener {
            perfHint = !perfHint
            refresh()
        }
        routeButton.setOnClickListener {
            audioRouteIdx = (audioRouteIdx + 1) % routeLabels.size
            refresh()
        }
        padVolumeButton.setOnClickListener {
            padVolumeIdx = (padVolumeIdx + 1) % padVolumeValues.size
            refresh()
        }

        for (b in listOf(
            codecButton, bitrateButton, jitterButton, cursorButton, hintButton,
            routeButton, padVolumeButton,
        )) {
            root.addView(b, lp())
        }
        root.addView(TextView(this).apply {
            text = "Changes apply when streaming resumes."
            textSize = 14f
        }, lp())

        // Put the Xbox Wireless Adapter into pairing mode, the same state Windows
        // sets from "Add a device" — for units whose physical pairing button is
        // dead. No-op without an adapter attached.
        val pairStatus = TextView(this).apply { textSize = 14f }
        root.addView(Button(this).apply {
            text = "Pair a controller"
            setOnClickListener {
                pairStatus.text = if (UsbBridge.setPairing(true)) {
                    "Pairing… hold the controller's pair button."
                } else {
                    "No wireless adapter attached."
                }
            }
        }, lp())
        root.addView(pairStatus, lp())

        root.addView(Button(this).apply {
            text = "Save and close"
            setOnClickListener {
                prefs.edit()
                    .putInt("prefer_codec", codecValues[codecIdx])
                    .putInt("max_bitrate_kbps", bitrateValues[bitrateIdx])
                    .putInt("jitter_min_ms", jitterValues[jitterIdx])
                    .putBoolean("show_cursor", showCursor)
                    .putBoolean("perf_hint", perfHint)
                    .putInt("audio_route", audioRouteIdx)
                    .putInt("pad_volume", padVolumeValues[padVolumeIdx])
                    .apply()
                finish()
            }
        }, lp())

        refresh()
        setContentView(root)
        codecButton.requestFocus()
    }

    private fun bitrateLabel(kbps: Int): String =
        if (kbps == 0) "Server default" else "${kbps / 1000} Mbps"

    private fun onOff(v: Boolean): String = if (v) "On" else "Off"

    private fun indexOf(values: IntArray, value: Int, fallback: Int): Int {
        val i = values.indexOf(value)
        return if (i >= 0) i else fallback
    }

    private fun lp() = LinearLayout.LayoutParams(
        ViewGroup.LayoutParams.WRAP_CONTENT,
        ViewGroup.LayoutParams.WRAP_CONTENT,
    ).apply { topMargin = 28 }
}
