// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

/** One tile on the app grid. `artPath` is a cached image file, or null. */
data class AppTile(val id: Int, val name: String, val artPath: String?) {
    /** The first tile: stream the desktop without launching anything. */
    val isDesktop: Boolean get() = id == DESKTOP_ID

    companion object {
        const val DESKTOP_ID = -1
        val DESKTOP = AppTile(DESKTOP_ID, "Desktop", null)
    }
}

/** The grid's data, kept apart from the activity so it runs on the JVM. */
object Catalogue {
    /**
     * `nativeCatalogue`'s flat answer — `id, name, artPath` per app, an empty
     * path for none — as tiles, with the Desktop tile first. A malformed answer
     * (a length that is not a multiple of three, a non-numeric id) yields what
     * could be read rather than nothing.
     */
    fun parse(flat: Array<String>?): List<AppTile> {
        val tiles = mutableListOf(AppTile.DESKTOP)
        if (flat == null) return tiles
        var i = 0
        while (i + 2 < flat.size) {
            val id = flat[i].toIntOrNull()
            if (id != null && id >= 0) {
                tiles += AppTile(id, flat[i + 1], flat[i + 2].ifEmpty { null })
            }
            i += 3
        }
        return tiles
    }

    /** Whether a failed launch should still stream: the app, or another, is
     *  already running, so there is a game to stream to. */
    fun streamAnyway(launchError: String): Boolean =
        launchError.contains("already running", ignoreCase = true)

    /** The largest power-of-two `inSampleSize` that keeps a `w`×`h` image at
     *  least `targetW`×`targetH`, so a tile never decodes a full-size bitmap. */
    fun sampleSize(w: Int, h: Int, targetW: Int, targetH: Int): Int {
        var sample = 1
        while (w / (sample * 2) >= targetW && h / (sample * 2) >= targetH) sample *= 2
        return sample
    }
}
