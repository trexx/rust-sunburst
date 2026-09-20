// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.content.Intent
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.media.MediaCodecList
import android.os.Build
import android.os.Bundle
import android.os.PerformanceHintManager
import android.os.PerformanceHintManager.Session
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.widget.FrameLayout
import android.view.WindowManager

/**
 * The streaming activity: a full-screen [SurfaceView] handed to the Rust client,
 * which owns network, decode and present. Kotlin keeps the surface lifecycle and
 * forwards the input the platform only exposes through View callbacks — captured
 * relative mouse, keyboard (before the IME), and gamepad — to Rust, which maps
 * and sends it.
 */
class StreamActivity : Activity(), SurfaceHolder.Callback {
    private var handle: Long = 0
    private var surface: Surface? = null
    private lateinit var view: SurfaceView
    private var hintSession: Session? = null
    private lateinit var cursorView: CursorView
    private var audioFocus: AudioFocusRequest? = null
    private var perfHintEnabled = true

    /** The subset of prefs the native client is started with; changing any of
     *  these means reconnecting, since the codec and bitrate are negotiated at
     *  connect. Held so [restartIfSettingsChanged] can tell when to rebuild. */
    private data class StreamParams(
        val host: String,
        val port: Int,
        val secret: String,
        val preferCodec: Int,
        val maxBitrateKbps: Int,
        val jitterMinMs: Int,
        val audioRoute: Int,
        val padVolume: Int,
    )
    private var startedWith: StreamParams? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        view = SurfaceView(this)
        view.holder.addCallback(this)
        view.isFocusable = true
        view.isFocusableInTouchMode = true
        // Relative mouse: captured-pointer deltas rather than absolute positions.
        view.setOnCapturedPointerListener { _, e -> onCapturedPointer(e) }
        cursorView = CursorView(this)
        val frame = FrameLayout(this)
        frame.addView(view)
        frame.addView(cursorView) // drawn above the video
        setContentView(frame)
        view.requestFocus()
        // Claim any Xbox pad / adapter and keep it for the activity's life, so
        // opening Settings (a pause) does not drop a paired controller.
        UsbBridge.start(this)
    }

    override fun onDestroy() {
        super.onDestroy()
        UsbBridge.stop(this)
    }

    override fun onResume() {
        super.onResume()
        requestAudioFocus()
        restartIfSettingsChanged()
        maybeStart()
    }

    /** Coming back from [SettingsActivity]: apply the presentation prefs that do
     *  not need a reconnect in place, and drop the client if a negotiated one
     *  (codec / bitrate / jitter) changed so [maybeStart] rebuilds it. */
    private fun restartIfSettingsChanged() {
        if (handle == 0L) return
        val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)
        cursorView.visibility =
            if (prefs.getBoolean("show_cursor", true)) View.VISIBLE else View.GONE
        perfHintEnabled = prefs.getBoolean("perf_hint", true)
        if (perfHintEnabled) {
            startPerformanceHint()
        } else {
            hintSession?.close()
            hintSession = null
        }
        val started = startedWith ?: return
        val reconnect = started.preferCodec != prefs.getInt("prefer_codec", -1) ||
            started.maxBitrateKbps != prefs.getInt("max_bitrate_kbps", 0) ||
            started.jitterMinMs != prefs.getInt("jitter_min_ms", 2) ||
            started.audioRoute != prefs.getInt("audio_route", 2) ||
            started.padVolume != prefs.getInt("pad_volume", 100)
        if (reconnect) {
            nativeStop(handle)
            handle = 0
            startedWith = null
        }
    }

    override fun onPause() {
        super.onPause()
        abandonAudioFocus()
    }

    /** Take audio focus for the stream (usage GAME), so other apps' audio ducks
     *  while we play. The Rust client owns the AAudio output itself. */
    private fun requestAudioFocus() {
        if (audioFocus != null) return
        val am = getSystemService(AudioManager::class.java) ?: return
        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_GAME)
            .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
            .build()
        val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(attrs)
            .build()
        am.requestAudioFocus(request)
        audioFocus = request
    }

    private fun abandonAudioFocus() {
        val request = audioFocus ?: return
        getSystemService(AudioManager::class.java)?.abandonAudioFocusRequest(request)
        audioFocus = null
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) view.requestPointerCapture()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surface = holder.surface
        maybeStart()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        if (handle == 0L) return
        nativeSurfaceChanged(handle, holder.surface)
        // Tell the compositor the content cadence (4K60 workload).
        holder.surface.setFrameRate(60f, Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        startPerformanceHint()
    }

    /** Ask the scheduler to favour the client thread for a ~16.6 ms frame budget.
     *  A real win on the Amlogic's small cores; API 31+. */
    private fun startPerformanceHint() {
        if (!perfHintEnabled || hintSession != null || Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val tid = nativeClientTid(handle)
        if (tid == 0) return
        val phm = getSystemService(PerformanceHintManager::class.java) ?: return
        hintSession = phm.createHintSession(intArrayOf(tid), 16_666_666L)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surface = null
        hintSession?.close()
        hintSession = null
        if (handle != 0L) {
            nativeStop(handle)
            handle = 0
        }
    }

    private fun maybeStart() {
        if (handle != 0L) return
        val s = surface ?: return
        val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)
        val secret = prefs.getString("secret_hex", "")!!
        if (secret.isEmpty()) {
            startActivity(Intent(this, PairActivity::class.java))
            return
        }
        val params = StreamParams(
            host = prefs.getString("server_host", "192.168.1.10")!!,
            port = prefs.getInt("server_port", 47811),
            secret = secret,
            preferCodec = prefs.getInt("prefer_codec", -1),
            maxBitrateKbps = prefs.getInt("max_bitrate_kbps", 0),
            jitterMinMs = prefs.getInt("jitter_min_ms", 2),
            audioRoute = prefs.getInt("audio_route", 2),
            padVolume = prefs.getInt("pad_volume", 100),
        )
        perfHintEnabled = prefs.getBoolean("perf_hint", true)
        cursorView.visibility =
            if (prefs.getBoolean("show_cursor", true)) View.VISIBLE else View.GONE
        handle = nativeStart(
            s, params.host, params.port, params.secret, supportedCodecs(),
            params.preferCodec, params.maxBitrateKbps, params.jitterMinMs,
            params.audioRoute, params.padVolume,
        )
        startedWith = params
    }

    // --- Input -------------------------------------------------------------

    private fun isGamepad(event: KeyEvent): Boolean =
        (event.source and InputDevice.SOURCE_GAMEPAD) == InputDevice.SOURCE_GAMEPAD ||
            (event.source and InputDevice.SOURCE_JOYSTICK) == InputDevice.SOURCE_JOYSTICK

    // The stream activity has no IME, so Activity-level key callbacks see every
    // key; repeats are dropped (the server holds key state).
    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean = key(keyCode, event, true) || super.onKeyDown(keyCode, event)
    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean = key(keyCode, event, false) || super.onKeyUp(keyCode, event)

    private fun key(keyCode: Int, event: KeyEvent, down: Boolean): Boolean {
        // Menu opens settings and is never forwarded to the server — it is not a
        // useful game key, and this is the one way in to the settings screen.
        if (keyCode == KeyEvent.KEYCODE_MENU) {
            if (down && event.repeatCount == 0) {
                startActivity(Intent(this, SettingsActivity::class.java))
            }
            return true
        }
        if (handle == 0L || event.repeatCount > 0) return false
        if (isGamepad(event)) {
            nativePadButton(handle, keyCode, down)
        } else {
            nativeKey(handle, keyCode, down, event.metaState)
        }
        return true
    }

    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (handle == 0L) return super.onGenericMotionEvent(event)
        val src = event.source
        if (src and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK) {
            nativePadAxis(
                handle,
                event.getAxisValue(MotionEvent.AXIS_X),
                event.getAxisValue(MotionEvent.AXIS_Y),
                event.getAxisValue(MotionEvent.AXIS_Z),
                event.getAxisValue(MotionEvent.AXIS_RZ),
                event.getAxisValue(MotionEvent.AXIS_LTRIGGER),
                event.getAxisValue(MotionEvent.AXIS_RTRIGGER),
                event.getAxisValue(MotionEvent.AXIS_HAT_X),
                event.getAxisValue(MotionEvent.AXIS_HAT_Y),
            )
            return true
        }
        if (src and InputDevice.SOURCE_CLASS_POINTER == InputDevice.SOURCE_CLASS_POINTER) {
            val v = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
            val h = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
            if (v != 0f) nativeWheel(handle, v, false)
            if (h != 0f) nativeWheel(handle, h, true)
            return true
        }
        return super.onGenericMotionEvent(event)
    }

    private fun onCapturedPointer(event: MotionEvent): Boolean {
        if (handle == 0L) return false
        when (event.actionMasked) {
            MotionEvent.ACTION_MOVE, MotionEvent.ACTION_HOVER_MOVE ->
                nativeMouseMove(handle, event.x, event.y) // captured: x/y are deltas
            MotionEvent.ACTION_BUTTON_PRESS ->
                nativeMouseButton(handle, event.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE ->
                nativeMouseButton(handle, event.actionButton, false)
            MotionEvent.ACTION_SCROLL -> {
                val v = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
                if (v != 0f) nativeWheel(handle, v, false)
            }
        }
        return true
    }

    /** Codecs this device can decode, as Hello.codecs bits (bit0 HEVC, bit1 AV1). */
    private fun supportedCodecs(): Int {
        var bits = 0
        for (info in MediaCodecList(MediaCodecList.ALL_CODECS).codecInfos) {
            if (info.isEncoder) continue
            for (type in info.supportedTypes) {
                when (type.lowercase()) {
                    "video/hevc" -> bits = bits or 0x1
                    "video/av01" -> bits = bits or 0x2
                    "video/avc" -> bits = bits or 0x4
                }
            }
        }
        return bits
    }

    // --- Cursor (called from the client thread; marshalled to the UI thread) ---

    /** A new cursor shape as BGRA bytes with its hotspot; `width == 0` hides it. */
    fun onCursorShape(bgra: ByteArray, width: Int, height: Int, hotspotX: Int, hotspotY: Int) {
        val bitmap = if (width == 0 || height == 0) {
            null
        } else {
            val px = IntArray(width * height)
            var i = 0
            while (i < px.size) {
                val b = bgra[i * 4].toInt() and 0xFF
                val g = bgra[i * 4 + 1].toInt() and 0xFF
                val r = bgra[i * 4 + 2].toInt() and 0xFF
                val a = bgra[i * 4 + 3].toInt() and 0xFF
                px[i] = (a shl 24) or (r shl 16) or (g shl 8) or b
                i++
            }
            Bitmap.createBitmap(px, width, height, Bitmap.Config.ARGB_8888)
        }
        runOnUiThread { cursorView.setShape(bitmap, hotspotX, hotspotY) }
    }

    /** A cursor position, normalised 0..65535 across the server's monitor. */
    fun onCursorPosition(x: Int, y: Int, visible: Boolean) {
        runOnUiThread { cursorView.setPosition(x, y, visible) }
    }

    /** Draws the server's cursor above the video, so the pointer is not encoded
     *  into the stream and its shape has no network round trip. */
    private class CursorView(context: Context) : View(context) {
        private var bitmap: Bitmap? = null
        private var hotspotX = 0
        private var hotspotY = 0
        private var normX = 0
        private var normY = 0
        private var visible = false

        fun setShape(bmp: Bitmap?, hx: Int, hy: Int) {
            bitmap = bmp
            hotspotX = hx
            hotspotY = hy
            invalidate()
        }

        fun setPosition(x: Int, y: Int, vis: Boolean) {
            normX = x
            normY = y
            visible = vis
            invalidate()
        }

        override fun onDraw(canvas: Canvas) {
            val bmp = bitmap ?: return
            if (!visible) return
            val px = normX.toLong() * width / 65535L - hotspotX
            val py = normY.toLong() * height / 65535L - hotspotY
            canvas.drawBitmap(bmp, px.toFloat(), py.toFloat(), null)
        }
    }

    private external fun nativeStart(
        surface: Surface,
        host: String,
        port: Int,
        secretHex: String,
        codecs: Int,
        preferCodec: Int,
        maxBitrateKbps: Int,
        jitterMinMs: Int,
        audioRoute: Int,
        padVolume: Int,
    ): Long
    private external fun nativeStop(handle: Long)
    private external fun nativeSurfaceChanged(handle: Long, surface: Surface)
    private external fun nativeClientTid(handle: Long): Int
    private external fun nativeKey(handle: Long, code: Int, down: Boolean, meta: Int)
    private external fun nativeMouseMove(handle: Long, dx: Float, dy: Float)
    private external fun nativeMouseButton(handle: Long, code: Int, down: Boolean)
    private external fun nativeWheel(handle: Long, delta: Float, horizontal: Boolean)
    private external fun nativePadButton(handle: Long, code: Int, down: Boolean)
    private external fun nativePadAxis(
        handle: Long, lx: Float, ly: Float, rx: Float, ry: Float, lt: Float, rt: Float, hatX: Float, hatY: Float,
    )

    companion object {
        init {
            System.loadLibrary("sunburst_android")
        }
    }
}
