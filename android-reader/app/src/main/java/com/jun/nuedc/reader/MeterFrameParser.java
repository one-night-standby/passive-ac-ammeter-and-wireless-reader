package com.jun.nuedc.reader;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * 电流表上报的两种帧。
 *
 * <p>{@code METER_TEST} 是读数帧，正则锚到行尾，字段不可扩展。
 * {@code METER_CAL} 带原始 RMS、档位和质量标志，电流表在读数不可信时
 * <b>只发这一条、不发 METER_TEST</b>——所以它不是可选的调试信息：不解析它，
 * 「表故障」和「表离线」在读表器看来完全一样。
 */
public final class MeterFrameParser {
    private static final int MAX_BUFFER_LENGTH = 2048;
    private static final Pattern FRAME_PATTERN = Pattern.compile(
            "^METER_TEST,ADDR=(\\d{1,2}),CURRENT_MA=(\\d{1,7}),STATUS=([A-Z_]+)$"
    );
    /** METER_CAL 的字段会随固件增减，所以只锚地址和标志，中间随它长。 */
    private static final Pattern CAL_PATTERN = Pattern.compile(
            "^METER_CAL,ADDR=(\\d{1,2}),.*?FLAG=([A-Z_]+).*$"
    );

    private final StringBuilder buffer = new StringBuilder();

    public List<ParsedFrame> feed(byte[] data) {
        List<ParsedFrame> frames = new ArrayList<>();
        if (data == null || data.length == 0) {
            return frames;
        }

        buffer.append(new String(data, StandardCharsets.US_ASCII));
        if (buffer.length() > MAX_BUFFER_LENGTH) {
            buffer.delete(0, buffer.length() - MAX_BUFFER_LENGTH);
        }

        int newline;
        while ((newline = buffer.indexOf("\n")) >= 0) {
            String line = buffer.substring(0, newline).replace("\r", "").trim();
            buffer.delete(0, newline + 1);
            ParsedFrame frame = parseLine(line);
            if (frame != null) {
                frames.add(frame);
            }
        }
        return frames;
    }

    public ParsedFrame parseLine(String line) {
        if (line == null) {
            return null;
        }
        String trimmed = line.trim();

        Matcher matcher = FRAME_PATTERN.matcher(trimmed);
        if (matcher.matches()) {
            int address;
            int currentMa;
            try {
                address = Integer.parseInt(matcher.group(1));
                currentMa = Integer.parseInt(matcher.group(2));
            } catch (NumberFormatException ignored) {
                return null;
            }
            if (address < 0 || address > 15 || currentMa < 0) {
                return null;
            }
            return new ParsedFrame(address, currentMa, matcher.group(3), null, trimmed);
        }

        Matcher cal = CAL_PATTERN.matcher(trimmed);
        if (cal.matches()) {
            int address;
            try {
                address = Integer.parseInt(cal.group(1));
            } catch (NumberFormatException ignored) {
                return null;
            }
            if (address < 0 || address > 15) {
                return null;
            }
            // 电流留 -1：这一条本来就不带读数，填 0 会被当成一次真实的低限报警。
            return new ParsedFrame(address, -1, null, cal.group(2), trimmed);
        }
        return null;
    }

    public void reset() {
        buffer.setLength(0);
    }

    public static final class ParsedFrame {
        public final int address;
        /** METER_TEST 的电流；METER_CAL 帧为 -1。 */
        public final int currentMa;
        /** METER_TEST 的 STATUS；METER_CAL 帧为 null。 */
        public final String meterStatus;
        /** METER_CAL 的 FLAG；METER_TEST 帧为 null。OK 之外都表示这次读数不可信。 */
        public final String flag;
        public final String raw;

        ParsedFrame(int address, int currentMa, String meterStatus, String flag, String raw) {
            this.address = address;
            this.currentMa = currentMa;
            this.meterStatus = meterStatus;
            this.flag = flag;
            this.raw = raw;
        }

        /** 带读数的帧。只有这种能落库。 */
        public boolean hasReading() {
            return currentMa >= 0;
        }

        /** 电流表答了，但明说这次读数不可信——必须和「没人应答」分开报。 */
        public boolean isFault() {
            return flag != null && !"OK".equals(flag);
        }
    }
}
