// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.os.Build
import android.util.Log
import java.io.File

/**
 * Claims an Xbox pad or Wireless Adapter and hands its file descriptor to the
 * native GIP driver (`sunburst-gip-bridge`, via the `nativeUsb*` calls).
 *
 * Kotlin owns only the USB permission dance and the descriptor's lifetime; the
 * driver — radio bring-up, GIP, the security handshake, audio — is native. This
 * is the sunburst analogue of xow's `UsbDriverService`, but a plain singleton the
 * [StreamActivity] runs while it is alive rather than a bound service.
 *
 * One device at a time for now: a single adapter serves up to four pads, and a
 * lone wired pad is the other case.
 */
object UsbBridge {
    private const val TAG = "sunburst.usb"
    private const val ACTION_PERMISSION = "com.trexx.sunburst.USB_PERMISSION"
    private const val MS_VENDOR = 0x045e
    private val ADAPTER_PIDS = intArrayOf(0x02e6, 0x02fe, 0x091e)
    private const val FIRMWARE_ASSET = "xone_dongle_fw.bin"

    private var receiver: BroadcastReceiver? = null

    // Held so the descriptor stays valid while the native driver uses it: closing
    // the connection closes the fd.
    private var connection: UsbDeviceConnection? = null

    /** Begin listening for and claiming pads. Idempotent. */
    @Synchronized
    fun start(context: Context) {
        if (receiver != null) return
        val app = context.applicationContext
        val usb = app.getSystemService(Context.USB_SERVICE) as? UsbManager ?: return

        val rx = object : BroadcastReceiver() {
            override fun onReceive(c: Context, intent: Intent) {
                when (intent.action) {
                    UsbManager.ACTION_USB_DEVICE_ATTACHED ->
                        deviceOf(intent)?.let { maybeClaim(app, usb, it) }
                    UsbManager.ACTION_USB_DEVICE_DETACHED ->
                        deviceOf(intent)?.let { detach() }
                    ACTION_PERMISSION -> {
                        val dev = deviceOf(intent) ?: return
                        if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                            open(app, usb, dev)
                        } else {
                            Log.i(TAG, "permission denied for ${hex(dev)}")
                        }
                    }
                }
            }
        }
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
            addAction(ACTION_PERMISSION)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            app.registerReceiver(rx, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            app.registerReceiver(rx, filter)
        }
        receiver = rx

        // Claim anything already plugged in.
        for (dev in usb.deviceList.values) maybeClaim(app, usb, dev)
    }

    /** Stop listening and release any claimed device. */
    @Synchronized
    fun stop(context: Context) {
        val rx = receiver ?: return
        context.applicationContext.unregisterReceiver(rx)
        receiver = null
        detach()
    }

    /** Put the adapter into (or out of) pairing mode. */
    fun setPairing(on: Boolean): Boolean = nativeSetPairing(on)

    private fun isAdapter(dev: UsbDevice) =
        dev.vendorId == MS_VENDOR && ADAPTER_PIDS.contains(dev.productId)

    private fun isWiredGip(dev: UsbDevice): Boolean {
        if (dev.vendorId != MS_VENDOR) return false
        for (i in 0 until dev.interfaceCount) {
            val itf = dev.getInterface(i)
            if (itf.interfaceClass == 0xff && itf.interfaceSubclass == 0x47 &&
                itf.interfaceProtocol == 0xd0
            ) {
                return true
            }
        }
        return false
    }

    @Synchronized
    private fun maybeClaim(ctx: Context, usb: UsbManager, dev: UsbDevice) {
        if (!(isAdapter(dev) || isWiredGip(dev))) return
        if (connection != null) return
        if (usb.hasPermission(dev)) {
            open(ctx, usb, dev)
        } else {
            val flags = if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
            val intent = Intent(ACTION_PERMISSION).setPackage(ctx.packageName)
            usb.requestPermission(dev, PendingIntent.getBroadcast(ctx, 0, intent, flags))
        }
    }

    @Synchronized
    private fun open(ctx: Context, usb: UsbManager, dev: UsbDevice) {
        if (connection != null) return
        val conn = usb.openDevice(dev)
        if (conn == null) {
            Log.e(TAG, "openDevice failed for ${hex(dev)}")
            return
        }
        connection = conn
        val firmware = if (isAdapter(dev)) firmwarePath(ctx) else ""
        val ok = nativeUsbAttach(conn.fileDescriptor, dev.vendorId, dev.productId, firmware)
        if (!ok) {
            Log.e(TAG, "native attach failed for ${hex(dev)}")
            conn.close()
            connection = null
        } else {
            Log.i(TAG, "attached ${hex(dev)}")
        }
    }

    @Synchronized
    private fun detach() {
        if (connection == null) return
        nativeUsbDetach()
        connection?.close()
        connection = null
    }

    /**
     * Stage the bundled firmware asset into files dir and return its path, or ""
     * if it is not bundled (fetch-firmware.sh was not run) — wired pads never need
     * it, and the native side logs the open failure for the adapter.
     */
    private fun firmwarePath(ctx: Context): String {
        val out = File(ctx.filesDir, FIRMWARE_ASSET)
        if (!out.exists()) {
            try {
                ctx.assets.open(FIRMWARE_ASSET).use { input ->
                    out.outputStream().use { input.copyTo(it) }
                }
            } catch (e: Exception) {
                Log.w(TAG, "firmware asset '$FIRMWARE_ASSET' missing: ${e.message}")
                return ""
            }
        }
        return out.absolutePath
    }

    @Suppress("DEPRECATION")
    private fun deviceOf(intent: Intent): UsbDevice? =
        if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
        } else {
            intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
        }

    private fun hex(dev: UsbDevice) = String.format("%04x:%04x", dev.vendorId, dev.productId)

    private external fun nativeUsbAttach(fd: Int, vid: Int, pid: Int, firmwarePath: String): Boolean
    private external fun nativeUsbDetach()
    private external fun nativeSetPairing(on: Boolean): Boolean

    init {
        System.loadLibrary("sunburst_android")
    }
}
