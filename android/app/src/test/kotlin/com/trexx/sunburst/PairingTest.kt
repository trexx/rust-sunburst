// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingTest {
    @Test
    fun onlySixtyFourHexCharactersAreASecret() {
        assertTrue(Pairing.isSecret("0123456789abcdef".repeat(4)))
        assertFalse(Pairing.isSecret("0123456789abcdef".repeat(4).dropLast(1)))
        assertFalse(Pairing.isSecret("0123456789ABCDEF".repeat(4)))
        assertFalse(Pairing.isSecret(""))
        assertFalse(Pairing.isSecret("the PIN was not entered in time"))
    }

    @Test
    fun aHostTakesTheDefaultPortUnlessOneIsGiven() {
        assertEquals(Pair("192.168.1.10", 47811), Pairing.parseHost(" 192.168.1.10 "))
        assertEquals(Pair("192.168.1.10", 5000), Pairing.parseHost("192.168.1.10:5000"))
        assertEquals(Pair("server", 47811), Pairing.parseHost("server:nope"))
    }
}
