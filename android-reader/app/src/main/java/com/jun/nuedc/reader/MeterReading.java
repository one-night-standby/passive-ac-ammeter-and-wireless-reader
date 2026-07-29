package com.jun.nuedc.reader;

import java.util.Locale;

public final class MeterReading {
    public static final String NORMAL = "NORMAL";
    public static final String LOW = "LOW";
    public static final String HIGH = "HIGH";
    public static final String OFFLINE = "OFFLINE";

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
            case OFFLINE:
                return "离线";
            default:
                return "正常";
        }
    }
}
