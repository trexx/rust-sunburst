// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst.probe;

import java.util.List;

/**
 * Serialises a probe result to JSON.
 *
 * <p>Hand-rolled rather than pulled from a library: the shape is fixed and tiny, and this is the
 * one part of the probe that runs on a JVM, so it is the one part that can carry tests.
 */
public final class ProbeReport {

    private ProbeReport() {}

    /** Render the report. {@code device} identifies the box the numbers came from. */
    public static String toJson(DeviceInfo device, List<DecoderInfo> decoders) {
        StringBuilder sb = new StringBuilder(4096);
        sb.append("{\n");
        sb.append("  \"schema\": 1,\n");
        sb.append("  \"device\": {\n");
        field(sb, 4, "manufacturer", device.manufacturer, true);
        field(sb, 4, "model", device.model, true);
        field(sb, 4, "device", device.device, true);
        field(sb, 4, "hardware", device.hardware, true);
        field(sb, 4, "androidRelease", device.androidRelease, true);
        sb.append("    \"sdkInt\": ").append(device.sdkInt).append(",\n");
        field(sb, 4, "abis", device.abis, false);
        sb.append("  },\n");

        sb.append("  \"decoders\": [\n");
        for (int i = 0; i < decoders.size(); i++) {
            DecoderInfo d = decoders.get(i);
            sb.append("    {\n");
            field(sb, 6, "name", d.name, true);
            field(sb, 6, "mimeType", d.mimeType, true);
            bool(sb, 6, "hardwareAccelerated", d.hardwareAccelerated, true);
            bool(sb, 6, "softwareOnly", d.softwareOnly, true);
            bool(sb, 6, "vendor", d.vendor, true);
            bool(sb, 6, "featureLowLatency", d.featureLowLatency, true);
            field(sb, 6, "keyLowLatency", d.keyLowLatency, true);
            field(sb, 6, "keyLowLatencyError", d.keyLowLatencyError, true);
            num(sb, 6, "maxWidth", d.maxWidth, true);
            num(sb, 6, "maxHeight", d.maxHeight, true);
            num(sb, 6, "maxInstances", d.maxInstances, true);
            bool(sb, 6, "supports4k60", d.supports4k60, true);
            field(sb, 6, "vendorLowLatencyKeys", d.vendorLowLatencyKeys, true);
            field(sb, 6, "profileLevels", d.profileLevels, false);
            sb.append(i + 1 < decoders.size() ? "    },\n" : "    }\n");
        }
        sb.append("  ]\n");
        sb.append("}\n");
        return sb.toString();
    }

    private static void field(StringBuilder sb, int indent, String key, String value, boolean more) {
        indent(sb, indent);
        sb.append('"').append(key).append("\": \"").append(escape(value)).append('"');
        sb.append(more ? ",\n" : "\n");
    }

    private static void bool(StringBuilder sb, int indent, String key, boolean value, boolean more) {
        indent(sb, indent);
        sb.append('"').append(key).append("\": ").append(value);
        sb.append(more ? ",\n" : "\n");
    }

    private static void num(StringBuilder sb, int indent, String key, int value, boolean more) {
        indent(sb, indent);
        sb.append('"').append(key).append("\": ").append(value);
        sb.append(more ? ",\n" : "\n");
    }

    private static void indent(StringBuilder sb, int n) {
        sb.append(" ".repeat(n));
    }

    /**
     * Escape a JSON string.
     *
     * <p>Not decorative. Decoder names are vendor strings and have contained backslashes and quotes
     * before now; one of those unescaped turns the whole report into something no parser will read,
     * after the device has already been put away.
     */
    static String escape(String s) {
        if (s == null) {
            return "";
        }
        StringBuilder out = new StringBuilder(s.length() + 8);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"' -> out.append("\\\"");
                case '\\' -> out.append("\\\\");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default -> {
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
                }
            }
        }
        return out.toString();
    }

    /** Which box the report came from. */
    public static final class DeviceInfo {
        public String manufacturer = "";
        public String model = "";
        public String device = "";
        public String hardware = "";
        public String androidRelease = "";
        public int sdkInt;
        public String abis = "";
    }
}
