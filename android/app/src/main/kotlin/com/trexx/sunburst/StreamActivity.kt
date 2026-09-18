// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.media.MediaCodecList
import android.os.Bundle
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager

/**
 * The streaming activity: a full-screen [SurfaceView] whose surface is handed to
 * the Rust client, which owns the network, decode and present. Kotlin keeps only
 * the Activity + surface lifecycle and the platform queries with no NDK
 * equivalent (here, the codec-support probe); input and HDR arrive in later
 * commits.
 *
 * `SurfaceView`, never `TextureView` — TextureView costs a full frame of
 * compositing (CLAUDE.md).
 */
class StreamActivity : Activity(), SurfaceHolder.Callback {
    private var handle: Long = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        val view = SurfaceView(this)
        view.holder.addCallback(this)
        setContentView(view)
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        if (handle == 0L) {
            val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)
            val host = prefs.getString("server_host", "192.168.1.10")!!
            val port = prefs.getInt("server_port", 47811)
            // The pairing secret is provisioned by the pairing screen (a later
            // commit); until then this is empty and the client declines to start.
            val secret = prefs.getString("secret_hex", "")!!
            handle = nativeStart(holder.surface, host, port, secret, supportedCodecs())
        }
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        if (handle != 0L) nativeSurfaceChanged(handle, holder.surface)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        if (handle != 0L) {
            nativeStop(handle)
            handle = 0
        }
    }

    /** The codecs this device can decode, as the protocol's `Hello.codecs` bits:
     *  bit0 HEVC Main10, bit1 AV1 Main10. The server negotiates one of them. */
    private fun supportedCodecs(): Int {
        var bits = 0
        for (info in MediaCodecList(MediaCodecList.ALL_CODECS).codecInfos) {
            if (info.isEncoder) continue
            for (type in info.supportedTypes) {
                when (type.lowercase()) {
                    "video/hevc" -> bits = bits or 0x1
                    "video/av01" -> bits = bits or 0x2
                }
            }
        }
        return bits
    }

    private external fun nativeStart(
        surface: Surface,
        host: String,
        port: Int,
        secretHex: String,
        codecs: Int,
    ): Long

    private external fun nativeStop(handle: Long)
    private external fun nativeSurfaceChanged(handle: Long, surface: Surface)

    companion object {
        init {
            System.loadLibrary("sunburst_android")
        }
    }
}
