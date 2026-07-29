package com.jun.nuedc.reader;

import android.content.Context;
import android.content.SharedPreferences;

public final class ReaderPreferences {
    public static final int DEFAULT_LOW_THRESHOLD_MA = 200;
    public static final int DEFAULT_HIGH_THRESHOLD_MA = 2000;
    public static final long POLLING_INTERVAL_MS = 120_000L;
    public static final long SCAN_DURATION_MS = 8_000L;
    public static final long CONNECTION_TIMEOUT_MS = 10_000L;
    public static final long FRAME_TIMEOUT_MS = 6_000L;

    private static final String FILE_NAME = "reader_settings";
    private static final String KEY_LOW_THRESHOLD = "low_threshold_ma";
    private static final String KEY_HIGH_THRESHOLD = "high_threshold_ma";
    private static final String KEY_DIALOG_HEX = "dialog_hex";
    private static final String KEY_APPEND_CRLF = "append_crlf";

    private final SharedPreferences preferences;

    public ReaderPreferences(Context context) {
        preferences = context.getSharedPreferences(FILE_NAME, Context.MODE_PRIVATE);
    }

    public int lowThresholdMa() {
        return preferences.getInt(KEY_LOW_THRESHOLD, DEFAULT_LOW_THRESHOLD_MA);
    }

    public int highThresholdMa() {
        return preferences.getInt(KEY_HIGH_THRESHOLD, DEFAULT_HIGH_THRESHOLD_MA);
    }

    public boolean saveThresholds(int lowMa, int highMa) {
        if (lowMa < 0 || highMa <= lowMa) {
            return false;
        }
        preferences.edit()
                .putInt(KEY_LOW_THRESHOLD, lowMa)
                .putInt(KEY_HIGH_THRESHOLD, highMa)
                .apply();
        return true;
    }

    public boolean dialogHex() {
        return preferences.getBoolean(KEY_DIALOG_HEX, false);
    }

    public boolean appendCrLf() {
        return preferences.getBoolean(KEY_APPEND_CRLF, false);
    }

    public void saveDialogOptions(boolean hex, boolean appendCrLf) {
        preferences.edit()
                .putBoolean(KEY_DIALOG_HEX, hex)
                .putBoolean(KEY_APPEND_CRLF, appendCrLf)
                .apply();
    }
}
