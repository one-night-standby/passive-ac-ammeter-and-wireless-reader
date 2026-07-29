package com.jun.nuedc.reader;

import android.content.Context;
import android.content.SharedPreferences;

public final class ReaderPreferences {
    public static final int DEFAULT_LOW_THRESHOLD_MA = 200;
    public static final int DEFAULT_HIGH_THRESHOLD_MA = 2000;
    public static final int DEFAULT_POLLING_INTERVAL_SECONDS = 120;
    public static final int MIN_POLLING_INTERVAL_SECONDS = 10;
    public static final int MAX_POLLING_INTERVAL_SECONDS = 300;
    public static final long SCAN_DURATION_MS = 8_000L;
    public static final long CONNECTION_TIMEOUT_MS = 10_000L;
    public static final long FRAME_TIMEOUT_MS = 6_000L;

    private static final String FILE_NAME = "reader_settings";
    private static final String KEY_POLLING_INTERVAL_MINUTES = "polling_interval_minutes";
    private static final String KEY_POLLING_INTERVAL_SECONDS = "polling_interval_seconds";
    private final SharedPreferences preferences;

    public ReaderPreferences(Context context) {
        preferences = context.getSharedPreferences(FILE_NAME, Context.MODE_PRIVATE);
    }

    public int pollingIntervalSeconds() {
        if (preferences.contains(KEY_POLLING_INTERVAL_SECONDS)) {
            return clampInterval(preferences.getInt(
                    KEY_POLLING_INTERVAL_SECONDS,
                    DEFAULT_POLLING_INTERVAL_SECONDS
            ));
        }

        int oldMinutes = preferences.getInt(KEY_POLLING_INTERVAL_MINUTES, 2);
        int migratedSeconds = clampInterval(oldMinutes * 60);
        preferences.edit()
                .putInt(KEY_POLLING_INTERVAL_SECONDS, migratedSeconds)
                .remove(KEY_POLLING_INTERVAL_MINUTES)
                .apply();
        return migratedSeconds;
    }

    public long pollingIntervalMs() {
        return pollingIntervalSeconds() * 1_000L;
    }

    public String pollingIntervalText() {
        return formatIntervalSeconds(pollingIntervalSeconds());
    }

    public void savePollingIntervalSeconds(int seconds) {
        if (seconds < MIN_POLLING_INTERVAL_SECONDS
                || seconds > MAX_POLLING_INTERVAL_SECONDS) {
            throw new IllegalArgumentException("Polling interval must be 10-300 seconds");
        }
        preferences.edit().putInt(KEY_POLLING_INTERVAL_SECONDS, seconds).apply();
    }

    public static String formatIntervalSeconds(int seconds) {
        if (seconds % 60 == 0) {
            return (seconds / 60) + "分钟";
        }
        return seconds + "秒";
    }

    private int clampInterval(int seconds) {
        return Math.max(
                MIN_POLLING_INTERVAL_SECONDS,
                Math.min(MAX_POLLING_INTERVAL_SECONDS, seconds)
        );
    }
}
