package com.jun.nuedc.reader;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public final class MeterFrameParser {
    private static final int MAX_BUFFER_LENGTH = 2048;
    private static final Pattern FRAME_PATTERN = Pattern.compile(
            "^METER_TEST,ADDR=(\\d{1,2}),CURRENT_MA=(\\d{1,7}),STATUS=([A-Z_]+)$"
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
        Matcher matcher = FRAME_PATTERN.matcher(line.trim());
        if (!matcher.matches()) {
            return null;
        }

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
        return new ParsedFrame(address, currentMa, matcher.group(3), line);
    }

    public void reset() {
        buffer.setLength(0);
    }

    public static final class ParsedFrame {
        public final int address;
        public final int currentMa;
        public final String meterStatus;
        public final String raw;

        ParsedFrame(int address, int currentMa, String meterStatus, String raw) {
            this.address = address;
            this.currentMa = currentMa;
            this.meterStatus = meterStatus;
            this.raw = raw;
        }
    }
}
