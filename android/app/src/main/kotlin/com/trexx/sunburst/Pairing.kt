// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

/** The pair screen's decisions, kept apart from the activity so they run on the JVM. */
object Pairing {
    const val DEFAULT_PORT = 47811

    private val SECRET = Regex("^[0-9a-f]{64}$")

    /**
     * Whether `nativePair`'s answer is the secret (64 lowercase hex characters)
     * rather than the message saying why pairing failed. A message is never
     * that shape, so the one string carries both.
     */
    fun isSecret(result: String): Boolean = SECRET.matches(result)

    /** `host` or `host:port`, the port defaulting when absent or unreadable. */
    fun parseHost(entry: String): Pair<String, Int> {
        val trimmed = entry.trim()
        val idx = trimmed.lastIndexOf(':')
        return if (idx > 0) {
            val port = trimmed.substring(idx + 1).toIntOrNull() ?: DEFAULT_PORT
            Pair(trimmed.substring(0, idx), port)
        } else {
            Pair(trimmed, DEFAULT_PORT)
        }
    }
}
