package com.jun.nuedc.reader;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.BlurMaskFilter;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.RenderEffect;
import android.graphics.Shader;
import android.os.Build;
import android.view.View;

public final class GlassBackgroundView extends View {
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);

    public GlassBackgroundView(Context context) {
        super(context);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            setRenderEffect(RenderEffect.createBlurEffect(55f, 55f, Shader.TileMode.CLAMP));
        } else {
            setLayerType(View.LAYER_TYPE_SOFTWARE, null);
            paint.setMaskFilter(new BlurMaskFilter(55f, BlurMaskFilter.Blur.NORMAL));
        }
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float width = getWidth();
        float height = getHeight();

        paint.setColor(Color.argb(105, 72, 151, 255));
        canvas.drawCircle(width * 0.88f, height * 0.14f, width * 0.34f, paint);

        paint.setColor(Color.argb(82, 112, 225, 205));
        canvas.drawCircle(width * 0.05f, height * 0.48f, width * 0.32f, paint);

        paint.setColor(Color.argb(72, 190, 126, 255));
        canvas.drawCircle(width * 0.82f, height * 0.82f, width * 0.40f, paint);
    }
}
