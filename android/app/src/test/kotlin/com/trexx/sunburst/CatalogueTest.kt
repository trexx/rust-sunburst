// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class CatalogueTest {
    @Test
    fun theDesktopTileComesFirstAndEmptyPathsMeanNoArt() {
        val tiles = Catalogue.parse(arrayOf("4", "Cyberpunk 2077", "/c/ab.webp", "0", "Big Picture", ""))
        assertEquals(
            listOf(
                AppTile.DESKTOP,
                AppTile(4, "Cyberpunk 2077", "/c/ab.webp"),
                AppTile(0, "Big Picture", null),
            ),
            tiles,
        )
        assertTrue(tiles[0].isDesktop)
        assertFalse(tiles[1].isDesktop)
    }

    @Test
    fun aFailedOrMalformedAnswerStillHasTheDesktop() {
        assertEquals(listOf(AppTile.DESKTOP), Catalogue.parse(null))
        // A trailing partial entry and a non-numeric id are skipped.
        val tiles = Catalogue.parse(arrayOf("x", "Bad", "", "7", "Good", "", "9", "Cut"))
        assertEquals(listOf(AppTile.DESKTOP, AppTile(7, "Good", null)), tiles)
    }

    @Test
    fun alreadyRunningStillStreams() {
        assertTrue(Catalogue.streamAnyway("an app is already running: 2"))
        assertFalse(Catalogue.streamAnyway("no such app: 9"))
    }

    @Test
    fun sampleSizeNeverDecodesBelowTheTile() {
        // Halving 600x900 lands exactly on the tile, which is still enough.
        assertEquals(2, Catalogue.sampleSize(600, 900, 300, 450))
        assertEquals(1, Catalogue.sampleSize(599, 899, 300, 450))
        assertEquals(8, Catalogue.sampleSize(2400, 3600, 300, 450))
        assertEquals(1, Catalogue.sampleSize(100, 100, 300, 450))
    }
}
