// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.os.Bundle
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager

/**
 * The streaming activity: a full-screen [SurfaceView] whose surface is handed to
 * the Rust client, which owns the network, decode and present. Kotlin keeps only
 * the Activity + surface lifecycle here; input and platform queries arrive in
 * later commits.
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
            handle = nativeStart(holder.surface, serverHost(), serverPort())
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

    // The server address is provisioned by the pairing/settings screen in a later
    // commit; hard-coded here so the foundation is runnable on the bench.
    private fun serverHost(): String = "192.168.1.10"
    private fun serverPort(): Int = 47811

    private external fun nativeStart(surface: Surface, host: String, port: Int): Long
    private external fun nativeStop(handle: Long)
    private external fun nativeSurfaceChanged(handle: Long, surface: Surface)

    companion object {
        init {
            System.loadLibrary("sunburst_android")
        }
    }
}
