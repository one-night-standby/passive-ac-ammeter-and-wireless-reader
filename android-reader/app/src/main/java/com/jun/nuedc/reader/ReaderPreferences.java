package com.jun.nuedc.reader;

import android.content.Context;
import android.content.SharedPreferences;

public final class ReaderPreferences {
    public static final int DEFAULT_LOW_THRESHOLD_MA = 200;
    public static final int DEFAULT_HIGH_THRESHOLD_MA = 2000;
    /** 阈值可调的上限。电流表一帧最多七位数，但界面按 x.xxx A 显示，超过就写不下。 */
    public static final int MAX_THRESHOLD_MA = 9_999;
    public static final int DEFAULT_POLLING_INTERVAL_SECONDS = 120;
    public static final int MIN_POLLING_INTERVAL_SECONDS = 10;
    public static final int MAX_POLLING_INTERVAL_SECONDS = 300 * 60;
    public static final long SCAN_DURATION_MS = 8_000L;
    public static final long CONNECTION_TIMEOUT_MS = 10_000L;
    /**
     * 发出 MEAS 之后等应答的时限。
     *
     * <p>决定这个数的不是测量本身。电流表读完一次会亮屏 1 秒，这一秒里它不接
     * 新请求，命令只是留在串口缓冲里等着——所以最坏情况是命令刚好赶在亮屏那一
     * 刻到，要等 1 秒灭屏、再测 260 ms、再加链路时延才有应答，接近 1.5 秒。按
     * 300-400 ms 的往返来定时限，等于把这一类<b>迟到但正常</b>的应答判成没读到，
     * 而它随后还是会到、并把界面刷成新值——于是记录里存的是旧值、表上显示的是
     * 新值。取 3 秒，把亮屏窗口整个盖住。
     *
     * <p>超时只说明这一次没读到，<b>不代表这台表离线</b>：在场与否由心跳说了算。
     */
    public static final long REPLY_TIMEOUT_MS = 3_000L;

    private static final String FILE_NAME = "reader_settings";
    private static final String KEY_POLLING_INTERVAL_MINUTES = "polling_interval_minutes";
    private static final String KEY_POLLING_INTERVAL_SECONDS = "polling_interval_seconds";
    private static final String KEY_LOW_THRESHOLD_MA = "low_threshold_ma";
    private static final String KEY_HIGH_THRESHOLD_MA = "high_threshold_ma";
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
            throw new IllegalArgumentException("Polling interval must be "
                    + MIN_POLLING_INTERVAL_SECONDS + "-" + MAX_POLLING_INTERVAL_SECONDS
                    + " seconds");
        }
        preferences.edit().putInt(KEY_POLLING_INTERVAL_SECONDS, seconds).apply();
    }

    public int lowThresholdMa() {
        return clampThreshold(
                preferences.getInt(KEY_LOW_THRESHOLD_MA, DEFAULT_LOW_THRESHOLD_MA));
    }

    public int highThresholdMa() {
        int high = clampThreshold(
                preferences.getInt(KEY_HIGH_THRESHOLD_MA, DEFAULT_HIGH_THRESHOLD_MA));
        // 低限必须严格小于超限，否则 classify 会把每一个读数都判成越限。存的时候
        // 已经拦过一次，这里兜住手改过 xml、或者只有一个键落盘的情况。
        return high > lowThresholdMa() ? high : DEFAULT_HIGH_THRESHOLD_MA;
    }

    /** 两个阈值一起存：它们是一对约束，分开存会出现低限大于超限的中间态。 */
    public void saveThresholds(int lowMa, int highMa) {
        if (lowMa < 0 || highMa > MAX_THRESHOLD_MA || lowMa >= highMa) {
            throw new IllegalArgumentException(
                    "Thresholds must satisfy 0 <= low < high <= " + MAX_THRESHOLD_MA + " mA");
        }
        preferences.edit()
                .putInt(KEY_LOW_THRESHOLD_MA, lowMa)
                .putInt(KEY_HIGH_THRESHOLD_MA, highMa)
                .apply();
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

    private int clampThreshold(int ma) {
        return Math.max(0, Math.min(MAX_THRESHOLD_MA, ma));
    }
}
