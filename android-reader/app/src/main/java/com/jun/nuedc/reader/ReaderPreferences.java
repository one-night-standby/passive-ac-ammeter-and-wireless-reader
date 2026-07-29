package com.jun.nuedc.reader;

public final class ReaderPreferences {
    public static final int DEFAULT_LOW_THRESHOLD_MA = 200;
    public static final int DEFAULT_HIGH_THRESHOLD_MA = 2000;
    public static final long POLLING_INTERVAL_MS = 120_000L;
    public static final long SCAN_DURATION_MS = 8_000L;
    public static final long CONNECTION_TIMEOUT_MS = 10_000L;
    public static final long FRAME_TIMEOUT_MS = 6_000L;

    private ReaderPreferences() {
    }
}
