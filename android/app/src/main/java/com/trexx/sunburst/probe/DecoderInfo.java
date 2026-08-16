// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst.probe;

/**
 * What Phase 0.2 needs to know about one decoder.
 *
 * <p>Plain data, deliberately: everything that touches {@code MediaCodecList} needs a device, so
 * the parts that can be tested on a JVM are kept separate from the parts that cannot. That is the
 * same split moonlight-trexx uses for {@code VideoStats} and {@code StickCalibration}.
 */
public final class DecoderInfo {
    public String name = "";
    public String mimeType = "";
    public boolean hardwareAccelerated;
    public boolean softwareOnly;
    public boolean vendor;

    /** Whether the decoder advertises {@code FEATURE_LowLatency}. */
    public boolean featureLowLatency;

    /**
     * What happened when {@code KEY_LOW_LATENCY} was actually set at configure time.
     *
     * <p>Three-state, and deliberately not a boolean. Configure cannot answer this question in
     * either direction: MediaCodec silently ignores keys it does not recognise, so success proves
     * nothing, and a key that fails to echo back through {@code getInputFormat()} may still have
     * taken effect. A boolean here would be a confident answer to a question that was never asked
     * properly.
     *
     * <ul>
     *   <li>{@link #KEY_ECHOED} — set, and read back. The strongest positive available.
     *   <li>{@link #KEY_SILENT} — accepted without complaint but not echoed. Unknown.
     *   <li>{@link #KEY_REJECTED} — configure threw. The only unambiguous negative.
     * </ul>
     *
     * <p>{@link #featureLowLatency} is the decoder's own advertisement and remains the thing to
     * trust; this records what setting the key actually did. Errata #15 exists because devices
     * disagree between the two.
     */
    public String keyLowLatency = KEY_SILENT;

    public static final String KEY_ECHOED = "echoed";
    public static final String KEY_SILENT = "silent";
    public static final String KEY_REJECTED = "rejected";

    /** Why the key was refused, when it was. */
    public String keyLowLatencyError = "";

    public int maxWidth;
    public int maxHeight;
    public int maxInstances;

    /** Profile/level pairs, as reported. */
    public String profileLevels = "";

    /** Whether a 4K60 stream is within the reported capabilities. */
    public boolean supports4k60;

    /**
     * Vendor low-latency keys the decoder accepted.
     *
     * <p>Errata #16 and #17: some Amlogic decoders produce no output at all without an
     * undocumented {@code MediaFormat} option, which reads as a broken stream rather than a missing
     * flag.
     */
    public String vendorLowLatencyKeys = "";
}
