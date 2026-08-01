package com.nuedc.reader;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Path;
import android.util.AttributeSet;
import android.view.MotionEvent;
import android.view.View;

import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Date;
import java.util.List;
import java.util.Locale;

public final class TrendView extends View {
    private static final float MIN_SCALE = 1f;
    private static final float MAX_SCALE = 12f;
    private static final float MIN_GESTURE_SPAN_DP = 12f;

    private final Paint gridPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint textPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint linePaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final SimpleDateFormat axisTimeFormat =
            new SimpleDateFormat("HH:mm:ss", Locale.CHINA);
    private List<MeterReading> readings = new ArrayList<>();

    private float scaleX = 1f;
    private float scaleY = 1f;
    private float viewportStartX;
    private float viewportStartY;
    private float previousSpanX;
    private float previousSpanY;
    private boolean scaling;

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
        linePaint.setColor(Color.rgb(0, 96, 168));
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

    public void resetZoom() {
        scaleX = 1f;
        scaleY = 1f;
        viewportStartX = 0f;
        viewportStartY = 0f;
        invalidate();
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float left = dp(48);
        float top = dp(24);
        float right = getWidth() - dp(16);
        float bottom = getHeight() - dp(48);
        if (right <= left || bottom <= top) {
            return;
        }

        int maximum = 2_500;
        for (MeterReading reading : readings) {
            maximum = Math.max(maximum, reading.currentMa);
        }
        maximum = ((maximum + 499) / 500) * 500;

        float viewportWidth = 1f / scaleX;
        float viewportHeight = 1f / scaleY;
        for (int i = 0; i <= 5; i++) {
            float y = top + (bottom - top) * i / 5f;
            float x = left + (right - left) * i / 5f;
            canvas.drawLine(left, y, right, y, gridPaint);
            canvas.drawLine(x, top, x, bottom, gridPaint);
            float normalizedCurrent =
                    viewportStartY + viewportHeight * (5 - i) / 5f;
            String label = String.format(
                    Locale.CHINA,
                    "%.2f",
                    maximum * normalizedCurrent / 1_000f
            );
            canvas.drawText(label, dp(5), y + dp(4), textPaint);
        }
        canvas.drawText("电流/A", dp(5), dp(16), textPaint);

        if (readings.isEmpty()) {
            canvas.drawText("暂无有效读数", left + dp(24), top + dp(40), textPaint);
            return;
        }

        long minTime = readings.get(0).timestamp;
        long maxTime = readings.get(readings.size() - 1).timestamp;
        if (minTime == maxTime) {
            maxTime = minTime + 1;
        }
        long fullDuration = maxTime - minTime;
        long visibleStartTime = minTime + (long) (fullDuration * viewportStartX);
        long visibleEndTime =
                minTime + (long) (fullDuration * (viewportStartX + viewportWidth));
        String startLabel = axisTimeFormat.format(new Date(visibleStartTime));
        String endLabel = axisTimeFormat.format(new Date(visibleEndTime));
        float labelY = bottom + dp(20);
        canvas.drawText(startLabel, left, labelY, textPaint);
        canvas.drawText(
                endLabel,
                right - textPaint.measureText(endLabel),
                labelY,
                textPaint
        );
        canvas.drawText("时间", (left + right) / 2f - dp(12), getHeight() - dp(8), textPaint);

        Path path = new Path();
        boolean first = true;
        canvas.save();
        canvas.clipRect(left, top, right, bottom);
        for (MeterReading reading : readings) {
            if (reading.currentMa < 0) {
                continue;
            }
            float normalizedX =
                    (reading.timestamp - minTime) / (float) fullDuration;
            float normalizedY = reading.currentMa / (float) maximum;
            float x = left + (right - left)
                    * (normalizedX - viewportStartX) / viewportWidth;
            float y = bottom - (bottom - top)
                    * (normalizedY - viewportStartY) / viewportHeight;
            if (first) {
                path.moveTo(x, y);
                first = false;
            } else {
                path.lineTo(x, y);
            }
            canvas.drawCircle(x, y, dp(2.5f), linePaint);
        }
        canvas.drawPath(path, linePaint);
        canvas.restore();
    }

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        switch (event.getActionMasked()) {
            case MotionEvent.ACTION_DOWN:
                scaling = false;
                return true;
            case MotionEvent.ACTION_POINTER_DOWN:
                if (event.getPointerCount() >= 2) {
                    scaling = true;
                    captureSpans(event);
                    getParent().requestDisallowInterceptTouchEvent(true);
                }
                return true;
            case MotionEvent.ACTION_MOVE:
                if (scaling && event.getPointerCount() >= 2) {
                    updateZoom(event);
                    getParent().requestDisallowInterceptTouchEvent(true);
                }
                return true;
            case MotionEvent.ACTION_POINTER_UP:
                if (event.getPointerCount() - 1 < 2) {
                    scaling = false;
                    getParent().requestDisallowInterceptTouchEvent(false);
                }
                return true;
            case MotionEvent.ACTION_UP:
                scaling = false;
                getParent().requestDisallowInterceptTouchEvent(false);
                performClick();
                return true;
            case MotionEvent.ACTION_CANCEL:
                scaling = false;
                getParent().requestDisallowInterceptTouchEvent(false);
                return true;
            default:
                return super.onTouchEvent(event);
        }
    }

    @Override
    public boolean performClick() {
        super.performClick();
        return true;
    }

    private void captureSpans(MotionEvent event) {
        previousSpanX = Math.abs(event.getX(1) - event.getX(0));
        previousSpanY = Math.abs(event.getY(1) - event.getY(0));
    }

    private void updateZoom(MotionEvent event) {
        float spanX = Math.abs(event.getX(1) - event.getX(0));
        float spanY = Math.abs(event.getY(1) - event.getY(0));
        float minimumSpan = dp(MIN_GESTURE_SPAN_DP);
        float factorX = previousSpanX >= minimumSpan && spanX >= minimumSpan
                ? spanX / previousSpanX
                : 1f;
        float factorY = previousSpanY >= minimumSpan && spanY >= minimumSpan
                ? spanY / previousSpanY
                : 1f;

        float left = dp(48);
        float top = dp(24);
        float right = getWidth() - dp(16);
        float bottom = getHeight() - dp(48);
        float focusX = (event.getX(0) + event.getX(1)) / 2f;
        float focusY = (event.getY(0) + event.getY(1)) / 2f;
        applyZoom(factorX, factorY, focusX, focusY, left, top, right, bottom);

        previousSpanX = spanX;
        previousSpanY = spanY;
    }

    private void applyZoom(
            float factorX,
            float factorY,
            float focusX,
            float focusY,
            float left,
            float top,
            float right,
            float bottom
    ) {
        if (right <= left || bottom <= top) {
            return;
        }

        float focusFractionX = clamp((focusX - left) / (right - left), 0f, 1f);
        float oldWidth = 1f / scaleX;
        float newScaleX = clamp(scaleX * factorX, MIN_SCALE, MAX_SCALE);
        float newWidth = 1f / newScaleX;
        float focusDataX = viewportStartX + focusFractionX * oldWidth;
        viewportStartX = clamp(
                focusDataX - focusFractionX * newWidth,
                0f,
                1f - newWidth
        );
        scaleX = newScaleX;

        float focusFractionY = clamp((bottom - focusY) / (bottom - top), 0f, 1f);
        float oldHeight = 1f / scaleY;
        float newScaleY = clamp(scaleY * factorY, MIN_SCALE, MAX_SCALE);
        float newHeight = 1f / newScaleY;
        float focusDataY = viewportStartY + focusFractionY * oldHeight;
        viewportStartY = clamp(
                focusDataY - focusFractionY * newHeight,
                0f,
                1f - newHeight
        );
        scaleY = newScaleY;
        invalidate();
    }

    private float clamp(float value, float minimum, float maximum) {
        return Math.max(minimum, Math.min(maximum, value));
    }

    private float dp(float value) {
        return value * getResources().getDisplayMetrics().density;
    }
}
