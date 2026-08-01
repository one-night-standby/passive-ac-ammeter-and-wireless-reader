package com.jun.nuedc.reader;

import java.util.Locale;

public final class MeterReading {
    public static final String NORMAL = "NORMAL";
    public static final String LOW = "LOW";
    public static final String HIGH = "HIGH";
    public static final String OFFLINE = "OFFLINE";
    /** 表在、也答了，但线上几乎没有电流：负载断开，不是电流偏低。 */
    public static final String DISCONNECTED = "DISCONNECTED";

    /**
     * 判成负载断开的上限，mA。
     *
     * <p>低限是可调的报警阈值，这个不是：0.1 A 以下不是「电流小」而是「没有
     * 电流」，把它报成低限会让人去查负载为什么变轻，而实际上负载根本不在。两者
     * 要分开说，所以这个判据固定在前面，不跟着 {@link ReaderPreferences} 的低限走。
     */
    public static final int DISCONNECT_MA = 100;

    public final long id;
    public final long timestamp;
    public final int address;
    public final int currentMa;
    public final String status;
    public final String mac;
    public final String deviceName;
    public final int rssi;
    public final String source;

    public MeterReading(
            long id,
            long timestamp,
            int address,
            int currentMa,
            String status,
            String mac,
            String deviceName,
            int rssi,
            String source
    ) {
        this.id = id;
        this.timestamp = timestamp;
        this.address = address;
        this.currentMa = currentMa;
        this.status = status;
        this.mac = mac == null ? "" : mac;
        this.deviceName = deviceName == null ? "" : deviceName;
        this.rssi = rssi;
        this.source = source == null ? "AUTO" : source;
    }

    public static String classify(int currentMa, int lowThresholdMa, int highThresholdMa) {
        if (currentMa < DISCONNECT_MA) {
            return DISCONNECTED;
        }
        if (currentMa < lowThresholdMa) {
            return LOW;
        }
        if (currentMa > highThresholdMa) {
            return HIGH;
        }
        return NORMAL;
    }

    public String currentText() {
        if (currentMa < 0) {
            return "--";
        }
        return String.format(Locale.CHINA, "%.3f A", currentMa / 1000.0);
    }

    public String statusText() {
        switch (status) {
            case LOW:
                return "低电流";
            case HIGH:
                return "高电流";
            case DISCONNECTED:
                return "负载断开";
            case OFFLINE:
                return "离线";
            default:
                return "正常";
        }
    }
}
