package com.nuedc.reader;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * 电流表上报的两种帧。
 *
 * <p>{@code METER_TEST} 是读数帧，正则锚到行尾，字段不可扩展。每一次读数都会
 * 发这一条，电流表面板上显示什么，这里就是什么。
 *
 * <p>{@code METER_CAL} 带原始 RMS、档位和 FLAG，是给标定台用的：它和读数一起
 * 发，不代替读数，FLAG 也不是读数的状态标记。
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
    /**
     * 保活心跳。电流表空闲时按 HEARTBEAT_MS 播一次自己的地址，不带读数，
     * 也不开模拟部分。它有两个作用：拨码开关一动，旧地址的心跳停、新地址的
     * 开始，读表器立刻知道身份变了；负载一断电流表跟着掉电，心跳随之消失，
     * 那就是 4.3 说的离线，比等一次轮询超时快得多。
     */
    private static final Pattern ALIVE_PATTERN = Pattern.compile(
            "^IMHERE,ADDR=(\\d{1,2})$"
    );

    private final StringBuilder buffer = new StringBuilder();

    /**
     * 收到的字节里已经完整的那些行,原文照给。
     *
     * <p>拆行和解帧分开,是因为标定端要看的行这个解析器不认得——{@code CALACK}、
     * {@code CALSTAT} 是标定推送的应答,读表器本身用不上。让它们从这里原样流出去,
     * 好过在 {@link #parseLine} 里给每一条新语法加一个分支、再让读表器的采集逻辑
     * 去跳过它们。
     */
    public List<String> feedLines(byte[] data) {
        List<String> lines = new ArrayList<>();
        if (data == null || data.length == 0) {
            return lines;
        }

        buffer.append(new String(data, StandardCharsets.US_ASCII));
        if (buffer.length() > MAX_BUFFER_LENGTH) {
            buffer.delete(0, buffer.length() - MAX_BUFFER_LENGTH);
        }

        int newline;
        while ((newline = buffer.indexOf("\n")) >= 0) {
            lines.add(buffer.substring(0, newline).replace("\r", "").trim());
            buffer.delete(0, newline + 1);
        }
        return lines;
    }

    public List<ParsedFrame> feed(byte[] data) {
        List<ParsedFrame> frames = new ArrayList<>();
        for (String line : feedLines(data)) {
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
            return new ParsedFrame(address, currentMa, matcher.group(3), null, false, trimmed);
        }

        Matcher alive = ALIVE_PATTERN.matcher(trimmed);
        if (alive.matches()) {
            int address;
            try {
                address = Integer.parseInt(alive.group(1));
            } catch (NumberFormatException ignored) {
                return null;
            }
            if (address < 0 || address > 15) {
                return null;
            }
            return new ParsedFrame(address, -1, null, null, true, trimmed);
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
            return new ParsedFrame(address, -1, null, cal.group(2), false, trimmed);
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
        /** METER_CAL 的 FLAG；METER_TEST 帧为 null。标定台的旁注，不是读数的状态。 */
        public final String flag;
        /** 保活心跳：只说明这个地址此刻在线，不是任何请求的应答。 */
        public final boolean alive;
        public final String raw;

        ParsedFrame(int address, int currentMa, String meterStatus, String flag,
                    boolean alive, String raw) {
            this.address = address;
            this.currentMa = currentMa;
            this.meterStatus = meterStatus;
            this.flag = flag;
            this.alive = alive;
            this.raw = raw;
        }

        /** 带读数的帧。只有这种能落库。 */
        public boolean hasReading() {
            return currentMa >= 0;
        }
    }
}
