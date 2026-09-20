// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.os.Bundle

/**
 * Does nothing, and exists only so Android offers the "use by default for this
 * USB device" checkbox.
 *
 * Android grants USB permission per-connection unless the user ticks that box,
 * and it only offers the box when the app declares a `USB_DEVICE_ATTACHED`
 * intent-filter matching the device. The filter has to sit on an activity, and
 * Android *launches* that activity on attach — so this one must not be a screen:
 * it finishes immediately, and its manifest entry gives it `Theme.NoDisplay` and
 * its own task affinity so a pad re-plugged mid-stream cannot pull the user out
 * of the game. Nothing is claimed here; [UsbBridge], active while streaming,
 * learns about the device through its own receiver.
 */
class UsbAttachTrampoline : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        finish()
    }
}
