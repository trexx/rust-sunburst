// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst.probe;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.widget.ScrollView;
import android.widget.TextView;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.List;

/**
 * Runs the Phase 0.2 enumeration and writes the result where adb can fetch it.
 *
 * <pre>
 * adb install -r app/build/outputs/apk/debug/app-debug.apk
 * adb shell am start -n com.trexx.sunburst/.probe.DecoderProbeActivity
 * adb pull /sdcard/Android/data/com.trexx.sunburst/files/decoders.json
 * </pre>
 *
 * <p><b>The file is the output, not logcat.</b> The Homatics ships with
 * {@code persist.log.tag=S}, which silences the whole main buffer — not just this app — so a probe
 * that only logged would look exactly like a probe that never ran. The log lines below are a
 * convenience for the Shield, not the deliverable.
 */
public final class DecoderProbeActivity extends Activity {

    private static final String TAG = "SunburstProbe";
    private static final String FILENAME = "decoders.json";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        TextView text = new TextView(this);
        text.setPadding(48, 48, 48, 48);
        text.setTextSize(14);
        ScrollView scroll = new ScrollView(this);
        scroll.addView(text);
        setContentView(scroll);

        String json;
        try {
            ProbeReport.DeviceInfo device = DecoderProbe.deviceInfo();
            List<DecoderInfo> decoders = DecoderProbe.enumerate();
            json = ProbeReport.toJson(device, decoders);
            text.setText(summarise(device, decoders) + "\n\n" + json);
        } catch (RuntimeException e) {
            // A probe that dies silently is worse than one that reports why: the device is usually
            // put away again before anyone checks.
            json = "{\"error\": \"" + ProbeReport.escape(e.toString()) + "\"}\n";
            text.setText("Enumeration failed:\n" + e);
            Log.e(TAG, "enumeration failed", e);
        }

        String written = write(json);
        text.append("\n\n" + written);
        Log.i(TAG, written);
    }

    private String summarise(ProbeReport.DeviceInfo device, List<DecoderInfo> decoders) {
        StringBuilder sb = new StringBuilder();
        sb.append(device.manufacturer)
                .append(' ')
                .append(device.model)
                .append("  Android ")
                .append(device.androidRelease)
                .append(" (API ")
                .append(device.sdkInt)
                .append(")\n")
                .append(device.abis)
                .append("\n\n");

        if (decoders.isEmpty()) {
            sb.append("No HEVC or AV1 decoder found at all.\n");
            sb.append("CLAUDE.md says never to assume a codec exists; this is that case.\n");
            return sb.toString();
        }

        for (DecoderInfo d : decoders) {
            sb.append(d.name)
                    .append("\n  ")
                    .append(d.mimeType)
                    .append(d.hardwareAccelerated ? "  hw" : "  SOFTWARE")
                    .append("  ")
                    .append(d.maxWidth)
                    .append('x')
                    .append(d.maxHeight)
                    .append(d.supports4k60 ? "  4K60 ok" : "  NO 4K60")
                    .append("\n  lowLatency: feature=")
                    .append(d.featureLowLatency)
                    .append(" key=")
                    .append(d.keyLowLatency);
            if (!d.vendorLowLatencyKeys.isEmpty()) {
                sb.append("\n  vendor keys: ").append(d.vendorLowLatencyKeys);
            }
            sb.append('\n');
        }
        return sb.toString();
    }

    private String write(String json) {
        File dir = getExternalFilesDir(null);
        if (dir == null) {
            return "No external files dir; could not write " + FILENAME;
        }
        File out = new File(dir, FILENAME);
        try (FileOutputStream fos = new FileOutputStream(out)) {
            fos.write(json.getBytes(StandardCharsets.UTF_8));
            return "Wrote " + out.getAbsolutePath();
        } catch (IOException e) {
            return "Failed to write " + out.getAbsolutePath() + ": " + e;
        }
    }
}
