// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst.probe;

import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.os.Build;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/**
 * Phase 0.2 — enumerate every HEVC and AV1 decoder on this box.
 *
 * <p>ROADMAP.md wants the quirks table seeded with real data instead of assumptions. This is what
 * produces the data.
 *
 * <p><b>Every decoder is examined, not the first match.</b> Errata #15 in decoder-errata.txt: some
 * devices do not support {@code FEATURE_LowLatency} on their first compatible decoder, and taking
 * the first one silently loses it.
 */
public final class DecoderProbe {

    /** The two codecs that matter here. H.264 is not a target on either box. */
    private static final String[] MIME_TYPES = {
        MediaFormat.MIMETYPE_VIDEO_HEVC, MediaFormat.MIMETYPE_VIDEO_AV1,
    };

    /**
     * Undocumented vendor keys that enable low-latency decoding before, or instead of,
     * {@code KEY_LOW_LATENCY}.
     *
     * <p>Errata #17 is the reason this list exists: the Fire TV 3's Amlogic HEVC decoder produces
     * no output frames at all without {@code vdec-lowlatency}, which presents as a broken stream
     * rather than as a missing flag. The Homatics is Amlogic too.
     */
    private static final String[] VENDOR_LOW_LATENCY_KEYS = {
        "vdec-lowlatency", // Amlogic
        "vendor.low-latency.enable", // Qualcomm
        "vendor.qti-ext-dec-picture-order.enable", // Qualcomm
        "vdec-lowlatency-enable", // MediaTek
        "vendor.rtc-ext-dec-low-latency.enable", // MediaTek
    };

    private DecoderProbe() {}

    public static ProbeReport.DeviceInfo deviceInfo() {
        ProbeReport.DeviceInfo d = new ProbeReport.DeviceInfo();
        d.manufacturer = Build.MANUFACTURER;
        d.model = Build.MODEL;
        d.device = Build.DEVICE;
        d.hardware = Build.HARDWARE;
        d.androidRelease = Build.VERSION.RELEASE;
        d.sdkInt = Build.VERSION.SDK_INT;
        d.abis = String.join(",", Build.SUPPORTED_ABIS);
        return d;
    }

    public static List<DecoderInfo> enumerate() {
        List<DecoderInfo> found = new ArrayList<>();
        MediaCodecInfo[] all = new MediaCodecList(MediaCodecList.ALL_CODECS).getCodecInfos();

        for (MediaCodecInfo codec : all) {
            if (codec.isEncoder()) {
                continue;
            }
            for (String mime : MIME_TYPES) {
                if (!supports(codec, mime)) {
                    continue;
                }
                found.add(describe(codec, mime));
            }
        }
        return found;
    }

    private static boolean supports(MediaCodecInfo codec, String mime) {
        for (String type : codec.getSupportedTypes()) {
            if (type.equalsIgnoreCase(mime)) {
                return true;
            }
        }
        return false;
    }

    private static DecoderInfo describe(MediaCodecInfo codec, String mime) {
        DecoderInfo info = new DecoderInfo();
        info.name = codec.getName();
        info.mimeType = mime;
        info.hardwareAccelerated = codec.isHardwareAccelerated();
        info.softwareOnly = codec.isSoftwareOnly();
        info.vendor = codec.isVendor();

        MediaCodecInfo.CodecCapabilities caps = codec.getCapabilitiesForType(mime);
        info.featureLowLatency =
                caps.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency);
        info.maxInstances = caps.getMaxSupportedInstances();

        MediaCodecInfo.VideoCapabilities video = caps.getVideoCapabilities();
        if (video != null) {
            info.maxWidth = video.getSupportedWidths().getUpper();
            info.maxHeight = video.getSupportedHeights().getUpper();
            // The workload, stated as a question rather than inferred from the maxima: a decoder
            // can report 4096 wide and still refuse 4K at 60.
            try {
                info.supports4k60 = video.areSizeAndRateSupported(3840, 2160, 60.0);
            } catch (IllegalArgumentException e) {
                info.supports4k60 = false;
            }
        }

        StringBuilder levels = new StringBuilder();
        for (MediaCodecInfo.CodecProfileLevel pl : caps.profileLevels) {
            if (levels.length() > 0) {
                levels.append(' ');
            }
            levels.append(pl.profile).append(':').append(pl.level);
        }
        info.profileLevels = levels.toString();

        probeLowLatency(info);
        return info;
    }

    /**
     * Configure the decoder for real and see what it accepts.
     *
     * <p>The capability query above reports what the decoder <em>advertises</em>. This reports what
     * it <em>does</em>, which is the question the quirks table needs answered: ROADMAP 0.2 asks
     * "whether {@code KEY_LOW_LATENCY} is accepted", and only a configure call can say.
     *
     * <p>Configured against a null surface in ByteBuffer mode, so nothing needs to be on screen.
     */
    private static void probeLowLatency(DecoderInfo info) {
        MediaCodec codec = null;
        try {
            codec = MediaCodec.createByCodecName(info.name);

            MediaFormat format = MediaFormat.createVideoFormat(info.mimeType, 3840, 2160);
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
            codec.configure(format, null, null, 0);

            // Configure succeeding proves nothing on its own: unknown keys are silently ignored.
            // Reading the key back is the strongest positive available, and its absence is not a
            // negative — hence three states rather than a boolean.
            MediaFormat applied = codec.getInputFormat();
            boolean echoed =
                    applied.containsKey(MediaFormat.KEY_LOW_LATENCY)
                            && applied.getInteger(MediaFormat.KEY_LOW_LATENCY) == 1;
            info.keyLowLatency = echoed ? DecoderInfo.KEY_ECHOED : DecoderInfo.KEY_SILENT;
        } catch (IOException | IllegalArgumentException | IllegalStateException e) {
            info.keyLowLatency = DecoderInfo.KEY_REJECTED;
            info.keyLowLatencyError = e.getClass().getSimpleName() + ": " + e.getMessage();
        } finally {
            if (codec != null) {
                try {
                    codec.release();
                } catch (IllegalStateException ignored) {
                    // Releasing a codec that never configured is not interesting.
                }
            }
        }

        info.vendorLowLatencyKeys = probeVendorKeys(info);
    }

    /**
     * Which undocumented vendor keys visibly survive configure.
     *
     * <p><b>A successful configure proves nothing here.</b> MediaCodec silently ignores keys it
     * does not recognise, so the first version of this reported all five keys as accepted by every
     * decoder on the Shield — including Google's software one, which has never heard of any of
     * them. That is the metric that is worse than no metric.
     *
     * <p>The only signal available without a real stream is whether the key survives into
     * {@code getInputFormat()}. Expect that to be empty on most decoders: a vendor key taking
     * effect while staying invisible here is entirely possible, so an empty result means "no
     * evidence", not "unsupported". Errata #17 can only be settled by decoding an actual stream
     * and seeing whether frames come out.
     */
    private static String probeVendorKeys(DecoderInfo info) {
        StringBuilder survived = new StringBuilder();
        for (String key : VENDOR_LOW_LATENCY_KEYS) {
            MediaCodec codec = null;
            try {
                codec = MediaCodec.createByCodecName(info.name);
                MediaFormat format = MediaFormat.createVideoFormat(info.mimeType, 3840, 2160);
                format.setInteger(key, 1);
                codec.configure(format, null, null, 0);

                MediaFormat applied = codec.getInputFormat();
                if (applied.containsKey(key) && applied.getInteger(key) == 1) {
                    if (survived.length() > 0) {
                        survived.append(' ');
                    }
                    survived.append(key);
                }
            } catch (IOException | IllegalArgumentException | IllegalStateException e) {
                // A decoder that refuses the key outright is the clearest negative there is.
            } finally {
                if (codec != null) {
                    try {
                        codec.release();
                    } catch (IllegalStateException ignored) {
                        // As above.
                    }
                }
            }
        }
        return survived.toString();
    }
}
