package com.jun.nuedc.reader;

import android.content.Context;
import android.content.SharedPreferences;

public final class ReaderPreferences {
    public static final int DEFAULT_LOW_THRESHOLD_MA = 200;
    public static final int DEFAULT_HIGH_THRESHOLD_MA = 2000;
    public static final int DEFAULT_POLLING_INTERVAL_MINUTES = 2;
    public static final long SCAN_DURATION_MS = 8_000L;
    public static final long CONNECTION_TIMEOUT_MS = 10_000L;
    public static final long FRAME_TIMEOUT_MS = 6_000L;

    private static final String FILE_NAME = "reader_settings";
    private static final String KEY_POLLING_INTERVAL_MINUTES = "polling_interval_minutes";
    private final SharedPreferences preferences;

    public ReaderPreferences(Context context) {
        preferences = context.getSharedPreferences(FILE_NAME, Context.MODE_PRIVATE);
    }

    public int pollingIntervalMinutes() {
        int value = preferences.getInt(
                KEY_POLLING_INTERVAL_MINUTES,
                DEFAULT_POLLING_INTERVAL_MINUTES
        );
        return Math.max(1, Math.min(60, value));
    }

    public long pollingIntervalMs() {
        return pollingIntervalMinutes() * 60_000L;
    }

    public void savePollingIntervalMinutes(int minutes) {
        if (minutes < 1 || minutes > 60) {
            throw new IllegalArgumentException("Polling interval must be 1-60 minutes");
        }
        preferences.edit().putInt(KEY_POLLING_INTERVAL_MINUTES, minutes).apply();
    }
}
