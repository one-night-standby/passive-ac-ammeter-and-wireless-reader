package com.jun.nuedc.reader;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Path;
import android.util.AttributeSet;
import android.view.View;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

public final class TrendView extends View {
    private static final int[] COLORS = {
            Color.rgb(23, 107, 135),
            Color.rgb(255, 140, 66),
            Color.rgb(46, 125, 50),
            Color.rgb(123, 31, 162),
            Color.rgb(198, 40, 40),
            Color.rgb(0, 121, 107)
    };

    private final Paint gridPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint textPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint linePaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private List<MeterReading> readings = new ArrayList<>();

    public TrendView(Context context) {
        super(context);
        init();
    }

    public TrendView(Context context, AttributeSet attrs) {
        super(context, attrs);
        init();
    }

    private void init() {
        gridPaint.setColor(Color.rgb(215, 222, 226));
        gridPaint.setStrokeWidth(dp(1));
        textPaint.setColor(Color.DKGRAY);
        textPaint.setTextSize(dp(11));
        linePaint.setStyle(Paint.Style.STROKE);
        linePaint.setStrokeWidth(dp(2));
        linePaint.setStrokeCap(Paint.Cap.ROUND);
        linePaint.setStrokeJoin(Paint.Join.ROUND);
        setBackgroundColor(Color.WHITE);
    }

    public void setReadings(List<MeterReading> readings) {
        this.readings = readings == null ? new ArrayList<>() : new ArrayList<>(readings);
        invalidate();
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float left = dp(48);
        float top = dp(24);
        float right = getWidth() - dp(16);
        float bottom = getHeight() - dp(38);
        if (right <= left || bottom <= top) {
            return;
        }

        int maximum = 2500;
        for (MeterReading reading : readings) {
            maximum = Math.max(maximum, reading.currentMa);
        }
        maximum = ((maximum + 499) / 500) * 500;

        for (int i = 0; i <= 5; i++) {
            float y = top + (bottom - top) * i / 5f;
            canvas.drawLine(left, y, right, y, gridPaint);
            String label = String.format(Locale.CHINA, "%.1f", maximum * (5 - i) / 5000f);
            canvas.drawText(label, dp(6), y + dp(4), textPaint);
        }
        canvas.drawText("电流/A", dp(6), dp(16), textPaint);
        canvas.drawText("时间 →", right - dp(40), getHeight() - dp(10), textPaint);

        if (readings.isEmpty()) {
            canvas.drawText("暂无有效读数", left + dp(24), top + dp(40), textPaint);
            return;
        }

        Map<Integer, List<MeterReading>> grouped = new LinkedHashMap<>();
        for (MeterReading reading : readings) {
            if (reading.currentMa >= 0) {
                grouped.computeIfAbsent(reading.address, key -> new ArrayList<>()).add(reading);
            }
        }

        long minTime = readings.get(0).timestamp;
        long maxTime = readings.get(readings.size() - 1).timestamp;
        if (minTime == maxTime) {
            maxTime = minTime + 1;
        }

        int colorIndex = 0;
        for (Map.Entry<Integer, List<MeterReading>> entry : grouped.entrySet()) {
            linePaint.setColor(COLORS[colorIndex % COLORS.length]);
            Path path = new Path();
            boolean first = true;
            for (MeterReading reading : entry.getValue()) {
                float x = left + (right - left) *
                        (reading.timestamp - minTime) / (float) (maxTime - minTime);
                float y = bottom - (bottom - top) * reading.currentMa / maximum;
                if (first) {
                    path.moveTo(x, y);
                    first = false;
                } else {
                    path.lineTo(x, y);
                }
                canvas.drawCircle(x, y, dp(2.5f), linePaint);
            }
            canvas.drawPath(path, linePaint);
            canvas.drawText(
                    String.format(Locale.CHINA, "%02d号", entry.getKey()),
                    left + colorIndex * dp(50),
                    getHeight() - dp(10),
                    linePaint
            );
            colorIndex++;
        }
    }

    private float dp(float value) {
        return value * getResources().getDisplayMetrics().density;
    }
}
