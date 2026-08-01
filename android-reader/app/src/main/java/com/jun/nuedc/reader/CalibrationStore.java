package com.jun.nuedc.reader;

import android.content.Context;
import android.content.SharedPreferences;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/**
 * 标定台自己的一点状态:采下来的点、地址、靶位数,以及"这张表推成功过没有"。
 *
 * <p>和读表器的 {@link ReaderPreferences} 分开存,因为它们的寿命不一样:阈值和
 * 采集间隔是作品的设置,而这些点是一次标定会话的中间产物,清掉不该影响读表器。
 *
 * <p>{@code pushed} 那一位是给自动重推用的。它记的不是"手机上有几个点",而是
 * "这些点曾经在表里生效过"——只有这样,看到 SRC=ROM 时才分得清是表掉过电、
 * 还是这张表本来就没推过。改动任何一个点都会把它清掉:改过之后手机上的表和
 * 表内的表已经不是同一张,再自动推就是拿没验证过的表盖掉验证过的。
 */
public final class CalibrationStore {
    private static final String FILE_NAME = "calibration_bench";
    private static final String KEY_POINTS = "points";
    private static final String KEY_ADDRESS = "address";
    private static final String KEY_TARGET_COUNT = "target_count";
    private static final String KEY_PUSHED = "pushed";

    private static final int DEFAULT_ADDRESS = 6;
    private static final int DEFAULT_TARGET_COUNT = 10;

    private final SharedPreferences preferences;

    public CalibrationStore(Context context) {
        preferences = context.getSharedPreferences(FILE_NAME, Context.MODE_PRIVATE);
    }

    /** 升序的 {@code {rms, amps}}。存坏了就当没有,不猜。 */
    public List<double[]> points() {
        List<double[]> points = new ArrayList<>();
        String raw = preferences.getString(KEY_POINTS, "");
        if (raw == null || raw.isEmpty()) {
            return points;
        }
        for (String item : raw.split(";")) {
            String[] pair = item.split(":");
            if (pair.length != 2) {
                continue;
            }
            try {
                points.add(new double[]{
                        Double.parseDouble(pair[0]),
                        Double.parseDouble(pair[1])
                });
            } catch (NumberFormatException ignored) {
                // 一条坏记录不该带走整张表
            }
        }
        return points;
    }

    public void savePoints(List<double[]> points) {
        StringBuilder raw = new StringBuilder();
        for (double[] point : points) {
            if (raw.length() > 0) {
                raw.append(';');
            }
            raw.append(String.format(Locale.ROOT, "%.4f:%.6f", point[0], point[1]));
        }
        preferences.edit().putString(KEY_POINTS, raw.toString()).apply();
    }

    public int address() {
        int value = preferences.getInt(KEY_ADDRESS, DEFAULT_ADDRESS);
        return value < 0 || value > 15 ? DEFAULT_ADDRESS : value;
    }

    public void saveAddress(int address) {
        preferences.edit().putInt(KEY_ADDRESS, address).apply();
    }

    public int targetCount() {
        int value = preferences.getInt(KEY_TARGET_COUNT, DEFAULT_TARGET_COUNT);
        return value < 2 || value > 16 ? DEFAULT_TARGET_COUNT : value;
    }

    public void saveTargetCount(int count) {
        preferences.edit().putInt(KEY_TARGET_COUNT, count).apply();
    }

    public boolean pushed() {
        return preferences.getBoolean(KEY_PUSHED, false);
    }

    public void markPushed(boolean pushed) {
        preferences.edit().putBoolean(KEY_PUSHED, pushed).apply();
    }
}
