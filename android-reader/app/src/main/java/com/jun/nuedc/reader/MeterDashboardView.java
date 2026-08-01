package com.jun.nuedc.reader;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.DashPathEffect;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.RectF;
import android.graphics.Typeface;
import android.media.AudioAttributes;
import android.media.Ringtone;
import android.media.RingtoneManager;
import android.net.Uri;
import android.os.SystemClock;
import android.os.VibrationEffect;
import android.os.Vibrator;
import android.view.MotionEvent;
import android.view.View;
import android.view.animation.PathInterpolator;

import java.text.SimpleDateFormat;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Date;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;

/**
 * app-prototype.html 的移植:布局、交互与所有动画细节与原型逐项对应。
 * 渲染循环对应原型的 requestAnimationFrame,60 ms 模型定时器对应原型的
 * setInterval —— 读条落库、静默判定、自动倒计时不依赖是否在画。
 */
public final class MeterDashboardView extends View {
    public interface Listener {
        /** 读到的这一次落库。只在真读到数时调用。调用方应同步刷新 setData。 */
        void onCaptureRecord(int address, int currentMa, String status,
                             String mac, String deviceName, int rssi);
        /** 基本模式:向这台表要一次读数。电流表不会主动报数,不问就没有。 */
        void onReadRequest(int address);
        /** 要了但没答上来。不落库,只提示——这次读取没有结果,不是这台表不在。 */
        void onReadFailed(int address);
        void onAutoModeChanged(boolean enabled);
        void onClearHistory();
        /** 小键盘上改好的间隔,已经在 10 秒 - 300 分钟内。 */
        void onIntervalChanged(int seconds);
        /** 小键盘上改好的阈值,单位 mA,已经满足 0 ≤ 低限 < 超限 ≤ 9999。 */
        void onThresholdsChanged(int lowThresholdMa, int highThresholdMa);
    }

    /* ══════════ 与原型对齐的常量 ══════════ */
    private static final int SLOTS = 16;
    private static final float STEP = 360f / SLOTS;          // 22.5°
    /* 读条时长。它同时是「发出请求到落库」的窗口:电流表答一次要测量 260 ms
       加链路时延,服务端的应答时限是 REPLY_TIMEOUT_MS,读条必须比它长,否则
       读条走完时应答还在路上,存下去的就是上一次的值。 */
    private static final long STORE_MS = 1400;
    private static final long ALERT_MS = 2200;                // 状态变化后的短暂提示
    /* 同一地址的响铃/震动去重窗口。不是「冷却」:每一次读数只要越限就该响,
       包括电流表上按键触发、值没变的那一次。这里只挡同一次读数被重复触发
       (比如同一帧被处理两遍),所以取得很短——两次真实读数之间的间隔,最快
       也是人按一下按键的间隔。 */
    private static final long ALARM_DEDUP_MS = 400;
    private static final int PAGE_SIZE = 6;
    private static final int CAP = MeterDatabase.MAX_STORED_READINGS;
    private static final String BLANK = "-.---";

    private static final PathInterpolator EASE = new PathInterpolator(0.22f, 1f, 0.36f, 1f);
    private static final PathInterpolator FLY = new PathInterpolator(0.34f, 0.9f, 0.3f, 1f);

    /* ══════════ 颜色(与原型 CSS 变量一致) ══════════ */
    private static final int PAGE = 0xFFE8ECF2;
    private static final int FRAME = 0xFFFFFFFF;
    private static final int SUNKEN = 0xFFF1F4F9;
    private static final int INK = 0xFF131922;
    private static final int INK2 = 0x94131922;               // rgba(19,25,34,.58)
    private static final int INK3 = 0x57131922;               // rgba(19,25,34,.34)
    private static final int LINE = 0x1A131922;               // rgba(19,25,34,.10)
    private static final int LINE2 = 0x29131922;              // rgba(19,25,34,.16)
    private static final int ACCENT = 0xFF1F6FEB;
    private static final int C_OK = 0xFF0D9464;
    private static final int C_LOW = 0xFFC87C00;
    private static final int C_HIGH = 0xFFDC2626;
    private static final int C_OFF = 0xFF828C98;              // rgb(130,140,152)
    private static final int SLOT_OFF = 0xFF8A94A3;           // .slot.is-off

    /* ══════════ 设计坐标(980×462,对应原型 CSS 布局) ══════════ */
    private static final float DESIGN_W = 980f;
    private static final float DESIGN_H = 462f;
    private static final float SB_CY = 17.6f;
    private static final float DIAL_X = 33f, DIAL_Y = 31.25f, DIAL_C = 183f;
    private static final float SLOT_R = 144f, RING_R = 168f, COUNT_R = 104f, HUB_R = 88f;
    private static final float SLOT_HALF_W = 32f, SLOT_HALF_H = 17f;
    private static final float HUB_ADDR_BASE = 148f, HUB_VALUE_BASE = 189f;
    private static final float HUB_STATE_BASE = 210.5f, HUB_SUB_BASE = 228f;
    private static final float LEFT_CENTER = 216f;
    private static final float FOOT_CY = 422.6f;
    private static final float TOGGLE_W = 122f, TOGGLE_H = 36.75f, TOGGLE_BTN_W = 58f;
    private static final float RIGHT_X = 432f;
    private static final float HEAD_BASE = 53f;
    private static final RectF CHART_CARD = new RectF(432, 66.5f, 960, 222.5f);
    private static final float PLOT_X = 442f, PLOT_Y = 74.5f, PLOT_W = 508f, PLOT_H = 144f;
    private static final float CHW = 520f, CHH = 156f;
    private static final float CH_L = 30f, CH_R = 488f, CH_T = 12f, CH_B = 124f;
    private static final float LOGHEAD_CY = 239.1f;
    private static final RectF LOG_RECT = new RectF(432, 255.75f, 960, 446);
    private static final float ROW_H = 31.5f;
    private static final float REC_T_X = 444f, REC_A_X = 514f, REC_SRC_X = 546f;
    private static final float REC_V_RIGHT = 898f, REC_S_RIGHT = 948f;
    private static final RectF DIAL_RECT = new RectF(33, 31.25f, 399, 397.25f);
    private static final RectF CHART_WRAP = new RectF(442, 74.5f, 950, 218.5f);

    /* ══════════ 补间:对应 CSS transition(改目标时从当前值重新过渡) ══════════ */
    private static final class Tween {
        float value, from, to;
        long start;
        int dur;

        Tween(float v) { value = from = to = v; }

        void retarget(float target, long now, int durMs) {
            if (target == to) return;
            from = value;
            to = target;
            start = now;
            dur = durMs;
        }

        float get(long now) {
            if (dur <= 0 || now >= start + dur) {
                value = to;
            } else {
                float t = (now - start) / (float) dur;
                value = from + (to - from) * EASE.getInterpolation(t);
            }
            return value;
        }
    }

    /* 每个地址的实时帧流(帧一直在到达,但只有按下才收下一帧并存档) */
    private static final class Live {
        boolean everSeen;
        int frameMa = -1;
        long frameWallTs;
        long lastAt = Long.MIN_VALUE / 4;                     // uptime 时刻,判静默只用这个钟
        String mac = "", name = "";
        int rssi = -127;
        String status;                                        // 实时判定,跳变沿触发提示
        long alertUntil;
        boolean silent = true;
        final Tween bar = new Tween(0);                       // 条宽 .32s ease
    }

    private static final class Row {
        MeterReading rec;
        int anim;                                             // 0 无 1 fresh(.5s) 2 swap(.3s)
        long animStart;
        boolean forming;                                      // 飞行落地前只留骨架
        long formedAt;                                        // 落地后其余列 .26s 淡入
    }

    /* ══════════ 状态 ══════════ */
    private final Paint fill = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint stroke = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint textPaint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Path workPath = new Path();
    private final DashPathEffect limitDash = new DashPathEffect(new float[]{3, 4}, 0);
    private final DashPathEffect cursorDash = new DashPathEffect(new float[]{2, 3}, 0);
    private final Typeface sans = Typeface.create("sans-serif", Typeface.NORMAL);
    private final Typeface sansMedium = Typeface.create("sans-serif-medium", Typeface.NORMAL);
    private final Typeface mono = Typeface.MONOSPACE;
    private final Typeface monoBold = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD);
    private final SimpleDateFormat clockFormat = new SimpleDateFormat("HH:mm:ss", Locale.CHINA);
    private final SimpleDateFormat minuteFormat = new SimpleDateFormat("HH:mm", Locale.CHINA);

    private Listener listener;

    // 转盘
    private float angle = -STEP;                              // 开机停在 01 号表
    private float vel;
    private boolean dragging;
    private boolean hasSnap;
    private float snapTarget;
    private int snapAddr = -1;
    private int sel = 1;
    private float pointerAngle;
    private long lastDialMoveAt;
    private float downX, downY, movePx, velAtDown;
    private int pressSlot = -1;

    // 中心
    private boolean hubPressed;
    private long bumpStart = -1;
    private int pendingAddr = -1;
    private long pendingFrom, pendingUntil;
    private String cachedVersion;
    private int holdAddr = -1;
    private int holdMa;
    private long holdUntil;

    // 判定阈值:界面上点一下就能改,所以不能是常量
    private int lowMa = ReaderPreferences.DEFAULT_LOW_THRESHOLD_MA;
    private int highMa = ReaderPreferences.DEFAULT_HIGH_THRESHOLD_MA;

    // 模式
    private boolean autoMode;
    private float activation;
    private float remain;
    private int intervalSeconds = 120;
    private final Tween modeInd = new Tween(0);               // 滑块 .34s
    private final Tween modeText = new Tween(0);              // 文字色 .26s

    // 记录范围
    private boolean allScope;
    private final Tween chipAll = new Tween(0);               // .22s
    private int panelAddr = 1;
    private int page;

    // 数据
    private final Map<Integer, MeterReading> latest = new HashMap<>();
    private final Set<Integer> registered = new HashSet<>();
    private List<MeterReading> history = new ArrayList<>();
    private long maxSeenId;
    private boolean firstLoad = true;
    private boolean expectCaptureRefresh;
    private final Live[] live = new Live[SLOTS];
    private final long[] alarmMuteUntil = new long[SLOTS];
    private Ringtone alarmRingtone;
    private final List<Row> rows = new ArrayList<>();
    private boolean connectionError;
    private String connectionDetail = "正在连接 HC-42…";

    // 图表
    private float viewU0 = 0f, viewU1 = 1f;
    private long chartFadeStart = -1;
    private final List<float[]> chartPtXY = new ArrayList<>();
    private final List<MeterReading> chartPtRec = new ArrayList<>();
    private long chartDomainSpan;
    private int chartDomainN;
    private final Tween resetAlpha = new Tween(0);            // .2s
    private boolean readoutOn;
    private final Tween readoutAlpha = new Tween(0);          // .14s
    private String readoutText = "";
    private float readoutX, readoutY, cursorX = -1;

    // 图表手势
    private static final int TOUCH_NONE = 0, TOUCH_DIAL = 1, TOUCH_CHART = 2, TOUCH_HUB = 3,
            TOUCH_UI = 4, TOUCH_PAD = 5;
    private int touchMode = TOUCH_NONE;
    private boolean pinching;
    private float panStartX, panU0, panU1, chartMoved, lastSpanX;
    private long lastChartTapAt;
    private float lastChartTapX, lastChartTapY;

    // 清空的两段式确认
    private boolean clearArmed;
    private long clearDeadline;

    // 飞行:测完的数从中心飞到右侧,落地正好成为那一行的数值
    private boolean flyActive;
    private long flyStart;
    private Row flyRow;
    private String flyNum = "";
    private float flyFromRight, flyFromBase, flyToRight, flyToBase;
    private int flyNumFrom, flyNumTo, flyUnitFrom, flyUnitTo;

    // 数字小键盘:阈值和间隔就地改,没有确认键——单位键就是确认键
    private static final int PAD_NONE = 0, PAD_INTERVAL = 1, PAD_LOW = 2, PAD_HIGH = 3;
    private static final int KEY_NONE = -1, KEY_UNIT_A = 12, KEY_UNIT_B = 13, KEY_CLOSE = 14;
    private static final float PAD_W = 468, PAD_H = 300;
    private static final float KEY_W = 72, KEY_H = 46, KEY_GAP = 9;
    private int padMode = PAD_NONE;
    private boolean padOpen;
    private String padInput = "";
    private long padCaretAt;                                  // 每次按键重新计时,刚打完的字后面光标是亮的
    private long padErrorAt;                                  // 越界:红一下 + 抖一下
    private int padPressed = KEY_NONE;
    private final Tween padAlpha = new Tween(0);              // .18s 弹入 / .14s 收起
    private final RectF padCard = new RectF();
    private final RectF[] padKeys = new RectF[12];
    private final RectF padUnitA = new RectF();
    private final RectF padUnitB = new RectF();
    private final RectF padClose = new RectF();

    // 命中区(绘制时更新)
    private final float[] slotOx = new float[SLOTS];
    private final float[] slotOy = new float[SLOTS];
    private final float[] slotScale = new float[SLOTS];
    private final float[] slotAlpha = new float[SLOTS];
    private final int[] slotZ = new int[SLOTS];
    private final RectF modeManualRect = new RectF();
    private final RectF modeAutoRect = new RectF();
    private final RectF intervalChipRect = new RectF();
    private final RectF lowChipRect = new RectF();
    private final RectF highChipRect = new RectF();
    private final RectF chipOneRect = new RectF();
    private final RectF chipAllRect = new RectF();
    private final RectF resetRect = new RectF();
    private final RectF prevRect = new RectF(646, 226.1f, 672, 252.1f);
    private final RectF nextRect = new RectF(720, 226.1f, 746, 252.1f);
    private final RectF clearRect = new RectF();

    private float viewScale = 1f, viewOffX, viewOffY;
    private long lastRenderAt, lastModelAt;

    /* 模型(定时器)与视图(动画帧)分开:读条落库、静默判定、倒计时挂在定时器上 */
    private final Runnable modelLoop = new Runnable() {
        @Override
        public void run() {
            modelTick();
            postDelayed(this, 60);
        }
    };
    private final Runnable renderLoop = new Runnable() {
        @Override
        public void run() {
            long now = SystemClock.uptimeMillis();
            float dt = Math.min(0.05f, (now - lastRenderAt) / 1000f);
            lastRenderAt = now;
            tickDial(dt);
            activation += ((autoMode ? 1f : 0f) - activation) * clamp(dt * 4.2f, 0f, 1f);
            if (flyActive && now >= flyStart + 460) landFlyer(now);
            invalidate();
            postOnAnimation(this);
        }
    };
    /* 标题、条数、曲线、记录说的是同一件事,必须一起刷新;90ms 防抖 */
    private final Runnable panelSwitch = new Runnable() {
        @Override
        public void run() {
            panelAddr = sel;
            page = 0;
            viewU0 = 0f;
            viewU1 = 1f;
            rebuildRows(0, true);
            chartFadeStart = SystemClock.uptimeMillis();
        }
    };

    public MeterDashboardView(Context context) {
        super(context);
        for (int i = 0; i < SLOTS; i++) {
            live[i] = new Live();
        }
        for (int i = 0; i < padKeys.length; i++) {
            padKeys[i] = new RectF();
        }
        layoutPad();
        stroke.setStyle(Paint.Style.STROKE);
        setFocusable(true);
        setContentDescription("无线读表器横屏仪表盘");
    }

    /* ══════════ 对外 API ══════════ */
    public void setListener(Listener listener) {
        this.listener = listener;
    }

    public void setData(List<MeterReading> latestReadings, List<MeterReading> allHistory,
                        Set<Integer> registeredAddresses) {
        latest.clear();
        for (MeterReading reading : latestReadings) {
            latest.put(reading.address, reading);
        }
        registered.clear();
        if (registeredAddresses != null) {
            registered.addAll(registeredAddresses);
        }
        long prevMax = maxSeenId;
        history = new ArrayList<>(allHistory);
        int newTotal = 0, newInScope = 0;
        boolean autoArrived = false;
        for (MeterReading r : history) {
            if (r.id > maxSeenId) maxSeenId = r.id;
            if (r.id > prevMax) {
                newTotal++;
                if (allScope || r.address == panelAddr) newInScope++;
                if ("AUTO".equalsIgnoreCase(r.source)) autoArrived = true;
            }
        }
        if (autoArrived && autoMode) {
            remain = intervalSeconds;                          // 一轮存下来了,重新计时
        }
        if (firstLoad) {
            firstLoad = false;
            rebuildRows(0, false);
        } else if (expectCaptureRefresh) {
            page = 0;                                          // 要飞:先别播「长出来」,等落地
            rebuildRows(0, false);
        } else if (newTotal > 0) {
            page = 0;
            rebuildRows(newInScope, false);
        } else {
            rebuildRows(0, false);
        }
        invalidate();
    }

    public void setConnectionState(String detail, boolean error) {
        connectionDetail = detail;
        connectionError = error;
        invalidate();
    }

    public void setAutoMode(boolean enabled) {
        applyMode(enabled);
    }

    public void setPollingIntervalSeconds(int seconds) {
        intervalSeconds = Math.max(10, seconds);
        if (autoMode) {
            remain = intervalSeconds;
        }
        invalidate();
    }

    /** 调用方保证 0 ≤ 低限 < 超限;界面上的输入在 padConfirm 里已经拦过。 */
    public void setThresholds(int lowThresholdMa, int highThresholdMa) {
        lowMa = lowThresholdMa;
        highMa = highThresholdMa;
        invalidate();
    }

    /**
     * 圆盘条与曲线的满量程。留在超限之上一档,报警的读数不会顶在边框上——
     * 看不出「刚过线」和「过了很多」的区别,就等于没画。
     */
    private int fullMa() {
        return highMa + 200;
    }

    /** 实时帧:进圆盘不进记录 */
    public void onLiveFrame(int address, int currentMa, long wallTs,
                            String mac, String deviceName, int rssi) {
        if (address < 0 || address >= SLOTS) {
            return;
        }
        Live m = live[address];
        m.everSeen = true;
        m.silent = false;
        // currentMa < 0 是保活心跳:它只说明这台表在场,不带读数。写进去会把
        // 上一次真实读数冲掉,圆盘就会在每一拍心跳后闪成空值。
        if (currentMa >= 0) {
            m.frameMa = currentMa;
            m.frameWallTs = wallTs;
            // 每一次读数都判一次:越限就响,不要求状态发生变化。读数是稀疏
            // 的——手动读一次、自动轮次一次、表上按一次键一次——每一次都是
            // 一个独立的、值得被告知的事件。
            String st = classifyStatus(currentMa);
            if (MeterReading.LOW.equals(st) || MeterReading.HIGH.equals(st)) {
                long alarmAt = SystemClock.uptimeMillis();
                m.alertUntil = alarmAt + ALERT_MS;
                alarmFeedback(address, alarmAt);
            }
        }
        m.lastAt = SystemClock.uptimeMillis();
        if (mac != null && !mac.isEmpty()) m.mac = mac;
        if (deviceName != null && !deviceName.isEmpty()) m.name = deviceName;
        m.rssi = rssi;
    }

    /** 问了没人答:做离线指示。不落库,记录里只该有真读数。 */
    public void onOffline(int address) {
        if (address < 0 || address >= SLOTS) {
            return;
        }
        live[address].silent = true;
    }

    /* ══════════ 生命周期 ══════════ */
    @Override
    protected void onAttachedToWindow() {
        super.onAttachedToWindow();
        long now = SystemClock.uptimeMillis();
        lastRenderAt = now;
        lastModelAt = now;
        removeCallbacks(modelLoop);
        removeCallbacks(renderLoop);
        postDelayed(modelLoop, 60);
        postOnAnimation(renderLoop);
    }

    @Override
    protected void onDetachedFromWindow() {
        removeCallbacks(modelLoop);
        removeCallbacks(renderLoop);
        removeCallbacks(panelSwitch);
        super.onDetachedFromWindow();
    }

    /* ══════════ 模型 ══════════ */
    private void modelTick() {
        long now = SystemClock.uptimeMillis();
        float dt = Math.min(0.5f, (now - lastModelAt) / 1000f);
        lastModelAt = now;
        for (int i = 0; i < SLOTS; i++) {
            Live m = live[i];
            // silent 不再按「多久没收到帧」推断:电流表平时就不出声,那样推
            // 出来的结果永远是全部离线。它现在由服务端的应答结果驱动——
            // 收到帧清掉,问了没人答置上。
            if (!present(i)) {
                m.status = null;
                continue;
            }
            // frameMa < 0 是「在场但还没读过数」——心跳只说明它在,不带电流。
            // 这不是一个电流档位:交给 classifyStatus 的话 -1 < 200 会被判成
            // 低限,于是表一出现就先假报一次低限,而真正读到 0.064 A 时状态
            // 已经是低限了,跳变沿不存在,该响的那次反而不响。
            String st;
            if (m.silent) {
                st = MeterReading.OFFLINE;
            } else if (m.frameMa < 0) {
                st = null;
            } else {
                st = classifyStatus(m.frameMa);
            }
            // 首次读到就报,不要求之前有已知状态。旧模型下帧一直在流,状态几拍
            // 就稳定了,漏掉第一次无关紧要;现在读数是问一次来一个,第一次往往
            // 就是唯一一次。
            // 这里只管视觉:状态变了闪一下。声音和震动挂在读数到达上,不挂在
            // 状态跳变上——越限的读数即使和上一次一样也要报,而这个循环每 60 ms
            // 跑一次,拿它当触发源只会变成按帧率响铃。
            if (st != null && !st.equals(m.status)) {
                m.alertUntil = now + ALERT_MS;
            }
            m.status = st;
        }
        if (pendingAddr >= 0 && now >= pendingUntil) {
            commitPending();                                   // 读条一定走完,不中途打断
        }
        if (autoMode) {
            remain = Math.max(0f, remain - dt);
        }
        if (clearArmed && now >= clearDeadline) {
            clearArmed = false;
        }
        if (!padOpen && padMode != PAD_NONE && padAlpha.get(now) <= 0.004f) {
            padMode = PAD_NONE;                                // 收起动画放完才真正关掉
        }
    }

    /* ══════════ 转盘物理(常数与原型一致) ══════════ */
    private void tickDial(float dt) {
        if (!dragging) {
            boolean snapping = hasSnap;
            float target = snapping ? snapTarget : Math.round(angle / STEP) * STEP;
            float grip = snapping ? 1f : clamp(1f - Math.abs(vel) / 260f, 0f, 1f);
            float k = snapping ? 78f : 62f;
            float c = (float) (2 * Math.sqrt(k)) * 1.02f;
            float acc = grip * (-k * (angle - target) - c * vel) - (1 - grip) * 2.1f * vel;
            vel += acc * dt;
            angle += vel * dt;
            if (Math.abs(vel) < 0.4f && Math.abs(angle - target) < 0.02f) {
                angle = target;
                vel = 0;
                hasSnap = false;
                snapAddr = -1;
            }
        }
        int s = selectedFromAngle();
        if (s != sel) {
            sel = s;
            onSelectionChanged();
            if (dragging) {
                vibrate();
            }
        }
    }

    private int selectedFromAngle() {
        return ((Math.round(-angle / STEP) % SLOTS) + SLOTS) % SLOTS;
    }

    private void snapTo(int addr) {
        float base = -addr * STEP;
        snapTarget = base + Math.round((angle - base) / 360f) * 360f;   // 走最近的一圈
        hasSnap = true;
        snapAddr = addr;
    }

    private void onSelectionChanged() {
        removeCallbacks(panelSwitch);
        postDelayed(panelSwitch, 90);
    }

    /* 中心此刻显示的是哪台表:转盘还在滚过去的途中,中心显示的已经是目标地址 */
    private int focusAddr(long now) {
        if (pendingAddr >= 0) return pendingAddr;
        if (holdAddr >= 0 && now < holdUntil) return holdAddr;
        if (hasSnap && snapAddr >= 0) return snapAddr;
        return sel;
    }

    /* ══════════ 点一下:发请求,等应答,存这一次 ══════════ */
    private void capture(int addr) {
        if (pendingAddr >= 0) {
            return;
        }
        long now = SystemClock.uptimeMillis();
        pendingAddr = addr;
        pendingFrom = now;
        pendingUntil = now + STORE_MS;
        if (listener != null) {
            listener.onReadRequest(addr);                       // 先问,读条走完时才有得存
        }
    }

    private void commitPending() {
        int addr = pendingAddr;
        pendingAddr = -1;
        Live m = live[addr];
        // 还要求真的收到过读数:一台只在心跳、这次读取又没答上来的表,
        // 在场但没有可存的数,不能把上一次的值当成这一次的结果。
        boolean hasFrame = present(addr) && m.everSeen && !m.silent && m.frameMa >= 0;
        int ma = hasFrame ? m.frameMa : -1;
        String status = hasFrame ? classifyStatus(ma) : MeterReading.OFFLINE;
        boolean willFly = allScope || panelAddr == addr;       // 出现在当前筛选里,才有得飞
        String mac = "", name = "";
        int rssi = -127;
        if (m.everSeen) {
            mac = m.mac;
            name = m.name;
            rssi = m.rssi;
        } else {
            MeterReading last = latest.get(addr);
            if (last != null) {
                mac = last.mac;
                name = last.deviceName;
                rssi = last.rssi;
            }
        }
        if (!hasFrame) {
            // 这次没读到数就什么都不存。离线不是读数——记录里只该有真读到的
            // 值,否则 30 条的空间会被「没读到」填满,历史和趋势也跟着变形。
            // 但要说出来:静悄悄什么都不发生,和「存了一条」在界面上分不出。
            if (listener != null) {
                listener.onReadFailed(addr);
            }
            return;
        }
        if (listener != null) {
            expectCaptureRefresh = true;
            listener.onCaptureRecord(addr, ma, status, mac, name, rssi);
            expectCaptureRefresh = false;
        }
        if (willFly) {
            flyToRecord(addr, ma, status);
        }
    }

    /* 因果:起点逐项等于中心的数值行,终点逐项等于行里的数值单元格,两端没有接缝 */
    private void flyToRecord(int addr, int ma, String status) {
        long now = SystemClock.uptimeMillis();
        // 飞行途中把被抓走的这个值冻住,否则下一帧一到起点就对不上了
        holdAddr = addr;
        holdMa = ma;
        holdUntil = now + 480;
        Row row = rows.isEmpty() ? null : rows.get(0);
        if (row == null || row.rec.address != addr || page != 0) {
            return;
        }
        flyNum = ma < 0 ? BLANK : fmtA(ma);
        float numW = measure(flyNum, 40, sansMedium, 0);
        float unitW = measure("A", 15, sansMedium, 0);
        float total = numW + 4 + unitW;
        flyFromRight = DIAL_X + DIAL_C + total / 2f;
        flyFromBase = DIAL_Y + HUB_VALUE_BASE;
        flyToRight = REC_V_RIGHT;
        flyToBase = rowBaseline(0, 13);
        flyNumFrom = ma < 0 ? C_OFF : warmColor(ma);
        flyNumTo = statusColor(status);
        flyUnitFrom = INK3;
        flyUnitTo = statusColor(status);
        row.forming = true;                                    // 目标行先只留骨架
        row.formedAt = 0;
        flyRow = row;
        flyActive = true;
        flyStart = now;
    }

    private void landFlyer(long now) {
        flyActive = false;
        if (flyRow != null) {
            flyRow.forming = false;
            flyRow.formedAt = now;                             // 其余列淡入,数值单元格接上
            flyRow = null;
        }
    }

    /* ══════════ 记录行 ══════════ */
    private List<MeterReading> scopedRecords() {
        if (allScope) {
            return history;
        }
        List<MeterReading> result = new ArrayList<>();
        for (MeterReading r : history) {
            if (r.address == panelAddr) {
                result.add(r);
            }
        }
        return result;
    }

    private void rebuildRows(int freshCount, boolean swap) {
        List<MeterReading> all = scopedRecords();
        int pages = Math.max(1, (all.size() + PAGE_SIZE - 1) / PAGE_SIZE);
        page = Math.max(0, Math.min(page, pages - 1));
        rows.clear();
        int from = page * PAGE_SIZE;
        int to = Math.min(all.size(), from + PAGE_SIZE);
        long now = SystemClock.uptimeMillis();
        for (int i = from; i < to; i++) {
            Row row = new Row();
            row.rec = all.get(i);
            int idx = i - from;
            if (freshCount > 0 && page == 0 && idx < freshCount) {
                row.anim = 1;
                row.animStart = now + idx * 40L;
            } else if (swap) {
                row.anim = 2;
                row.animStart = now + idx * 22L;
            }
            rows.add(row);
        }
    }

    /* ══════════ 输入 ══════════ */
    @Override
    public boolean onTouchEvent(MotionEvent event) {
        float dx = (event.getX() - viewOffX) / viewScale;
        float dy = (event.getY() - viewOffY) / viewScale;
        long now = SystemClock.uptimeMillis();
        switch (event.getActionMasked()) {
            case MotionEvent.ACTION_DOWN:
                downX = dx;
                downY = dy;
                movePx = 0;
                pinching = false;
                if (padMode != PAD_NONE) {
                    // 小键盘是模态的:开着的时候底下的转盘、曲线、记录都不接手势。
                    // 收起动画还在放的时候只吞手势不认键——那 140 ms 里再按一次
                    // 单位键会把刚存好的值又存一遍。
                    touchMode = TOUCH_PAD;
                    padPressed = padOpen ? padKeyAt(dx, dy) : KEY_NONE;
                    return true;
                }
                if (dist(dx, dy, DIAL_X + DIAL_C, DIAL_Y + DIAL_C) <= HUB_R) {
                    touchMode = TOUCH_HUB;
                    hubPressed = true;                          // :active 按压反馈
                } else if (DIAL_RECT.contains(dx, dy)) {
                    touchMode = TOUCH_DIAL;
                    pressSlot = hitSlot(dx, dy);
                    dragging = true;
                    hasSnap = false;
                    snapAddr = -1;
                    pointerAngle = angleAt(dx, dy);
                    lastDialMoveAt = now;
                    velAtDown = Math.abs(vel);                  // 按住正在滑行的盘 = 想让它停
                    vel = 0;
                } else if (resetAlpha.to > 0 && resetRect.contains(dx, dy)) {
                    touchMode = TOUCH_UI;
                } else if (lowChipRect.contains(dx, dy) || highChipRect.contains(dx, dy)) {
                    touchMode = TOUCH_UI;                       // 阈值牌坐在曲线里,得抢在平移前面
                } else if (CHART_WRAP.contains(dx, dy) && chartDomainSpan > 0) {
                    touchMode = TOUCH_CHART;
                    chartMoved = 0;
                    panStartX = dx;
                    panU0 = viewU0;
                    panU1 = viewU1;
                } else {
                    touchMode = TOUCH_UI;
                }
                return true;
            case MotionEvent.ACTION_POINTER_DOWN:
                if (touchMode == TOUCH_CHART && event.getPointerCount() >= 2) {
                    pinching = true;
                    lastSpanX = pointerSpanX(event);
                }
                return true;
            case MotionEvent.ACTION_MOVE:
                movePx = Math.max(movePx, (float) Math.hypot(dx - downX, dy - downY));
                if (touchMode == TOUCH_PAD) {
                    if (padPressed != KEY_NONE && padKeyAt(dx, dy) != padPressed) {
                        padPressed = KEY_NONE;                  // 手指滑出这个键就撤销按压
                    }
                } else if (touchMode == TOUCH_DIAL) {
                    float a = angleAt(dx, dy);
                    float d = norm180(a - pointerAngle);
                    pointerAngle = a;
                    angle += d;
                    float mdt = Math.max(8, now - lastDialMoveAt) / 1000f;
                    lastDialMoveAt = now;
                    vel = lerp(vel, d / mdt, 0.45f);
                } else if (touchMode == TOUCH_CHART) {
                    if (pinching && event.getPointerCount() >= 2) {
                        float span = pointerSpanX(event);
                        if (lastSpanX > 8 && span > 8) {
                            zoomSpanMul(lastSpanX / span, dx);
                        }
                        lastSpanX = span;
                    } else {
                        chartMoved = Math.max(chartMoved, Math.abs(dx - panStartX));
                        float du = (dx - panStartX) / PLOT_W * (panU1 - panU0);
                        float span = panU1 - panU0;
                        float u0 = clamp(panU0 - du, 0f, 1f - span);
                        viewU0 = u0;
                        viewU1 = u0 + span;
                    }
                }
                return true;
            case MotionEvent.ACTION_UP:
                if (touchMode == TOUCH_PAD) {
                    int hit = padKeyAt(dx, dy);
                    if (hit != KEY_NONE && hit == padPressed) {
                        padTap(hit);
                    } else if (hit == KEY_NONE && movePx < 8 && !padCard.contains(dx, dy)) {
                        closePad();                             // 点卡片外 = 不改了
                    }
                    padPressed = KEY_NONE;
                } else if (touchMode == TOUCH_DIAL) {
                    dragging = false;
                    // 点地址只是选中并滚过去,不触发存档
                    if (pressSlot >= 0 && movePx < 8 && velAtDown < 30) {
                        snapTo(pressSlot);
                    }
                    pressSlot = -1;
                } else if (touchMode == TOUCH_HUB) {
                    hubPressed = false;
                    if (movePx < 8 && dist(dx, dy, DIAL_X + DIAL_C, DIAL_Y + DIAL_C) <= HUB_R) {
                        hubClick();
                    }
                } else if (touchMode == TOUCH_CHART) {
                    if (!pinching && chartMoved < 5) {
                        chartTap(dx, dy, now);
                    }
                } else if (touchMode == TOUCH_UI) {
                    if (movePx < 8) {
                        uiTap(dx, dy);
                    }
                }
                touchMode = TOUCH_NONE;
                pinching = false;
                performClick();
                return true;
            case MotionEvent.ACTION_CANCEL:
                dragging = false;
                hubPressed = false;
                pressSlot = -1;
                padPressed = KEY_NONE;
                touchMode = TOUCH_NONE;
                pinching = false;
                return true;
            default:
                return true;
        }
    }

    @Override
    public boolean performClick() {
        super.performClick();
        return true;
    }

    @Override
    public boolean onGenericMotionEvent(MotionEvent event) {
        if (event.getActionMasked() == MotionEvent.ACTION_SCROLL) {
            float dx = (event.getX() - viewOffX) / viewScale;
            float dy = (event.getY() - viewOffY) / viewScale;
            float v = event.getAxisValue(MotionEvent.AXIS_VSCROLL);
            if (v != 0 && DIAL_RECT.contains(dx, dy)) {
                hasSnap = false;
                snapAddr = -1;
                vel += (v < 0 ? -1 : 1) * 220f;
                return true;
            }
            if (v != 0 && CHART_WRAP.contains(dx, dy) && chartDomainSpan > 0) {
                zoomSpanMul(v < 0 ? 1.18f : 1 / 1.18f, dx);
                return true;
            }
        }
        return super.onGenericMotionEvent(event);
    }

    @Override
    public boolean onHoverEvent(MotionEvent event) {
        int action = event.getActionMasked();
        if (action == MotionEvent.ACTION_HOVER_MOVE) {
            float dx = (event.getX() - viewOffX) / viewScale;
            float dy = (event.getY() - viewOffY) / viewScale;
            if (CHART_WRAP.contains(dx, dy)) {
                int idx = nearestChartPoint(dx, dy);
                if (idx >= 0) {
                    showReadout(idx);
                } else {
                    hideReadout();
                }
            } else {
                hideReadout();
            }
        } else if (action == MotionEvent.ACTION_HOVER_EXIT) {
            hideReadout();
        }
        return super.onHoverEvent(event);
    }

    private void hubClick() {
        if (pendingAddr >= 0) {
            bumpStart = SystemClock.uptimeMillis();            // 已经在读:读条继续,但这一下也得有反应
            return;
        }
        capture(focusAddr(SystemClock.uptimeMillis()));
    }

    private void uiTap(float x, float y) {
        if (modeManualRect.contains(x, y)) {
            if (autoMode) {
                applyMode(false);
                if (listener != null) listener.onAutoModeChanged(false);
            }
            return;
        }
        if (modeAutoRect.contains(x, y)) {
            if (!autoMode) {
                applyMode(true);
                if (listener != null) listener.onAutoModeChanged(true);
            }
            return;
        }
        if (intervalChipRect.contains(x, y)) {
            openPad(PAD_INTERVAL);
            return;
        }
        if (highChipRect.contains(x, y)) {
            openPad(PAD_HIGH);
            return;
        }
        if (lowChipRect.contains(x, y)) {
            openPad(PAD_LOW);
            return;
        }
        if (chipOneRect.contains(x, y)) {
            setScope(false);
            return;
        }
        if (chipAllRect.contains(x, y)) {
            setScope(true);
            return;
        }
        if (resetAlpha.to > 0 && resetRect.contains(x, y)) {
            viewU0 = 0f;
            viewU1 = 1f;
            return;
        }
        List<MeterReading> all = scopedRecords();
        int pages = all.isEmpty() ? 0 : (all.size() + PAGE_SIZE - 1) / PAGE_SIZE;
        if (prevRect.contains(x, y)) {
            if (page > 0) {
                page--;
                rebuildRows(0, true);
            }
            return;
        }
        if (nextRect.contains(x, y)) {
            if (page + 1 < pages) {
                page++;
                rebuildRows(0, true);
            }
            return;
        }
        if (clearRect.contains(x, y)) {
            if (clearArmed) {
                clearArmed = false;
                if (listener != null) listener.onClearHistory();
            } else {
                clearArmed = true;
                clearDeadline = SystemClock.uptimeMillis() + 3000;
            }
            return;
        }
        if (LOG_RECT.contains(x, y) && !rows.isEmpty()) {
            int index = (int) ((y - LOG_RECT.top) / ROW_H);
            if (index >= 0 && index < rows.size()) {
                snapTo(rows.get(index).rec.address);
            }
        }
    }

    private void setScope(boolean all) {
        if (allScope == all) {
            return;
        }
        allScope = all;
        chipAll.retarget(all ? 1f : 0f, SystemClock.uptimeMillis(), 220);
        page = 0;
        viewU0 = 0f;
        viewU1 = 1f;
        rebuildRows(0, true);
        chartFadeStart = SystemClock.uptimeMillis();
    }

    private void applyMode(boolean enabled) {
        if (autoMode == enabled) {
            return;
        }
        autoMode = enabled;
        if (enabled) {
            remain = intervalSeconds;
        }
        long now = SystemClock.uptimeMillis();
        modeInd.retarget(enabled ? 1f : 0f, now, 340);
        modeText.retarget(enabled ? 1f : 0f, now, 260);
    }

    /* ══════════ 数字小键盘:阈值和间隔就地改 ══════════ */
    private void openPad(int mode) {
        long now = SystemClock.uptimeMillis();
        padMode = mode;
        padOpen = true;
        padInput = "";
        padErrorAt = 0;
        padPressed = KEY_NONE;
        padCaretAt = now;
        padAlpha.retarget(1f, now, 180);
    }

    private void closePad() {
        padOpen = false;
        padAlpha.retarget(0f, SystemClock.uptimeMillis(), 140);
    }

    /** 键位与内容无关,尺寸也不随输入变,所以构造时算一次就够。 */
    private void layoutPad() {
        float left = (DESIGN_W - PAD_W) / 2f;
        float top = (DESIGN_H - PAD_H) / 2f;
        padCard.set(left, top, left + PAD_W, top + PAD_H);
        float keysLeft = padCard.right - 24 - (3 * KEY_W + 2 * KEY_GAP);
        float keysTop = top + (PAD_H - (4 * KEY_H + 3 * KEY_GAP)) / 2f;
        for (int i = 0; i < padKeys.length; i++) {
            float kx = keysLeft + (i % 3) * (KEY_W + KEY_GAP);
            float ky = keysTop + (i / 3) * (KEY_H + KEY_GAP);
            padKeys[i].set(kx, ky, kx + KEY_W, ky + KEY_H);
        }
        float paneLeft = left + 26;
        float paneRight = keysLeft - 24;
        float unitW = (paneRight - paneLeft - 8) / 2f;
        float unitBottom = keysTop + 4 * KEY_H + 3 * KEY_GAP;   // 与键盘底边齐平
        padUnitA.set(paneLeft, unitBottom - 44, paneLeft + unitW, unitBottom);
        padUnitB.set(paneRight - unitW, unitBottom - 44, paneRight, unitBottom);
        padClose.set(padCard.right - 40, top + 12, padCard.right - 14, top + 38);
    }

    private int padKeyAt(float x, float y) {
        for (int i = 0; i < padKeys.length; i++) {
            if (padKeys[i].contains(x, y)) {
                return i;
            }
        }
        if (padUnitA.contains(x, y)) return KEY_UNIT_A;
        if (padUnitB.contains(x, y)) return KEY_UNIT_B;
        if (padClose.contains(x, y)) return KEY_CLOSE;
        return KEY_NONE;
    }

    private void padTap(int key) {
        vibrate();
        padCaretAt = SystemClock.uptimeMillis();
        if (key == KEY_CLOSE) {
            closePad();
        } else if (key == KEY_UNIT_A || key == KEY_UNIT_B) {
            padConfirm(key == KEY_UNIT_B);
        } else if (key == 11) {
            if (!padInput.isEmpty()) {
                padInput = padInput.substring(0, padInput.length() - 1);
            }
        } else {
            padType(key == 9 ? '.' : key == 10 ? '0' : (char) ('1' + key));
        }
    }

    private void padType(char c) {
        if (c == '.') {
            if (padInput.contains(".")) {
                return;
            }
            padInput = padInput.isEmpty() ? "0." : padInput + ".";
            return;
        }
        if (padInput.equals("0")) {
            padInput = "";                                     // 前导零直接被顶掉
        }
        int dot = padInput.indexOf('.');
        if (dot >= 0 && padInput.length() - dot > 3) {
            return;                                            // 最多三位小数,再多也显示不出来
        }
        if (padInput.length() >= 6) {
            return;
        }
        padInput += c;
    }

    /** 输入框里的数。空的、只有小数点的、读不成数的都算没有输入。 */
    private float padValue() {
        try {
            float v = Float.parseFloat(padInput);
            return v < 0 ? Float.NaN : v;
        } catch (NumberFormatException e) {
            return Float.NaN;
        }
    }

    /** 单位键就是确认键:按的是哪个单位,输入的数就按哪个单位读进来。 */
    private void padConfirm(boolean secondUnit) {
        float v = padValue();
        if (Float.isNaN(v)) {
            padFail();
            return;
        }
        if (padMode == PAD_INTERVAL) {
            int seconds = Math.round(secondUnit ? v * 60f : v);
            if (seconds < ReaderPreferences.MIN_POLLING_INTERVAL_SECONDS
                    || seconds > ReaderPreferences.MAX_POLLING_INTERVAL_SECONDS) {
                padFail();
                return;
            }
            setPollingIntervalSeconds(seconds);
            if (listener != null) {
                listener.onIntervalChanged(seconds);
            }
        } else {
            int ma = Math.round(secondUnit ? v * 1000f : v);
            int low = padMode == PAD_LOW ? ma : lowMa;
            int high = padMode == PAD_HIGH ? ma : highMa;
            if (low < 0 || high > ReaderPreferences.MAX_THRESHOLD_MA || low >= high) {
                padFail();
                return;
            }
            setThresholds(low, high);
            if (listener != null) {
                listener.onThresholdsChanged(low, high);
            }
        }
        closePad();
    }

    /* 越界不悄悄夹到边界上:夹过的值和输入的不是同一个意思,而这两个数决定
       什么时候响铃。红一下、抖一下,让人自己改。 */
    private void padFail() {
        padErrorAt = SystemClock.uptimeMillis();
        vibrateEffect(VibrationEffect.createOneShot(28, VibrationEffect.DEFAULT_AMPLITUDE));
    }

    private String padTitle() {
        if (padMode == PAD_INTERVAL) return "自动采集间隔";
        return padMode == PAD_LOW ? "低限阈值" : "超限阈值";
    }

    private String padCurrent() {
        if (padMode == PAD_INTERVAL) return "现在 " + intervalChipText();
        return "现在 " + fmtA(padMode == PAD_LOW ? lowMa : highMa) + " A";
    }

    private String padRange() {
        if (padMode == PAD_INTERVAL) {
            // 两端单位不同,各自带上单位写出来,别折算成同一个单位的大数
            return ReaderPreferences.formatIntervalSeconds(
                    ReaderPreferences.MIN_POLLING_INTERVAL_SECONDS)
                    + " – " + ReaderPreferences.formatIntervalSeconds(
                    ReaderPreferences.MAX_POLLING_INTERVAL_SECONDS);
        }
        if (padMode == PAD_LOW) {
            return "0 – " + fmtA(highMa - 1) + " A";
        }
        return fmtA(lowMa + 1) + " – " + fmtA(ReaderPreferences.MAX_THRESHOLD_MA) + " A";
    }

    private String padUnitLabel(boolean secondUnit) {
        if (padMode == PAD_INTERVAL) return secondUnit ? "分钟" : "秒";
        return secondUnit ? "A" : "mA";
    }

    private void chartTap(float x, float y, long now) {
        int idx = nearestChartPoint(x, y);
        if (idx >= 0) {
            snapTo(chartPtRec.get(idx).address);               // 点曲线上的点把转盘转到那台表
        }
        boolean dbl = now - lastChartTapAt < 300
                && Math.hypot(x - lastChartTapX, y - lastChartTapY) < 24;
        if (dbl) {
            viewU0 = 0f;
            viewU1 = 1f;
            lastChartTapAt = 0;
        } else {
            lastChartTapAt = now;
            lastChartTapX = x;
            lastChartTapY = y;
        }
    }

    private void zoomSpanMul(float mul, float focusDesignX) {
        float vx = (focusDesignX - PLOT_X) / PLOT_W * CHW;
        float oldSpan = viewU1 - viewU0;
        float uAt = viewU0 + clamp((vx - CH_L) / (CH_R - CH_L), 0f, 1f) * oldSpan;
        float span = clamp(oldSpan * mul, 0.02f, 1f);
        float u0 = clamp(uAt - (uAt - viewU0) * (span / oldSpan), 0f, 1f - span);
        viewU0 = u0;
        viewU1 = u0 + span;
    }

    private int nearestChartPoint(float designX, float designY) {
        float vx = (designX - PLOT_X) / PLOT_W * CHW;
        float vy = (designY - PLOT_Y) / PLOT_H * CHH;
        int best = -1;
        float bd = Float.MAX_VALUE;
        for (int i = 0; i < chartPtXY.size(); i++) {
            float[] p = chartPtXY.get(i);
            if (p[0] < CH_L - 4 || p[0] > CH_R + 4) continue;
            float dd = (p[0] - vx) * (p[0] - vx) + (p[1] - vy) * (p[1] - vy) * 0.55f;
            if (dd < bd) {
                bd = dd;
                best = i;
            }
        }
        return bd < 1600 ? best : -1;
    }

    private void showReadout(int idx) {
        float[] p = chartPtXY.get(idx);
        MeterReading r = chartPtRec.get(idx);
        readoutOn = true;
        readoutAlpha.retarget(1f, SystemClock.uptimeMillis(), 140);
        readoutText = clockFormat.format(new Date(r.timestamp)) + " · 地址 " + hex1(r.address)
                + " · " + fmtA(r.currentMa) + " A · " + statusLabel(statusOf(r));
        readoutX = PLOT_X + p[0] * (PLOT_W / CHW);
        readoutY = PLOT_Y + p[1] * (PLOT_H / CHH) - 8;
        cursorX = p[0];
    }

    private void hideReadout() {
        readoutOn = false;
        readoutAlpha.retarget(0f, SystemClock.uptimeMillis(), 140);
    }

    private int hitSlot(float x, float y) {
        int best = -1, bestZ = Integer.MIN_VALUE;
        for (int i = 0; i < SLOTS; i++) {
            if (Math.abs(x - slotOx[i]) <= SLOT_HALF_W * slotScale[i]
                    && Math.abs(y - slotOy[i]) <= SLOT_HALF_H * slotScale[i]
                    && slotZ[i] > bestZ) {
                bestZ = slotZ[i];
                best = i;
            }
        }
        return best;
    }

    private float pointerSpanX(MotionEvent event) {
        return Math.abs(event.getX(1) - event.getX(0)) / viewScale;
    }

    private float angleAt(float x, float y) {
        return (float) Math.toDegrees(Math.atan2(y - (DIAL_Y + DIAL_C), x - (DIAL_X + DIAL_C)));
    }

    private void vibrate() {
        vibrateEffect(VibrationEffect.createOneShot(8, VibrationEffect.DEFAULT_AMPLITUDE));
    }

    private void vibrateEffect(VibrationEffect effect) {
        Vibrator v = (Vibrator) getContext().getSystemService(Context.VIBRATOR_SERVICE);
        if (v == null) {
            return;
        }
        try {
            v.vibrate(effect);
        } catch (Exception ignored) {
            // 没有振动权限或马达,静默跳过
        }
    }

    /** 一次越限读数:震动两下 + 响一声系统铃(走闹钟音量,静音通知也听得见)。 */
    private void alarmFeedback(int addr, long now) {
        if (now < alarmMuteUntil[addr]) {
            return;                                            // 同一次读数被重复处理时去个重
        }
        alarmMuteUntil[addr] = now + ALARM_DEDUP_MS;
        vibrateEffect(VibrationEffect.createWaveform(new long[]{0, 120, 80, 120}, -1));
        try {
            if (alarmRingtone == null) {
                Uri uri = RingtoneManager.getDefaultUri(RingtoneManager.TYPE_NOTIFICATION);
                alarmRingtone = RingtoneManager.getRingtone(getContext(), uri);
                if (alarmRingtone != null) {
                    alarmRingtone.setAudioAttributes(new AudioAttributes.Builder()
                            .setUsage(AudioAttributes.USAGE_ALARM)
                            .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                            .build());
                }
            }
            if (alarmRingtone != null) {
                // 先停再播。Ringtone.play() 在正在播放时不会重头开始,而是
                // 直接被吞掉——默认通知音有一两秒,读数可能来得更快,于是
                // 「响一次吞一次」。停一下才能保证每一次越限都听得见。
                alarmRingtone.stop();
                alarmRingtone.play();
            }
        } catch (Exception ignored) {
            // 没有可用铃声也不影响视觉报警
        }
    }

    /* ══════════ 绘制 ══════════ */
    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        viewScale = Math.min(getWidth() / DESIGN_W, getHeight() / DESIGN_H);
        viewOffX = (getWidth() - DESIGN_W * viewScale) / 2f;
        viewOffY = (getHeight() - DESIGN_H * viewScale) / 2f;
        canvas.drawColor(PAGE);
        canvas.save();
        canvas.translate(viewOffX, viewOffY);
        canvas.scale(viewScale, viewScale);
        long now = SystemClock.uptimeMillis();
        drawDevice(canvas);
        drawStatusBar(canvas);
        drawDial(canvas, now);
        drawFoot(canvas, now);
        drawRightHead(canvas, now);
        drawChart(canvas, now);
        drawLogHead(canvas);
        drawLog(canvas, now);
        drawReadout(canvas, now);
        drawFlyer(canvas, now);
        drawKeypad(canvas, now);
        canvas.restore();
    }

    private void drawDevice(Canvas canvas) {
        fill.setColor(FRAME);
        fill.setShadowLayer(30, 0, 16, 0x4D0A101A);
        canvas.drawRoundRect(0, 0, DESIGN_W, DESIGN_H, 30, 30, fill);
        fill.clearShadowLayer();
        stroke.setColor(LINE2);
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(0.5f, 0.5f, DESIGN_W - 0.5f, DESIGN_H - 0.5f, 30, 30, stroke);
    }

    private void drawStatusBar(Canvas canvas) {
        int known = 0, alive = 0;
        for (int i = 0; i < SLOTS; i++) {
            if (present(i)) {
                known++;
                if (!live[i].silent) alive++;
            }
        }
        int dot = connectionError ? C_HIGH : (alive > 0 ? C_OK : C_HIGH);
        fill.setColor(dot);
        canvas.drawCircle(29, SB_CY, 3, fill);
        // 版本号常驻状态栏。迭代快的时候「手机上装的到底是哪个包」是个真问题:
        // 报警行为改过好几轮,而所有 APK 长得一模一样,凭现象反推装了哪版会
        // 把人引到错误的方向去。
        String link = (known > 0 ? "HC-42 透传 · 已知 " + known + " 台" : connectionDetail)
                + "  v" + appVersion();
        text(canvas, link, 40, SB_CY + 4.1f, 11.5f, INK2, Paint.Align.LEFT, sans, 0, 1);
        text(canvas, minuteFormat.format(new Date()), 924, SB_CY + 4.1f, 11.5f, INK2,
                Paint.Align.RIGHT, sans, 0, 1);
        stroke.setColor(INK2);
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(932, 13.1f, 951, 22.1f, 3, 3, stroke);
        fill.setColor(INK2);
        canvas.drawRoundRect(934.5f, 15.6f, 944.9f, 19.6f, 1, 1, fill);
        canvas.drawRoundRect(951.5f, 15.6f, 953.5f, 19.6f, 0, 2, fill);
    }

    /* ── 转盘 ── */
    private void drawDial(Canvas canvas, long now) {
        float cx = DIAL_X + DIAL_C, cy = DIAL_Y + DIAL_C;
        stroke.setColor(LINE);
        stroke.setStrokeWidth(1);
        canvas.drawCircle(cx, cy, RING_R, stroke);

        boolean reading = pendingAddr >= 0;
        int focus = focusAddr(now);
        Live fm = live[focus];

        // 倒计时环轨道
        float trackAlpha = reading ? 0.9f : activation * 0.9f;
        if (trackAlpha > 0.003f) {
            stroke.setStrokeWidth(8);
            stroke.setStrokeCap(Paint.Cap.BUTT);
            stroke.setColor(mulAlpha(SUNKEN, trackAlpha));
            canvas.drawCircle(cx, cy, COUNT_R, stroke);
        }
        // 倒计时环:从满环顺时针走完,终点固定在 12 点
        float arcAlpha = reading ? 0f : activation;
        if (arcAlpha > 0.003f) {
            float p = autoMode && intervalSeconds > 0 ? remain / intervalSeconds : 1f;
            float sweep = clamp(p, 0f, 0.9999f) * 360f;
            int col = fm.status != null ? statusColor(fm.status) : ACCENT;
            stroke.setColor(mulAlpha(col, arcAlpha));
            stroke.setStrokeWidth(8);
            stroke.setStrokeCap(Paint.Cap.BUTT);
            canvas.drawArc(cx - COUNT_R, cy - COUNT_R, cx + COUNT_R, cy + COUNT_R,
                    -90 + (360 - sweep), sweep, false, stroke);
        }
        // 读条:从 12 点起顺时针填充,一定走完整条
        if (reading) {
            float p = Math.max(0.012f, clamp((now - pendingFrom) / (float) STORE_MS, 0f, 1f));
            stroke.setColor(ACCENT);
            stroke.setStrokeWidth(8);
            stroke.setStrokeCap(Paint.Cap.ROUND);
            canvas.drawArc(cx - COUNT_R, cy - COUNT_R, cx + COUNT_R, cy + COUNT_R,
                    -90, p * 360, false, stroke);
        }
        stroke.setStrokeCap(Paint.Cap.ROUND);
        stroke.setColor(INK3);
        stroke.setStrokeWidth(2);
        canvas.drawLine(cx, DIAL_Y + 1, cx, DIAL_Y + 11, stroke);
        stroke.setStrokeCap(Paint.Cap.BUTT);

        layoutSlots(now);
        Integer[] order = new Integer[SLOTS];
        for (int i = 0; i < SLOTS; i++) order[i] = i;
        java.util.Arrays.sort(order, (a, b) -> slotZ[a] - slotZ[b]);
        for (int idx : order) {
            drawSlot(canvas, idx, now);
        }
        drawHub(canvas, now, focus, fm, reading);
    }

    private void layoutSlots(long now) {
        for (int i = 0; i < SLOTS; i++) {
            float a = i * STEP + angle;
            float off = Math.abs(norm180(a)) / 180f;
            Live m = live[i];
            boolean alarm = MeterReading.LOW.equals(m.status) || MeterReading.HIGH.equals(m.status);
            boolean alerting = now < m.alertUntil;             // 只在刚变化时提示
            float beat = alerting ? 1f + 0.045f * (float) Math.sin(now / 130.0) : 1f;
            slotScale[i] = lerp(1.14f, 0.82f, off) * beat;
            // 报警状态保持清晰可读(不淡出),但不会一直动
            slotAlpha[i] = (alarm || alerting)
                    ? Math.max(0.94f, lerp(1f, 0.55f, off))
                    : lerp(1f, 0.55f, off);
            slotZ[i] = 100 - Math.round(off * 100) + (alarm ? 40 : 0);
            slotOx[i] = DIAL_X + DIAL_C + SLOT_R * (float) Math.sin(Math.toRadians(a));
            slotOy[i] = DIAL_Y + DIAL_C - 18 + SLOT_HALF_H
                    - SLOT_R * (float) Math.cos(Math.toRadians(a));
        }
    }

    private void drawSlot(Canvas canvas, int i, long now) {
        Live m = live[i];
        float alpha = slotAlpha[i];
        boolean isPresent = present(i);
        boolean liveNow = isPresent && !m.silent;
        canvas.save();
        canvas.translate(slotOx[i], slotOy[i]);
        canvas.scale(slotScale[i], slotScale[i]);
        if (i == sel) {
            fill.setColor(mulAlpha(SUNKEN, alpha));
            canvas.drawRoundRect(-SLOT_HALF_W, -SLOT_HALF_H, SLOT_HALF_W, SLOT_HALF_H, 10, 10, fill);
            stroke.setColor(mulAlpha(LINE2, alpha));
            stroke.setStrokeWidth(1);
            canvas.drawRoundRect(-SLOT_HALF_W, -SLOT_HALF_H, SLOT_HALF_W, SLOT_HALF_H, 10, 10, stroke);
        }
        // 编号用离散判定色;不在线灰,空地址浅
        int color = liveNow ? statusColor(m.status) : (isPresent ? SLOT_OFF : INK3);
        String id = hex1(i);
        float idW = measure(id, 15, monoBold, 0.04f);
        float contentW = 6 + 5 + idW;
        float left = -contentW / 2f;
        float headCy = -SLOT_HALF_H + 12f;
        // 在线点:静态红绿,状态没变就不该动;空地址是空心圈
        if (!isPresent) {
            stroke.setColor(mulAlpha(INK3, alpha));
            stroke.setStrokeWidth(1);
            canvas.drawCircle(left + 3, headCy, 3, stroke);
        } else {
            fill.setColor(mulAlpha(m.silent ? C_HIGH : C_OK, alpha));
            canvas.drawCircle(left + 3, headCy, 3, fill);
        }
        text(canvas, id, left + 6 + 5, headCy + 5.4f, 15, color, Paint.Align.LEFT, monoBold,
                0.04f, alpha);
        if (isPresent) {
            // 圆盘是实时的:条子跟着帧走(宽度 .32s 过渡),条用连续变暖色
            m.bar.retarget(liveNow ? clamp(m.frameMa / (float) fullMa(), 0f, 1f) : 0f, now, 320);
            float frac = m.bar.get(now);
            fill.setColor(mulAlpha(LINE, alpha));
            canvas.drawRoundRect(-25, 8f, 25, 11.5f, 2, 2, fill);
            if (frac > 0.001f) {
                fill.setColor(mulAlpha(warmColor(m.frameMa), alpha));
                canvas.drawRoundRect(-25, 8f, -25 + 50 * frac, 11.5f, 2, 2, fill);
            }
        }
        canvas.restore();
    }

    private void drawHub(Canvas canvas, long now, int focus, Live m, boolean measuring) {
        float cx = DIAL_X + DIAL_C, cy = DIAL_Y + DIAL_C;
        float scale = bumpScale(now);
        if (hubPressed) {
            scale *= 0.985f;
        }
        canvas.save();
        if (scale != 1f) {
            canvas.scale(scale, scale, cx, cy);
        }
        fill.setColor(FRAME);
        fill.setShadowLayer(12, 0, 4, 0x4D0A101A);
        canvas.drawCircle(cx, cy, HUB_R, fill);
        fill.clearShadowLayer();
        stroke.setColor(LINE);
        stroke.setStrokeWidth(1);
        canvas.drawCircle(cx, cy, HUB_R, stroke);

        boolean holding = holdAddr >= 0 && now < holdUntil;
        boolean isPresent = present(focus);
        boolean liveNow = isPresent && m.everSeen && !m.silent;
        boolean hasShown = holding ? holdMa >= 0 : liveNow;
        int shownMa = holding ? holdMa : (liveNow ? m.frameMa : -1);

        text(canvas, hex1(focus), cx, DIAL_Y + HUB_ADDR_BASE, 15, INK3, Paint.Align.CENTER,
                monoBold, 0.1f, 1);

        // 中心数值:直接跳变,不滚动;大数字用连续变暖色
        String valText = hasShown ? fmtHub(shownMa) : BLANK;
        int valColor = hasShown ? warmColor(shownMa) : C_OFF;
        float numW = measure(valText, 40, sansMedium, 0);
        float unitW = measure("A", 15, sansMedium, 0);
        float groupLeft = cx - (numW + 4 + unitW) / 2f;
        float vb = DIAL_Y + HUB_VALUE_BASE;
        // 飞行途中中心继续显示冻住的值,flyer 的起点与它逐像素重合
        text(canvas, valText, groupLeft, vb, 40, valColor, Paint.Align.LEFT, sansMedium, 0, 1);
        text(canvas, "A", groupLeft + numW + 4, vb, 15, INK3, Paint.Align.LEFT, sansMedium, 0, 1);

        // 状态标签用离散判定色
        String label;
        int labelCol;
        if (measuring) {
            label = "存档中";
            labelCol = ACCENT;
        } else if (!isPresent) {
            label = "无表";
            labelCol = C_OFF;
        } else if (!liveNow) {
            label = "离线";
            labelCol = C_OFF;
        } else {
            label = statusLabel(m.status);
            labelCol = statusColor(m.status);
        }
        text(canvas, label, cx, DIAL_Y + HUB_STATE_BASE, 14, labelCol, Paint.Align.CENTER,
                sansMedium, 0, 1);

        String sub;
        if (measuring) {
            sub = "读这一台";
        } else if (autoMode) {
            int s = Math.max(0, (int) Math.ceil(remain));
            sub = String.format(Locale.CHINA, "%02d:%02d", s / 60, s % 60);
        } else if (!isPresent) {
            sub = "此地址无表";
        } else if (!liveNow) {
            long lastTs = m.everSeen ? m.frameWallTs
                    : (latest.containsKey(focus) ? latest.get(focus).timestamp : 0);
            sub = lastTs > 0 ? "最后一帧 " + clockFormat.format(new Date(lastTs)) : "没有帧";
        } else {
            MeterReading lastStored = firstRecordFor(focus);
            sub = lastStored != null
                    ? "上次存 " + clockFormat.format(new Date(lastStored.timestamp))
                    : "点一下读一次";
        }
        text(canvas, sub, cx, DIAL_Y + HUB_SUB_BASE, 12, INK3, Paint.Align.CENTER, mono, 0, 1);
        canvas.restore();
    }

    private float bumpScale(long now) {
        if (bumpStart < 0) {
            return 1f;
        }
        float t = (now - bumpStart) / 260f;
        if (t >= 1f) {
            bumpStart = -1;
            return 1f;
        }
        if (t < 0.45f) {
            return lerp(1f, 0.972f, EASE.getInterpolation(t / 0.45f));
        }
        return lerp(0.972f, 1f, EASE.getInterpolation((t - 0.45f) / 0.55f));
    }

    /* ── 采集模式开关 ── */
    private void drawFoot(Canvas canvas, long now) {
        // 间隔本身就是按钮:显示它的那几个字点开就是小键盘
        String chip = intervalChipText();
        String before = autoMode ? "每 " : "点中心读一次 · 自动 ";
        String after = autoMode ? " 自动测一轮" : "";
        float chipW = measure(chip, 12.5f, sans, 0) + 18;
        float beforeW = measure(before, 11.5f, sans, 0);
        float afterW = measure(after, 11.5f, sans, 0);
        float noteW = beforeW + chipW + afterW;
        float groupW = TOGGLE_W + 12 + noteW;
        float left = LEFT_CENTER - groupW / 2f;
        float top = FOOT_CY - TOGGLE_H / 2f;
        float bottom = FOOT_CY + TOGGLE_H / 2f;
        float radius = TOGGLE_H / 2f;

        fill.setColor(SUNKEN);
        canvas.drawRoundRect(left, top, left + TOGGLE_W, bottom, radius, radius, fill);
        float indX = left + 3 + modeInd.get(now) * TOGGLE_BTN_W;
        fill.setColor(FRAME);
        fill.setShadowLayer(3, 0, 1, 0x240A101A);
        canvas.drawRoundRect(indX, top + 3, indX + TOGGLE_BTN_W, bottom - 3,
                radius - 3, radius - 3, fill);
        fill.clearShadowLayer();

        float f = modeText.get(now);
        int manualCol = mixColor(INK, INK3, f);
        int autoCol = mixColor(INK3, INK, f);
        text(canvas, "手动", left + 3 + TOGGLE_BTN_W / 2f, FOOT_CY + 4.5f, 12.5f, manualCol,
                Paint.Align.CENTER, sansMedium, 0, 1);
        text(canvas, "自动", left + 3 + TOGGLE_BTN_W * 1.5f, FOOT_CY + 4.5f, 12.5f, autoCol,
                Paint.Align.CENTER, sansMedium, 0, 1);

        float noteLeft = left + TOGGLE_W + 12;
        text(canvas, before, noteLeft, FOOT_CY + 4.1f, 11.5f, INK3, Paint.Align.LEFT, sans, 0, 1);
        intervalChipRect.set(noteLeft + beforeW, FOOT_CY - 12.5f,
                noteLeft + beforeW + chipW, FOOT_CY + 12.5f);
        drawValueChip(canvas, intervalChipRect, chip, 12.5f, INK2, 1f);
        text(canvas, after, intervalChipRect.right, FOOT_CY + 4.1f, 11.5f, INK3,
                Paint.Align.LEFT, sans, 0, 1);

        modeManualRect.set(left + 3, top, left + 3 + TOGGLE_BTN_W, bottom);
        modeAutoRect.set(left + 3 + TOGGLE_BTN_W, top, left + 119, bottom);
    }

    private String intervalChipText() {
        if (intervalSeconds % 60 == 0) {
            return (intervalSeconds / 60) + " 分钟";
        }
        return intervalSeconds + " 秒";
    }

    /** 可点的数值牌:凹底加一圈描边,和旁边的说明文字区分开。 */
    private void drawValueChip(Canvas canvas, RectF rect, String label, float size,
                               int color, float alpha) {
        float r = rect.height() / 2f;
        fill.setColor(mulAlpha(SUNKEN, alpha));
        canvas.drawRoundRect(rect, r, r, fill);
        stroke.setColor(mulAlpha(LINE2, alpha));
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(rect, r, r, stroke);
        text(canvas, label, rect.centerX(), rect.centerY() + size * 0.36f, size, color,
                Paint.Align.CENTER, sans, 0, alpha);
    }

    /* ── 右侧标题与筛选 ── */
    private void drawRightHead(Canvas canvas, long now) {
        String title = allScope ? "全部电流表" : "地址 " + hex1(panelAddr);
        String sub = allScope ? "最近记录" : "电流趋势";
        float titleW = measure(title, 15, sansMedium, 0);
        text(canvas, title, RIGHT_X, HEAD_BASE, 15, INK, Paint.Align.LEFT, sansMedium, 0, 1);
        text(canvas, sub, RIGHT_X + titleW + 6, HEAD_BASE, 12, INK3, Paint.Align.LEFT, sans, 0, 1);

        float wOne = measure("本表", 11.5f, sans, 0) + 24;
        float wAll = measure("全部", 11.5f, sans, 0) + 24;
        chipAllRect.set(960 - wAll, 31.25f, 960, 58.5f);
        chipOneRect.set(960 - wAll - 5 - wOne, 31.25f, 960 - wAll - 5, 58.5f);
        float f = chipAll.get(now);
        drawChip(canvas, chipOneRect, "本表", 1 - f);
        drawChip(canvas, chipAllRect, "全部", f);
    }

    private void drawChip(Canvas canvas, RectF rect, String label, float active) {
        float r = rect.height() / 2f;
        int bg = mixColor(FRAME, INK, active);
        int border = mixColor(LINE2, INK, active);
        int col = mixColor(INK2, FRAME, active);
        fill.setColor(bg);
        canvas.drawRoundRect(rect, r, r, fill);
        stroke.setColor(border);
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(rect, r, r, stroke);
        text(canvas, label, rect.centerX(), rect.centerY() + 4.1f, 11.5f, col,
                Paint.Align.CENTER, sans, 0, 1);
    }

    /* ── 曲线 ── */
    private void drawChart(Canvas canvas, long now) {
        fill.setColor(FRAME);
        canvas.drawRoundRect(CHART_CARD, 16, 16, fill);
        stroke.setColor(LINE);
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(CHART_CARD, 16, 16, stroke);

        float fade = chartFadeStart < 0 ? 1f
                : EASE.getInterpolation(clamp((now - chartFadeStart) / 260f, 0f, 1f));

        Map<Integer, List<MeterReading>> groups = chartGroups();
        long tMin = Long.MAX_VALUE, tMax = Long.MIN_VALUE;
        float top = fullMa() / 1000f;
        int n = 0;
        for (List<MeterReading> arr : groups.values()) {
            for (MeterReading r : arr) {
                tMin = Math.min(tMin, r.timestamp);
                tMax = Math.max(tMax, r.timestamp);
                top = Math.max(top, (float) Math.ceil(r.currentMa / 100f) / 10f);
                n++;
            }
        }
        chartDomainSpan = n > 0 ? tMax - tMin : 0;
        chartDomainN = n;
        chartPtXY.clear();
        chartPtRec.clear();

        canvas.save();
        canvas.translate(PLOT_X, PLOT_Y);
        canvas.scale(PLOT_W / CHW, PLOT_H / CHH);

        float yLow = yOf(lowMa / 1000f, top);
        float yHigh = yOf(highMa / 1000f, top);
        fill.setColor(mulAlpha(0xFFC87C00, 0.06f * fade));
        canvas.drawRect(CH_L, yLow, CH_R, CH_B, fill);
        fill.setColor(mulAlpha(0xFFDC2626, 0.06f * fade));
        canvas.drawRect(CH_L, CH_T, CH_R, yHigh, fill);
        stroke.setColor(mulAlpha(LINE, fade));
        stroke.setStrokeWidth(1);
        canvas.drawLine(CH_L, CH_B, CH_R, CH_B, stroke);
        drawDashed(canvas, CH_L, yHigh, CH_R, yHigh, mulAlpha(C_HIGH, fade), limitDash);
        drawDashed(canvas, CH_L, yLow, CH_R, yLow, mulAlpha(C_LOW, fade), limitDash);
        if (top > fullMa() / 1000f + 0.05f) {
            text(canvas, String.format(Locale.US, "%.1f", top), 0, CH_T + 3, 9,
                    mulAlpha(INK3, fade), Paint.Align.LEFT, mono, 0, 1);
        }

        if (n < 1) {
            text(canvas, "还没有存档记录", (CH_L + CH_R) / 2f, 68, 11, mulAlpha(INK3, fade),
                    Paint.Align.CENTER, sans, 0, 1);
        } else {
            if (n == 1) {
                text(canvas, "再测一次就有曲线", (CH_L + CH_R) / 2f, CH_B - 8, 11,
                        mulAlpha(INK3, fade), Paint.Align.CENTER, sans, 0, 1);
            }
            List<float[]> labels = new ArrayList<>();          // addr, x, y
            List<Integer> addrs = new ArrayList<>(groups.keySet());
            Collections.sort(addrs);
            canvas.save();
            canvas.clipRect(CH_L, CH_T - 6, CH_R, CH_B + 6);
            long span = chartDomainSpan;
            for (int addr : addrs) {
                List<MeterReading> arr = groups.get(addr);
                int color = seriesColor(addr);
                if (arr.size() > 1) {
                    workPath.reset();
                    for (int i = 0; i < arr.size(); i++) {
                        MeterReading r = arr.get(i);
                        float px = xOf(r.timestamp, tMin, span);
                        float py = yOf(r.currentMa / 1000f, top);
                        if (i == 0) workPath.moveTo(px, py);
                        else workPath.lineTo(px, py);
                    }
                    stroke.setColor(mulAlpha(color, fade));
                    stroke.setStrokeWidth(2);
                    stroke.setStrokeJoin(Paint.Join.ROUND);
                    stroke.setStrokeCap(Paint.Cap.ROUND);
                    canvas.drawPath(workPath, stroke);
                    stroke.setStrokeCap(Paint.Cap.BUTT);
                }
                for (MeterReading r : arr) {
                    float px = xOf(r.timestamp, tMin, span);
                    float py = yOf(r.currentMa / 1000f, top);
                    chartPtXY.add(new float[]{px, py});
                    chartPtRec.add(r);
                    String st = statusOf(r);
                    if (MeterReading.LOW.equals(st) || MeterReading.HIGH.equals(st)) {
                        fill.setColor(mulAlpha(FRAME, fade));
                        canvas.drawCircle(px, py, 4, fill);
                        stroke.setColor(mulAlpha(statusColor(st), fade));
                        stroke.setStrokeWidth(2);
                        canvas.drawCircle(px, py, 4, stroke);
                    }
                }
                MeterReading last = arr.get(arr.size() - 1);
                float lx = xOf(last.timestamp, tMin, span);
                float ly = yOf(last.currentMa / 1000f, top);
                fill.setColor(mulAlpha(FRAME, fade));
                canvas.drawCircle(lx, ly, 2.6f, fill);
                stroke.setColor(mulAlpha(color, fade));
                stroke.setStrokeWidth(2);
                canvas.drawCircle(lx, ly, 2.6f, stroke);
                if (lx <= CH_R + 2) {
                    labels.add(new float[]{addr, lx + 6, ly});
                }
            }
            canvas.restore();

            // 标签贴在各条线真正结束的地方,横向挨得近的才纵向避让
            labels.sort((a, b) -> Float.compare(a[2], b[2]));
            for (int i = 1; i < labels.size(); i++) {
                float[] prev = labels.get(i - 1);
                if (Math.abs(labels.get(i)[1] - prev[1]) < 24
                        && labels.get(i)[2] - prev[2] < 11) {
                    labels.get(i)[2] = prev[2] + 11;
                }
            }
            for (float[] l : labels) {
                text(canvas, hex1((int) l[0]), Math.min(l[1], CH_R + 6),
                        clamp(l[2], CH_T + 4, CH_B) + 3, 9,
                        mulAlpha(seriesColor((int) l[0]), fade), Paint.Align.LEFT, mono, 0, 1);
            }
            if (readoutOn) {
                drawDashed(canvas, cursorX, CH_T, cursorX, CH_B, mulAlpha(INK3, fade), cursorDash);
            }
            if (chartDomainSpan > 0) {
                text(canvas, clockFormat.format(new Date(tOfU(viewU0, tMin))), CH_L, CH_B + 15,
                        9, mulAlpha(INK3, fade), Paint.Align.LEFT, mono, 0, 1);
                text(canvas, clockFormat.format(new Date(tOfU(viewU1, tMin))), CH_R, CH_B + 15,
                        9, mulAlpha(INK3, fade), Paint.Align.RIGHT, mono, 0, 1);
            }
        }
        canvas.restore();

        // 两条限值线的刻度就是阈值本身,点开就能改。画在图表变换之外,免得
        // 跟着 508/520 的缩放走形。
        drawLimitChip(canvas, highChipRect, fmtLimit(highMa),
                PLOT_Y + yHigh * (PLOT_H / CHH), C_HIGH, fade);
        drawLimitChip(canvas, lowChipRect, fmtLimit(lowMa),
                PLOT_Y + yLow * (PLOT_H / CHH), C_LOW, fade);

        // 复位按钮 .2s 淡入
        boolean zoomed = viewU0 > 0.001f || viewU1 < 0.999f;
        resetAlpha.retarget(zoomed ? 1f : 0f, now, 200);
        float ra = resetAlpha.get(now);
        if (ra > 0.003f) {
            float w = measure("复位", 10.5f, sans, 0) + 18;
            resetRect.set(952 - w, 72.5f, 952, 96.2f);
            fill.setColor(mulAlpha(FRAME, ra));
            canvas.drawRoundRect(resetRect, 11.85f, 11.85f, fill);
            stroke.setColor(mulAlpha(LINE2, ra));
            stroke.setStrokeWidth(1);
            canvas.drawRoundRect(resetRect, 11.85f, 11.85f, stroke);
            text(canvas, "复位", resetRect.centerX(), resetRect.centerY() + 3.8f, 10.5f,
                    INK2, Paint.Align.CENTER, sans, 0, ra);
        }
    }

    private void drawLimitChip(Canvas canvas, RectF rect, String label, float cy,
                               int color, float alpha) {
        // 右边贴着纵轴、往左边的刻度栏里长:阈值写到三位小数也压不到数据点上
        float w = measure(label, 11f, mono, 0) + 12;
        float right = PLOT_X + CH_L * (PLOT_W / CHW);
        rect.set(right - w, cy - 10f, right, cy + 10f);
        fill.setColor(mulAlpha(FRAME, alpha));
        canvas.drawRoundRect(rect, 10, 10, fill);
        stroke.setColor(mulAlpha(color, 0.42f * alpha));
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(rect, 10, 10, stroke);
        text(canvas, label, rect.centerX(), cy + 4f, 11, color, Paint.Align.CENTER,
                mono, 0, alpha);
    }

    private void drawReadout(Canvas canvas, long now) {
        float a = readoutAlpha.get(now);
        if (a <= 0.003f) {
            return;
        }
        float tw = measure(readoutText, 11, sans, 0);
        float w = tw + 18, h = 26.5f;
        float leftEdge = readoutX - w / 2f;
        fill.setColor(mulAlpha(INK, a));
        canvas.drawRoundRect(leftEdge, readoutY - h, leftEdge + w, readoutY, 9, 9, fill);
        text(canvas, readoutText, readoutX, readoutY - h / 2f + 4, 11, FRAME,
                Paint.Align.CENTER, sans, 0, a);
    }

    /* ── 记录头 ── */
    private void drawLogHead(Canvas canvas) {
        List<MeterReading> all = scopedRecords();
        String count = allScope
                ? "共 " + history.size() + " 条（上限 " + CAP + "）"
                : "地址 " + hex1(panelAddr) + " " + all.size() + " 条 · 全部 " + history.size();
        text(canvas, count, 434, LOGHEAD_CY + 4.1f, 11.5f, INK3, Paint.Align.LEFT, sans, 0, 1);

        int pages = all.isEmpty() ? 0 : (all.size() + PAGE_SIZE - 1) / PAGE_SIZE;
        drawPagerButton(canvas, prevRect, "‹", page > 0);
        text(canvas, (all.isEmpty() ? 0 : page + 1) + " / " + pages, 696, LOGHEAD_CY + 4.1f,
                11.5f, INK3, Paint.Align.CENTER, sans, 0, 1);
        drawPagerButton(canvas, nextRect, "›", page + 1 < pages);

        String clearLabel = clearArmed ? "确认清空" : "清空";
        float cw = measure(clearLabel, 11.5f, clearArmed ? sansMedium : sans, 0);
        text(canvas, clearLabel, 958, LOGHEAD_CY + 4.1f, 11.5f, clearArmed ? C_HIGH : INK3,
                Paint.Align.RIGHT, clearArmed ? sansMedium : sans, 0, 1);
        clearRect.set(958 - cw - 8, LOGHEAD_CY - 11, 960, LOGHEAD_CY + 11);
    }

    private void drawPagerButton(Canvas canvas, RectF rect, String label, boolean enabled) {
        float a = enabled ? 1f : 0.35f;
        fill.setColor(mulAlpha(FRAME, a));
        canvas.drawRoundRect(rect, 8, 8, fill);
        stroke.setColor(mulAlpha(LINE2, a));
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(rect, 8, 8, stroke);
        text(canvas, label, rect.centerX(), rect.centerY() + 5.4f, 15, INK2,
                Paint.Align.CENTER, sans, 0, a);
    }

    /* ── 记录 ── */
    private void drawLog(Canvas canvas, long now) {
        fill.setColor(FRAME);
        canvas.drawRoundRect(LOG_RECT, 16, 16, fill);
        stroke.setColor(LINE);
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(LOG_RECT, 16, 16, stroke);

        if (rows.isEmpty()) {
            String msg = allScope ? "还没有记录" : "地址 " + hex1(panelAddr) + " 还没有记录";
            text(canvas, msg, LOG_RECT.centerX(), LOG_RECT.top + 36, 12.5f, INK3,
                    Paint.Align.CENTER, sans, 0, 1);
            return;
        }
        canvas.save();
        canvas.clipRect(LOG_RECT);
        for (int i = 0; i < rows.size(); i++) {
            drawRow(canvas, i, now);
        }
        canvas.restore();
    }

    private float rowBaseline(int i, float fontSize) {
        return LOG_RECT.top + i * ROW_H + ROW_H / 2f + fontSize * 0.36f;
    }

    private void drawRow(Canvas canvas, int i, long now) {
        Row row = rows.get(i);
        MeterReading r = row.rec;
        float top = LOG_RECT.top + i * ROW_H;
        float rowAlpha = 1f;
        float clipFrac = 1f;
        float tyOff = 0f;
        if (row.anim == 1) {                                   // 长出来:clip 自上而下展开
            float t = (now - row.animStart) / 500f;
            if (t < 0) return;
            if (t < 1) {
                float e = EASE.getInterpolation(t);
                rowAlpha = e;
                clipFrac = e;
            } else {
                row.anim = 0;
            }
        } else if (row.anim == 2) {                            // 换页:下移淡入
            float t = (now - row.animStart) / 300f;
            if (t < 0) return;
            if (t < 1) {
                float e = EASE.getInterpolation(t);
                rowAlpha = e;
                tyOff = -4f * (1 - e);
            } else {
                row.anim = 0;
            }
        }
        float othersAlpha = rowAlpha;
        boolean valueHidden = row.forming;
        if (row.forming) {
            othersAlpha = 0f;
        } else if (row.formedAt > 0) {
            float t = (now - row.formedAt) / 260f;
            if (t >= 1) {
                row.formedAt = 0;
            } else {
                othersAlpha = rowAlpha * EASE.getInterpolation(clamp(t, 0f, 1f));
            }
        }
        canvas.save();
        if (clipFrac < 1f) {
            canvas.clipRect(LOG_RECT.left, top, LOG_RECT.right, top + ROW_H * clipFrac);
        }
        if (tyOff != 0f) {
            canvas.translate(0, tyOff);
        }
        if (i > 0) {
            stroke.setColor(mulAlpha(LINE, rowAlpha));
            stroke.setStrokeWidth(1);
            canvas.drawLine(LOG_RECT.left, top, LOG_RECT.right, top, stroke);
        }
        String status = statusOf(r);
        int col = statusColor(status);
        text(canvas, clockFormat.format(new Date(r.timestamp)), REC_T_X, rowBaseline(i, 11.5f),
                11.5f, INK2, Paint.Align.LEFT, mono, 0, othersAlpha);
        text(canvas, hex1(r.address), REC_A_X, rowBaseline(i, 11.5f), 11.5f, INK2,
                Paint.Align.LEFT, monoBold, 0, othersAlpha);
        text(canvas, "AUTO".equalsIgnoreCase(r.source) ? "自动" : "手动", REC_SRC_X,
                rowBaseline(i, 10.5f), 10.5f, INK3, Paint.Align.LEFT, sans, 0, othersAlpha);
        if (!valueHidden) {
            String value = r.currentMa < 0 ? BLANK : fmtA(r.currentMa);
            float unitW = measure("A", 13, sansMedium, 0);
            float base = rowBaseline(i, 13);
            text(canvas, "A", REC_V_RIGHT, base, 13, col, Paint.Align.RIGHT, sansMedium, 0, rowAlpha);
            text(canvas, value, REC_V_RIGHT - unitW - 3, base, 13, col, Paint.Align.RIGHT,
                    sansMedium, 0, rowAlpha);
        }
        text(canvas, statusLabel(status), REC_S_RIGHT, rowBaseline(i, 10.5f), 10.5f, col,
                Paint.Align.RIGHT, sansMedium, 0, othersAlpha);
        canvas.restore();
    }

    /* ── 飞行 ── */
    private void drawFlyer(Canvas canvas, long now) {
        if (!flyActive) {
            return;
        }
        float t = clamp((now - flyStart) / 460f, 0f, 1f);
        float e = FLY.getInterpolation(t);
        float right = lerp(flyFromRight, flyToRight, e);
        float base = lerp(flyFromBase, flyToBase, e);
        float numSize = lerp(40, 13, e);
        float unitSize = lerp(15, 13, e);
        float gap = lerp(4, 3, e);
        int numColor = mixColor(flyNumFrom, flyNumTo, e);
        int unitColor = mixColor(flyUnitFrom, flyUnitTo, e);
        float unitW = measure("A", unitSize, sansMedium, 0);
        text(canvas, "A", right, base, unitSize, unitColor, Paint.Align.RIGHT, sansMedium, 0, 1);
        text(canvas, flyNum, right - unitW - gap, base, numSize, numColor, Paint.Align.RIGHT,
                sansMedium, 0, 1);
    }

    /* ── 数字小键盘 ── */
    private void drawKeypad(Canvas canvas, long now) {
        if (padMode == PAD_NONE) {
            return;
        }
        float a = padAlpha.get(now);
        if (a <= 0.003f) {
            return;
        }
        fill.setColor(mulAlpha(INK, 0.34f * a));
        canvas.drawRoundRect(0, 0, DESIGN_W, DESIGN_H, 30, 30, fill);

        // 弹入的 96% → 100%,和收起时反过来走同一条路
        int save = a < 0.999f
                ? canvas.saveLayerAlpha(padCard.left - 50, padCard.top - 50,
                padCard.right + 50, padCard.bottom + 50, Math.round(a * 255))
                : canvas.save();
        canvas.scale(lerp(0.96f, 1f, a), lerp(0.96f, 1f, a),
                padCard.centerX(), padCard.centerY());
        float shakeT = padErrorAt > 0 ? (now - padErrorAt) / 380f : 1f;
        if (shakeT < 1f) {
            canvas.translate((float) Math.sin(shakeT * Math.PI * 6) * 6f * (1 - shakeT), 0);
        }

        fill.setColor(FRAME);
        fill.setShadowLayer(34, 0, 14, 0x590A101A);
        canvas.drawRoundRect(padCard, 20, 20, fill);
        fill.clearShadowLayer();
        stroke.setColor(LINE2);
        stroke.setStrokeWidth(1);
        canvas.drawRoundRect(padCard, 20, 20, stroke);

        float paneLeft = padUnitA.left;
        float paneRight = padUnitB.right;
        text(canvas, padTitle(), paneLeft, padCard.top + 46, 15, INK, Paint.Align.LEFT,
                sansMedium, 0, 1);
        text(canvas, padCurrent(), paneLeft, padCard.top + 68, 11, INK3, Paint.Align.LEFT,
                sans, 0, 1);

        float base = padCard.top + 138;
        float typedW = padInput.isEmpty() ? 0 : measure(padInput, 38, sansMedium, 0);
        if (!padInput.isEmpty()) {
            text(canvas, padInput, paneLeft, base, 38, INK, Paint.Align.LEFT, sansMedium, 0, 1);
        }
        // 光标:还没打字时,它是唯一说明「这里在等输入」的东西
        if ((now - padCaretAt) / 530L % 2 == 0) {
            fill.setColor(ACCENT);
            canvas.drawRoundRect(paneLeft + typedW + 2, base - 27,
                    paneLeft + typedW + 4.4f, base + 4, 1.2f, 1.2f, fill);
        }
        stroke.setColor(padInput.isEmpty() ? LINE2 : ACCENT);
        stroke.setStrokeWidth(padInput.isEmpty() ? 1 : 1.6f);
        canvas.drawLine(paneLeft, base + 12, paneRight, base + 12, stroke);
        boolean bad = padErrorAt > 0 && now - padErrorAt < 1400;
        text(canvas, padRange(), paneLeft, base + 32, 10.5f, bad ? C_HIGH : INK3,
                Paint.Align.LEFT, mono, 0, 1);

        for (int i = 0; i < padKeys.length; i++) {
            drawPadKey(canvas, i);
        }
        boolean ready = !Float.isNaN(padValue());
        drawPadUnit(canvas, padUnitA, padUnitLabel(false), padPressed == KEY_UNIT_A, ready);
        drawPadUnit(canvas, padUnitB, padUnitLabel(true), padPressed == KEY_UNIT_B, ready);
        text(canvas, "按单位键即保存", (paneLeft + paneRight) / 2f, padUnitA.bottom + 19, 10.5f,
                INK3, Paint.Align.CENTER, sans, 0, 1);

        stroke.setColor(padPressed == KEY_CLOSE ? INK : INK3);
        stroke.setStrokeWidth(1.6f);
        float ccx = padClose.centerX(), ccy = padClose.centerY();
        canvas.drawLine(ccx - 5, ccy - 5, ccx + 5, ccy + 5, stroke);
        canvas.drawLine(ccx + 5, ccy - 5, ccx - 5, ccy + 5, stroke);
        canvas.restoreToCount(save);
    }

    private void drawPadKey(Canvas canvas, int i) {
        RectF r = padKeys[i];
        boolean pressed = padPressed == i;
        canvas.save();
        if (pressed) {
            canvas.scale(0.97f, 0.97f, r.centerX(), r.centerY());
        }
        fill.setColor(pressed ? mixColor(SUNKEN, INK, 0.1f) : SUNKEN);
        canvas.drawRoundRect(r, 12, 12, fill);
        if (i == 11) {
            drawBackspace(canvas, r.centerX(), r.centerY());
        } else {
            String label = i == 9 ? "." : i == 10 ? "0" : String.valueOf((char) ('1' + i));
            text(canvas, label, r.centerX(), r.centerY() + 7, 20, INK, Paint.Align.CENTER,
                    sansMedium, 0, 1);
        }
        canvas.restore();
    }

    /** 单位键就是确认键,所以它长得像确认键;没输入数字时是灰的。 */
    private void drawPadUnit(Canvas canvas, RectF r, String label, boolean pressed,
                             boolean ready) {
        canvas.save();
        if (pressed) {
            canvas.scale(0.97f, 0.97f, r.centerX(), r.centerY());
        }
        fill.setColor(ready ? (pressed ? mixColor(ACCENT, INK, 0.22f) : ACCENT)
                : mulAlpha(INK, 0.14f));
        canvas.drawRoundRect(r, 13, 13, fill);
        text(canvas, label, r.centerX(), r.centerY() + 5.4f, 15, ready ? FRAME : INK3,
                Paint.Align.CENTER, sansMedium, 0, 1);
        canvas.restore();
    }

    /* 退格画出来而不是打出来:⌫ 在不少机器的默认字体里是空豆腐。 */
    private void drawBackspace(Canvas canvas, float cx, float cy) {
        workPath.reset();
        workPath.moveTo(cx - 11, cy);
        workPath.lineTo(cx - 4, cy - 7);
        workPath.lineTo(cx + 10, cy - 7);
        workPath.lineTo(cx + 10, cy + 7);
        workPath.lineTo(cx - 4, cy + 7);
        workPath.close();
        stroke.setColor(INK2);
        stroke.setStrokeWidth(1.6f);
        stroke.setStrokeJoin(Paint.Join.ROUND);
        canvas.drawPath(workPath, stroke);
        canvas.drawLine(cx + 1, cy - 3.2f, cx + 7, cy + 3.2f, stroke);
        canvas.drawLine(cx + 7, cy - 3.2f, cx + 1, cy + 3.2f, stroke);
    }

    /* ══════════ 图表数据 ══════════ */
    private Map<Integer, List<MeterReading>> chartGroups() {
        Map<Integer, List<MeterReading>> groups = new LinkedHashMap<>();
        for (MeterReading r : scopedRecords()) {
            if (r.currentMa < 0 || MeterReading.OFFLINE.equals(r.status)) {
                continue;                                      // 离线记录不进曲线,留成缺口
            }
            List<MeterReading> arr = groups.computeIfAbsent(r.address, k -> new ArrayList<>());
            if (arr.size() < 200) {
                arr.add(r);
            }
        }
        for (List<MeterReading> arr : groups.values()) {
            Collections.reverse(arr);                          // 时间升序
        }
        return groups;
    }

    private float yOf(float amps, float top) {
        return CH_B - amps / top * (CH_B - CH_T);
    }

    private float xOf(long ts, long tMin, long span) {
        float u = span > 0 ? (ts - tMin) / (float) span : 0.5f;   // 只有一条记录时放在正中
        return CH_L + (u - viewU0) / (viewU1 - viewU0) * (CH_R - CH_L);
    }

    private long tOfU(float u, long tMin) {
        return tMin + (long) (u * chartDomainSpan);
    }

    /* ══════════ 小工具 ══════════ */
    private boolean present(int addr) {
        return registered.contains(addr) || live[addr].everSeen;
    }

    private MeterReading firstRecordFor(int addr) {
        for (MeterReading r : history) {
            if (r.address == addr) {
                return r;
            }
        }
        return null;
    }

    private String classifyStatus(int ma) {
        return MeterReading.classify(ma, lowMa, highMa);
    }

    /**
     * 显示时用当前阈值重判一次。库里存的是那一刻的判定,改完阈值再看,
     * 一条 1.8 A 的记录不能既在红线之上又写着「正常」——曲线上的线就画在
     * 旁边,两种说法同时出现只会让人以为界面坏了。
     */
    private String statusOf(MeterReading r) {
        if (r.currentMa < 0 || MeterReading.OFFLINE.equals(r.status)) {
            return r.status;
        }
        return classifyStatus(r.currentMa);
    }

    private static String hex1(int n) {
        return Integer.toHexString(n).toUpperCase(Locale.ROOT);
    }

    private static String fmtA(int ma) {
        return String.format(Locale.US, "%.3f", ma / 1000f);
    }

    /** 限值牌上的读数:0.200 A 写成 0.2,0.250 A 才写两位——刻度栏只有这么宽。 */
    private static String fmtLimit(int ma) {
        String s = fmtA(ma);
        while (s.endsWith("0") && !s.endsWith(".0")) {
            s = s.substring(0, s.length() - 1);
        }
        return s;
    }

    private static String fmtHub(int ma) {
        return String.format(Locale.US, "%.3f", clamp(ma / 1000f, 0f, 9.999f));
    }

    /** 应用版本,取不到就留空——它是辅助信息,不该因为取不到而影响别的显示。 */
    private String appVersion() {
        if (cachedVersion == null) {
            cachedVersion = "";
            try {
                cachedVersion = getContext()
                        .getPackageManager()
                        .getPackageInfo(getContext().getPackageName(), 0)
                        .versionName;
            } catch (Exception ignored) {
                // 保持空串
            }
        }
        return cachedVersion;
    }

    private static String statusLabel(String status) {
        // null = 在场但还没读过数。不能落进「正常」:没读过就不知道正不正常,
        // 显示成绿色的正常是这个界面唯一会主动骗人的地方。
        if (status == null) return "未读";
        if (MeterReading.LOW.equals(status)) return "低限";
        if (MeterReading.HIGH.equals(status)) return "超限";
        if (MeterReading.OFFLINE.equals(status)) return "离线";
        return "正常";
    }

    private static int statusColor(String status) {
        if (status == null) return INK3;
        if (MeterReading.LOW.equals(status)) return C_LOW;
        if (MeterReading.HIGH.equals(status)) return C_HIGH;
        if (MeterReading.OFFLINE.equals(status)) return C_OFF;
        return C_OK;
    }

    /* 越靠近阈值越暖:沿色相「绿 → 琥珀 → 红」,不在 RGB 里直接对插 */
    private static int heat(float t, boolean toRed) {
        float stop = toRed ? 0.55f : 1f;
        if (t <= stop) {
            float u = stop > 0 ? t / stop : 0;
            return hsl(lerp(160, 42, u), lerp(84, 100, u), lerp(32, 40, u));
        }
        float u = (t - stop) / (1 - stop);
        return hsl(lerp(42, 0, u), lerp(100, 72, u), lerp(40, 50, u));
    }

    /* 变暖的区间跟着阈值缩放:默认 0.2/2.0 A 时正好是原型的 0.16 和 0.32 A。
       写死成绝对宽度的话,把超限调到 0.5 A 会让整条量程都是红的。 */
    private int warmColor(int ma) {
        if (ma < 0) {
            return C_OFF;
        }
        float amps = ma / 1000f;
        float lowBand = Math.max(0.02f, lowMa * 0.8f / 1000f);
        float highBand = Math.max(0.05f, highMa * 0.16f / 1000f);
        float lowP = clamp((lowMa / 1000f + lowBand - amps) / lowBand, 0f, 1f);
        float highP = clamp((amps - (highMa / 1000f - highBand)) / highBand, 0f, 1f);
        return highP >= lowP ? heat(highP, true) : heat(lowP, false);
    }

    /* 16 个地址 16 个色相,落在 172°–320°,乘 5 与 16 互素拉开相邻地址 */
    private static int seriesColor(int addr) {
        return hsl(172 + ((addr * 5) % 16) * 9.8f, 58, 46);
    }

    private static int hsl(float h, float s, float l) {
        s /= 100f;
        l /= 100f;
        float c = (1 - Math.abs(2 * l - 1)) * s;
        float hp = (((h % 360) + 360) % 360) / 60f;
        float x = c * (1 - Math.abs(hp % 2 - 1));
        float r = 0, g = 0, b = 0;
        if (hp < 1) { r = c; g = x; }
        else if (hp < 2) { r = x; g = c; }
        else if (hp < 3) { g = c; b = x; }
        else if (hp < 4) { g = x; b = c; }
        else if (hp < 5) { r = x; b = c; }
        else { r = c; b = x; }
        float m = l - c / 2;
        return Color.rgb(Math.round((r + m) * 255), Math.round((g + m) * 255),
                Math.round((b + m) * 255));
    }

    private void drawDashed(Canvas canvas, float x1, float y1, float x2, float y2,
                            int color, DashPathEffect dash) {
        workPath.reset();
        workPath.moveTo(x1, y1);
        workPath.lineTo(x2, y2);
        stroke.setColor(color);
        stroke.setStrokeWidth(1);
        stroke.setPathEffect(dash);
        canvas.drawPath(workPath, stroke);
        stroke.setPathEffect(null);
    }

    private void text(Canvas canvas, String s, float x, float baseline, float size, int color,
                      Paint.Align align, Typeface tf, float letterSpacingEm, float alpha) {
        textPaint.setTextSize(size);
        textPaint.setTypeface(tf);
        textPaint.setTextAlign(align);
        textPaint.setLetterSpacing(letterSpacingEm);
        textPaint.setColor(color);
        if (alpha < 1f) {
            textPaint.setAlpha((int) (Color.alpha(color) * alpha));
        }
        canvas.drawText(s, x, baseline, textPaint);
    }

    private float measure(String s, float size, Typeface tf, float letterSpacingEm) {
        textPaint.setTextSize(size);
        textPaint.setTypeface(tf);
        textPaint.setTextAlign(Paint.Align.LEFT);
        textPaint.setLetterSpacing(letterSpacingEm);
        return textPaint.measureText(s);
    }

    private static int mulAlpha(int color, float f) {
        int a = (int) (Color.alpha(color) * clamp(f, 0f, 1f));
        return (color & 0x00FFFFFF) | (a << 24);
    }

    private static int mixColor(int from, int to, float t) {
        t = clamp(t, 0f, 1f);
        int a = Math.round(Color.alpha(from) + (Color.alpha(to) - Color.alpha(from)) * t);
        int r = Math.round(Color.red(from) + (Color.red(to) - Color.red(from)) * t);
        int g = Math.round(Color.green(from) + (Color.green(to) - Color.green(from)) * t);
        int b = Math.round(Color.blue(from) + (Color.blue(to) - Color.blue(from)) * t);
        return Color.argb(a, r, g, b);
    }

    private static float clamp(float v, float lo, float hi) {
        return Math.max(lo, Math.min(hi, v));
    }

    private static float lerp(float a, float b, float t) {
        return a + (b - a) * t;
    }

    private static float norm180(float d) {
        return ((d + 180) % 360 + 360) % 360 - 180;
    }

    private static float dist(float x, float y, float cx, float cy) {
        return (float) Math.hypot(x - cx, y - cy);
    }
}
