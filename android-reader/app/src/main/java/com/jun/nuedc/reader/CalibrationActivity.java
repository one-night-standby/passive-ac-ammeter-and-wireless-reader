package com.jun.nuedc.reader;

import android.Manifest;
import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Deque;
import java.util.List;
import java.util.Locale;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * 现场标定台:把一张 (RMS, 标定表读数) 的表推进电流表的 flash,盖掉固件里那张。
 *
 * <p>为什么要有它:1(1) 那 30 分比的是电流表<b>自己显示</b>的读数对标定表,而固件
 * 里那张表是在我们自己的台子上、对着我们自己那台参考表拟的。验收现场换一台
 * 4½ 位手持表,AC 档的差就可能盖过 0.5% 的整个预算——这不是我们做得再仔细能
 * 消掉的东西。所以校正必须回到表上,不能只改手机显示;也所以它写 flash,不是
 * 存在手机里:电流表靠被测电流取电,回路一断就掉电,存手机上的表在每一次掉电
 * 之后都要人记得重推。
 *
 * <p>界面不替人做判断。记点没有任何前置条件,点记成什么样、记多少个、按什么
 * 顺序记,都是操作的人的事;这里只把收到的东西如实摆出来,再原样存下去。要
 * 推的时候才按 RMS 排一次序——那是固件查表的前提,不是对输入的要求。
 *
 * <p>它是独立的一个图标,和读表器界面分开:自动模式要"一键启动",那个界面上不
 * 该多一个能把表改坏的按钮。两边共用 {@link MeterPollingService} 的那条 BLE 链。
 */
public final class CalibrationActivity extends Activity {
    private static final int REQUEST_PERMISSIONS = 43;

    /** 表内表的点数上限,必须和固件的 {@code cal::FIELD_MAX} 一致。 */
    private static final int MAX_POINTS = 16;

    /**
     * 手机上能记多少。比表里能装的多得多——多记几个再挑,比记的时候就得算着
     * 数强。挑的事交给 {@link #pushTable()}。
     */
    private static final int MAX_CAPTURED = 64;

    /**
     * 实况里显示抖动幅度用的窗口。只是显示——记点不看它。
     *
     * <p>这里一条都不拦:按下「记录此点」就把当前这一帧的 RMS 和填进去的读数
     * 原样存下来,稳没稳、表报什么 FLAG、和已有的点挨多近,都不作数。要不要
     * 这个点是操作的人的判断,界面只负责把它看到的东西如实摆出来。
     */
    private static final int SPREAD_WINDOW = 5;

    /** 读数节奏。一次测量 260 ms 加亮屏 1 s,比这更密只是白等。 */
    private static final long POLL_MS = 1_500L;

    /**
     * 一条 CALPT 等 CALACK 的时限,以及重发次数。
     *
     * <p>两秒不是链路时延定的,是电流表的亮屏窗口:一次读数之后它要亮 1 秒,
     * 这一秒里命令只是躺在串口缓冲里等着。推送的第一条命令正好接在最后一次
     * 读数后面,所以它的应答天然可能迟到将近一秒。
     */
    private static final long ACK_TIMEOUT_MS = 2_000L;
    private static final int ACK_RETRIES = 3;

    /** CALEND 要等表把 flash 写完,可能还要先擦一个扇区,给得比 ACK 宽。 */
    private static final long COMMIT_TIMEOUT_MS = 5_000L;

    /** 看到 SRC=ROM 之后,最快隔多久自动补推一次,以及最多连着补几次。 */
    private static final long REPUSH_COOLDOWN_MS = 10_000L;
    private static final int MAX_REPUSH_ATTEMPTS = 2;

    private static final Pattern TEST_PATTERN = Pattern.compile(
            "^METER_TEST,ADDR=(\\d{1,2}),CURRENT_MA=(\\d{1,7}),STATUS=([A-Z_]+)$");
    private static final Pattern CAL_PATTERN = Pattern.compile(
            "^METER_CAL,ADDR=(\\d{1,2}),RMS=([0-9.]+),GAIN=(\\d+),FLAG=([A-Z_]+)(?:,SRC=([A-Z]+))?.*$");
    private static final Pattern ACK_PATTERN = Pattern.compile(
            "^CALACK,ADDR=(\\d{1,2}),I=(\\d{1,2})(?:,ERR=([A-Z_]+))?$");
    private static final Pattern STAT_PATTERN = Pattern.compile(
            "^CALSTAT,ADDR=(\\d{1,2}),SRC=([A-Z]+),N=(\\d{1,2})(?:,ERR=([A-Z_]+))?$");

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final Deque<Double> recentRms = new ArrayDeque<>();
    private final List<double[]> points = new ArrayList<>();
    /** 上一次「推送」实际发出去的那一份:排过序、去过重。 */
    private List<double[]> pushOrder = new ArrayList<>();
    private final StringBuilder log = new StringBuilder();
    private String lastLogLine = "";
    private int repeatedLogLines;

    private CalibrationStore store;
    private int address = 6;

    private double lastRms = Double.NaN;
    private int lastMa = -1;
    private String lastFlag = "";
    private String lastSource = "";
    private long lastFrameAt;

    /** 推送状态机:-1 表示没在推,否则是正在等应答的点号。 */
    private int pushingIndex = -1;
    private int pushRetries;
    private boolean committing;
    private long lastRepushAt;
    private int repushAttempts;

    private TextView liveView;
    private TextView rmsView;
    private TextView rmsHint;
    private TextView sourcePill;
    private TextView pointsCount;
    private TextView pointsView;
    private TextView logView;
    private TextView addressView;
    private EditText referenceInput;
    private ScrollView logScroll;

    private final Runnable poll = new Runnable() {
        @Override
        public void run() {
            // 推送期间不问读数:表在一次测量结束时会丢掉缓冲里没读完的命令
            // (固件的 discard_pending),推到一半插一条 MEAS 就可能吃掉一个点。
            if (pushingIndex < 0 && !committing) {
                Intent intent = new Intent(
                        CalibrationActivity.this, MeterPollingService.class);
                intent.setAction(MeterPollingService.ACTION_READ_NOW);
                intent.putExtra(MeterPollingService.EXTRA_ADDRESS, address);
                startService(intent);
            }
            handler.postDelayed(this, POLL_MS);
        }
    };

    private final Runnable ackTimeout = () -> {
        if (pushingIndex < 0 && !committing) {
            return;
        }
        if (pushRetries >= ACK_RETRIES) {
            String what = committing ? "CALEND" : ("点 " + pushingIndex);
            appendLog("✗ " + what + " 无应答,推送中止");
            abortPush();
            return;
        }
        pushRetries++;
        appendLog("… 重发(第 " + pushRetries + " 次)");
        sendCurrentPushStep();
    };

    private final BroadcastReceiver receiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent.getAction();
            if (MeterPollingService.ACTION_LINE.equals(action)) {
                onLine(intent.getStringExtra(MeterPollingService.EXTRA_LINE));
            } else if (MeterPollingService.ACTION_STATE.equals(action)) {
                String detail = intent.getStringExtra(MeterPollingService.EXTRA_DETAIL);
                if (detail != null) {
                    appendLog("· " + detail);
                }
            }
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        store = new CalibrationStore(this);
        address = store.address();
        points.addAll(store.points());
        setContentView(buildUi());
        refreshPoints();
        refreshLive();
        registerLineReceiver();
        ensurePermissions(() -> {
            startReader(MeterPollingService.ACTION_CONNECT_BASIC);
            sendCommand(String.format(Locale.ROOT, "CALGET,ADDR=%d", address));
        });
        appendLog("标定台就绪。");
    }

    @Override
    protected void onResume() {
        super.onResume();
        handler.removeCallbacks(poll);
        handler.post(poll);
    }

    @Override
    protected void onPause() {
        super.onPause();
        handler.removeCallbacks(poll);
    }

    @Override
    protected void onDestroy() {
        handler.removeCallbacksAndMessages(null);
        unregisterReceiver(receiver);
        super.onDestroy();
    }

    /* ══════════ 界面 ══════════ */

    /**
     * 取自 app-prototype.html 的同一套色板和圆角,读表器界面用的也是它。这是
     * 调试工具,但它和读表器装在同一台手机上、在同一个场子里被同一个人看,长成
     * 两个样子没有道理。
     */
    private static final int PAGE = 0xFFE8ECF2;
    private static final int SURFACE = 0xFFFFFFFF;
    private static final int SUNKEN = 0xFFF1F4F9;
    private static final int INK = 0xFF131922;
    private static final int INK_2 = 0xFF767C86;
    private static final int LINE = 0x1A131922;
    private static final int LINE_2 = 0x29131922;
    private static final int ACCENT = 0xFF1F6FEB;
    private static final int MUTED = 0xFF8A94A3;
    private static final int DANGER = 0xFFDC2626;

    private View buildUi() {
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(dp(14), dp(14), dp(14), dp(14));

        root.addView(header());
        root.addView(liveCard(), stretch());
        root.addView(captureCard(), stretch());
        root.addView(pointsCard(), stretch());
        root.addView(actionCard(), stretch());
        root.addView(logCard(), stretch());

        // 整页滚。这几张卡加起来本来就比一屏高,靠 weight 去分只会把最下面的
        // 按钮裁掉。页面自己不会乱跳——会跳的是 appendLog 里那个 fullScroll,
        // 它已经换成只滚日志框自己的 scrollTo 了。
        ScrollView page = new ScrollView(this);
        page.setBackgroundColor(PAGE);
        page.setFillViewport(true);
        page.addView(root);
        return page;
    }

    /** 标题 + 地址步进器。地址是这个界面唯一会改表的东西,所以它在最上面。 */
    private View header() {
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);

        TextView title = new TextView(this);
        title.setText("标定台");
        title.setTextSize(19);
        title.setTypeface(Typeface.DEFAULT_BOLD);
        title.setTextColor(INK);
        row.addView(title, new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f));

        LinearLayout stepper = new LinearLayout(this);
        stepper.setOrientation(LinearLayout.HORIZONTAL);
        stepper.setGravity(Gravity.CENTER_VERTICAL);
        stepper.setBackground(rounded(SURFACE, dp(999), LINE_2));
        stepper.setPadding(dp(4), dp(2), dp(4), dp(2));
        stepper.addView(stepButton("−", v -> setAddress(address - 1)));
        addressView = new TextView(this);
        addressView.setText(String.valueOf(address));
        addressView.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        addressView.setTextSize(17);
        addressView.setTextColor(INK);
        addressView.setGravity(Gravity.CENTER);
        addressView.setMinWidth(dp(34));
        stepper.addView(addressView);
        stepper.addView(stepButton("+", v -> setAddress(address + 1)));

        TextView caption = new TextView(this);
        caption.setText("表地址");
        caption.setTextSize(11.5f);
        caption.setTextColor(INK_2);
        caption.setPadding(0, 0, dp(8), 0);
        row.addView(caption);
        row.addView(stepper);
        return row;
    }

    /**
     * 实况。三行定死,不随内容增减——行数一变,下面整块跟着跳,而这块每秒都在刷。
     */
    private View liveCard() {
        LinearLayout card = card();

        sourcePill = new TextView(this);
        sourcePill.setTextSize(11.5f);
        sourcePill.setTypeface(Typeface.DEFAULT_BOLD);
        sourcePill.setPadding(dp(10), dp(4), dp(10), dp(5));
        card.addView(sourcePill);

        rmsView = new TextView(this);
        rmsView.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        rmsView.setTextSize(34);
        rmsView.setTextColor(INK);
        rmsView.setPadding(0, dp(8), 0, 0);
        card.addView(rmsView);

        rmsHint = new TextView(this);
        rmsHint.setTextSize(11.5f);
        rmsHint.setTextColor(INK_2);
        card.addView(rmsHint);

        liveView = new TextView(this);
        liveView.setTypeface(Typeface.MONOSPACE);
        liveView.setTextSize(13);
        liveView.setTextColor(INK_2);
        liveView.setLines(2);
        liveView.setPadding(0, dp(10), 0, 0);
        card.addView(liveView);
        return card;
    }

    private View captureCard() {
        LinearLayout card = card();
        card.addView(caption("标定表读数"));

        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);
        row.setPadding(0, dp(6), 0, 0);

        referenceInput = new EditText(this);
        referenceInput.setInputType(
                InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_DECIMAL);
        referenceInput.setHint("1.2345");
        referenceInput.setTextColor(INK);
        referenceInput.setHintTextColor(MUTED);
        referenceInput.setTypeface(Typeface.MONOSPACE);
        referenceInput.setTextSize(20);
        referenceInput.setBackground(rounded(SUNKEN, dp(10), LINE));
        referenceInput.setPadding(dp(12), dp(10), dp(12), dp(10));
        row.addView(referenceInput, new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f));

        TextView unit = new TextView(this);
        unit.setText("A");
        unit.setTextSize(15);
        unit.setTextColor(INK_2);
        unit.setPadding(dp(8), 0, dp(10), 0);
        row.addView(unit);
        row.addView(filledButton("记录此点", ACCENT, v -> capturePoint()));
        card.addView(row);
        return card;
    }

    private View pointsCard() {
        LinearLayout card = card();

        LinearLayout head = new LinearLayout(this);
        head.setOrientation(LinearLayout.HORIZONTAL);
        head.setGravity(Gravity.CENTER_VERTICAL);
        head.addView(caption("已记的点"), new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        pointsCount = new TextView(this);
        pointsCount.setTextSize(11.5f);
        pointsCount.setTextColor(INK_2);
        pointsCount.setTypeface(Typeface.MONOSPACE);
        head.addView(pointsCount);
        card.addView(head);

        pointsView = new TextView(this);
        pointsView.setTypeface(Typeface.MONOSPACE);
        pointsView.setTextSize(13);
        pointsView.setTextColor(INK);
        pointsView.setLineSpacing(dp(3), 1f);
        pointsView.setPadding(dp(10), dp(8), dp(10), dp(8));
        pointsView.setBackground(rounded(SUNKEN, dp(10), 0));
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT);
        params.topMargin = dp(6);
        card.addView(pointsView, params);

        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setPadding(0, dp(8), 0, 0);
        row.addView(quietButton("删最后一点", INK_2, v -> dropLastPoint()));
        row.addView(quietButton("清空", DANGER, v -> clearPoints()));
        row.addView(quietButton("导出", INK_2, v -> exportTable()));
        card.addView(row);
        return card;
    }

    private View actionCard() {
        LinearLayout card = card();

        // 两个按钮是一个开关的两头,不是"做一件事"和"撤销"。表里两张表随时可以
        // 换着用:出厂表一直在固件里,现场表的点一直在手机上,换过去换回来都不用
        // 重新量。当场要证明现场标定确实在起作用,来回按这两个就是。
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);
        row.addView(filledButton("推送并启用", ACCENT, v -> startPush()),
                new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        row.addView(outlineButton("切回出厂表", v -> clearMeterTable()),
                new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 0.85f));
        // 「读状态」和那两个不是一个量级的动作,所以它没有面,只有字。三个并成
        // 一行,免得为一个次要按钮空出一整条带子。
        row.addView(quietButton("读状态", INK_2,
                v -> sendCommand(String.format(Locale.ROOT, "CALGET,ADDR=%d", address))));
        card.addView(row);
        return card;
    }

    private View logCard() {
        LinearLayout card = card();
        card.addView(caption("日志"));
        logView = new TextView(this);
        logView.setTypeface(Typeface.MONOSPACE);
        logView.setTextSize(11.5f);
        logView.setTextColor(INK_2);
        logView.setLineSpacing(dp(2), 1f);
        logView.setPadding(dp(10), dp(8), dp(10), dp(8));
        logScroll = new ScrollView(this);
        logScroll.setBackground(rounded(SUNKEN, dp(10), 0));
        logScroll.addView(logView);
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, dp(120));
        params.topMargin = dp(6);
        card.addView(logScroll, params);
        return card;
    }

    /* ── 上面那些卡片和控件的样子 ── */

    private LinearLayout card() {
        LinearLayout card = new LinearLayout(this);
        card.setOrientation(LinearLayout.VERTICAL);
        card.setBackground(rounded(SURFACE, dp(16), LINE));
        card.setPadding(dp(14), dp(12), dp(14), dp(12));
        card.setElevation(dp(1));
        return card;
    }

    private TextView caption(String text) {
        TextView view = new TextView(this);
        view.setText(text);
        view.setTextSize(11.5f);
        view.setTextColor(INK_2);
        return view;
    }

    /** 圆角块:填色 + 可选 1px 描边。原型里所有面都是这个形状。 */
    private GradientDrawable rounded(int fill, float radius, int stroke) {
        GradientDrawable shape = new GradientDrawable();
        shape.setColor(fill);
        shape.setCornerRadius(radius);
        if (stroke != 0) {
            shape.setStroke(Math.max(1, dp(1) / 2), stroke);
        }
        return shape;
    }

    private Button filledButton(String text, int fill, View.OnClickListener listener) {
        Button button = baseButton(text, listener);
        button.setBackground(rounded(fill, dp(999), 0));
        button.setTextColor(0xFFFFFFFF);
        button.setPadding(dp(18), 0, dp(18), 0);
        return button;
    }

    private Button outlineButton(String text, View.OnClickListener listener) {
        Button button = baseButton(text, listener);
        button.setBackground(rounded(SURFACE, dp(999), LINE_2));
        button.setTextColor(INK);
        button.setPadding(dp(18), 0, dp(18), 0);
        return button;
    }

    /** 次要动作:没有面,只有字。删、清空、导出都属于这一类。 */
    private Button quietButton(String text, int color, View.OnClickListener listener) {
        Button button = baseButton(text, listener);
        button.setBackground(null);
        button.setTextColor(color);
        button.setTextSize(12.5f);
        button.setPadding(dp(10), 0, dp(10), 0);
        return button;
    }

    private Button stepButton(String text, View.OnClickListener listener) {
        Button button = baseButton(text, listener);
        button.setBackground(null);
        button.setTextColor(ACCENT);
        button.setTextSize(19);
        button.setMinWidth(dp(38));
        button.setPadding(0, 0, 0, 0);
        return button;
    }

    private Button baseButton(String text, View.OnClickListener listener) {
        Button button = new Button(this);
        button.setText(text);
        button.setAllCaps(false);
        button.setTextSize(14);
        button.setMinWidth(0);
        button.setMinHeight(dp(42));
        button.setMinimumWidth(0);
        button.setMinimumHeight(dp(42));
        button.setStateListAnimator(null);
        button.setOnClickListener(listener);
        return button;
    }

    /** 卡片之间统一的间距。 */
    private LinearLayout.LayoutParams stretch() {
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT);
        params.topMargin = dp(10);
        return params;
    }

    private void setAddress(int value) {
        if (value < 0 || value > 15) {
            return;
        }
        address = value;
        addressView.setText(String.valueOf(address));
        store.saveAddress(address);
        sendCommand(String.format(Locale.ROOT, "CALGET,ADDR=%d", address));
    }


    /* ══════════ 收帧 ══════════ */

    private void onLine(String line) {
        if (line == null || line.isEmpty()) {
            return;
        }

        Matcher cal = CAL_PATTERN.matcher(line);
        if (cal.matches()) {
            if (Integer.parseInt(cal.group(1)) != address) {
                return;
            }
            lastRms = Double.parseDouble(cal.group(2));
            lastFlag = cal.group(4);
            lastSource = cal.group(5) == null ? "?" : cal.group(5);
            lastFrameAt = System.currentTimeMillis();
            recentRms.addLast(lastRms);
            while (recentRms.size() > SPREAD_WINDOW) {
                recentRms.removeFirst();
            }
            maybeRepush();
            refreshLive();
            return;
        }

        Matcher test = TEST_PATTERN.matcher(line);
        if (test.matches()) {
            if (Integer.parseInt(test.group(1)) == address) {
                lastMa = Integer.parseInt(test.group(2));
                refreshLive();
            }
            return;
        }

        Matcher ack = ACK_PATTERN.matcher(line);
        if (ack.matches()) {
            onAck(Integer.parseInt(ack.group(1)), Integer.parseInt(ack.group(2)),
                    ack.group(3));
            return;
        }

        Matcher stat = STAT_PATTERN.matcher(line);
        if (stat.matches()) {
            onStatus(Integer.parseInt(stat.group(1)), stat.group(2),
                    Integer.parseInt(stat.group(3)), stat.group(4));
        }
    }

    private void onAck(int addr, int index, String error) {
        if (addr != address || pushingIndex < 0 || index != pushingIndex) {
            return;
        }
        if (error != null) {
            appendLog("✗ 点 " + index + " 被拒:" + error);
            abortPush();
            return;
        }
        handler.removeCallbacks(ackTimeout);
        pushingIndex++;
        pushRetries = 0;
        if (pushingIndex >= pushOrder.size()) {
            pushingIndex = -1;
            committing = true;
            appendLog("… 全部 " + pushOrder.size() + " 点已暂存,提交中(表要写 flash)");
            sendCurrentPushStep();
            handler.postDelayed(ackTimeout, COMMIT_TIMEOUT_MS);
            return;
        }
        sendCurrentPushStep();
        handler.postDelayed(ackTimeout, ACK_TIMEOUT_MS);
    }

    private void onStatus(int addr, String source, int count, String error) {
        if (addr != address) {
            return;
        }
        lastSource = source;
        if (committing) {
            committing = false;
            handler.removeCallbacks(ackTimeout);
            if (error != null) {
                appendLog("✗ 提交失败:" + error);
                // 表可能其实已经装上了:一次成功的 CALEND 会清掉暂存,所以
                // 应答丢了之后的那次重发必然报 MISSING。问一句它现在用的是
                // 哪张,比按这条应答下结论准。
                handler.postDelayed(() -> sendCommand(
                        String.format(Locale.ROOT, "CALGET,ADDR=%d", address)), 400L);
            } else if ("FIELD".equals(source)) {
                appendLog("✓ 表内已是现场表," + count + " 点,已写入 flash");
                store.markPushed(true);
                repushAttempts = 0;
            } else {
                appendLog("✗ 提交后表仍报 " + source + ",没生效");
            }
        } else {
            appendLog("· 表内:" + source + "," + count + " 点"
                    + (error == null ? "" : ",ERR=" + error));
            // 上一次提交的应答丢了、但表其实装上了,就是这条查询来兜的。
            // 重启之后 pushOrder 是空的,拿点数比——认不出是不是"我们这张",
            // 但这个标志本来也只是给自动补推用的。
            int mine = pushOrder.isEmpty() ? points.size() : pushOrder.size();
            if ("FIELD".equals(source) && count == mine) {
                store.markPushed(true);
            }
        }
        refreshLive();
    }

    /**
     * 表说它在用出厂表,而这张现场表本该是装好的——补推一次。
     *
     * <p>表内表在 flash 里,掉电不会丢,所以这条路只在几种病态情况下走得到:
     * {@code CALEND} 的应答丢了而提交其实没做成、flash 记录坏了、或者别人对
     * 这台表发过 {@code CALOFF}。补推能治第一种,治不了后两种。
     *
     * <p>所以它连着补两次就停,不再无限重试:flash 真坏了的话,每 10 秒推一遍
     * 只是一遍遍写同一块坏区,还把日志刷满,让真正该看见的那行被顶出去。停下来
     * 之后状态牌一直是"出厂表",那才是该看见的东西。
     */
    private void maybeRepush() {
        if (!"ROM".equals(lastSource) || !store.pushed()) {
            return;
        }
        if (points.size() < 2 || pushingIndex >= 0 || committing) {
            return;
        }
        if (repushAttempts >= MAX_REPUSH_ATTEMPTS) {
            return;
        }
        long now = System.currentTimeMillis();
        if (now - lastRepushAt < REPUSH_COOLDOWN_MS) {
            return;
        }
        lastRepushAt = now;
        repushAttempts++;
        appendLog("! 表在用出厂表,但这张本该装好的——补推(第 "
                + repushAttempts + "/" + MAX_REPUSH_ATTEMPTS + " 次)");
        startPush();
        if (repushAttempts >= MAX_REPUSH_ATTEMPTS) {
            appendLog("  这次再不成就不自动推了,要推自己按");
        }
    }

    /* ══════════ 记点 ══════════ */

    /**
     * 记下当前这一帧的 RMS 和填进去的读数。原样存,一条不拦。
     *
     * <p>只有两件事会让它记不下来:一帧都还没收到(没有 RMS 可记),和输入框里
     * 不是一个数(没有读数可记)。表报什么 FLAG、读数稳不稳、和已有的点挨多近、
     * 记下来的顺序是不是递增,都不在这儿判——那些是操作的人看着办的事,不是
     * 界面该替他决定的。存下来的点列表里什么都看得见,不合适随时删。
     */
    private void capturePoint() {
        if (points.size() >= MAX_CAPTURED) {
            toast("最多记 " + MAX_CAPTURED + " 个点");
            return;
        }
        if (Double.isNaN(lastRms)) {
            toast("还没收到帧,没有 RMS 可记");
            return;
        }
        double reference;
        try {
            reference = Double.parseDouble(referenceInput.getText().toString().trim());
        } catch (NumberFormatException ignored) {
            toast("先填标定表读数,单位 A");
            return;
        }

        points.add(new double[]{lastRms, reference});
        store.savePoints(points);
        store.markPushed(false);
        referenceInput.setText("");
        appendLog(String.format(Locale.CHINA, "+ 点 %d:RMS %.2f → %.4f A%s",
                points.size(), lastRms, reference,
                "OK".equals(lastFlag) ? "" : "  (FLAG=" + lastFlag + ")"));
        refreshPoints();
        refreshLive();
    }

    private void dropLastPoint() {
        if (points.isEmpty()) {
            return;
        }
        points.remove(points.size() - 1);
        store.savePoints(points);
        store.markPushed(false);
        refreshPoints();
        refreshLive();
    }

    private void clearPoints() {
        points.clear();
        store.savePoints(points);
        store.markPushed(false);
        refreshPoints();
        refreshLive();
        appendLog("· 手机上的点已清空(表内那张没动)");
    }

    /* ══════════ 推送 ══════════ */

    private void startPush() {
        if (pushingIndex >= 0 || committing) {
            toast("正在推送");
            return;
        }
        pushOrder = pushTable();
        if (pushOrder.size() < 2) {
            toast("至少要 2 个 RMS 不同的点");
            appendLog("✗ 不能推:去掉 RMS 重复的之后不足 2 点");
            return;
        }
        pushingIndex = 0;
        pushRetries = 0;
        int dropped = points.size() - pushOrder.size();
        appendLog("→ 推送 " + pushOrder.size() + " 点到 " + address + " 号表"
                + (dropped == 0 ? "" : "(排序去重取单调,挑掉 " + dropped + " 个)"));
        sendCurrentPushStep();
        handler.postDelayed(ackTimeout, ACK_TIMEOUT_MS);
    }

    /**
     * 推送用的那一份。记进来的点一个不改,这里只挑。
     *
     * <p>三步,每一步只解决一件表那边做不了的事:
     *
     * <ol>
     * <li><b>按 RMS 稳定排序</b>。{@code lsb_to_amps} 拿 RMS 找落在哪一段,RMS
     *     不单调,"哪一段"就没有定义。记的顺序不必是升的,排一次就是。
     * <li><b>RMS 相同的只留后记的</b>。同一个 RMS 上有两个点,中间那一段跨度是
     *     零;而在同一个读数上重记一次,本来就是"刚才那个不算"的意思。
     * <li><b>取电流那一列的最长递增子序列</b>。负载动完到前端跟上要几秒,这段
     *     里记的点,参考表已经走了而表还没走——它描述的是两个不同的电流,任何
     *     单调的表都容纳不下。LIS 挑掉的正是这些,而不是"抖得厉害"的点:抖动
     *     不破坏单调,它留得下来。
     * </ol>
     *
     * <p>最后如果还多于表能装的,按"去掉它对折线影响最小"逐个减,而不是等距抽。
     */
    private List<double[]> pushTable() {
        List<double[]> sorted = new ArrayList<>(points);
        // 稳定排序:RMS 相同的一串仍按记录先后排,所以去重留下的是后记的那个。
        Collections.sort(sorted, (a, b) -> Double.compare(a[0], b[0]));

        List<double[]> unique = new ArrayList<>();
        for (double[] point : sorted) {
            if (!unique.isEmpty() && unique.get(unique.size() - 1)[0] == point[0]) {
                unique.set(unique.size() - 1, point);
                continue;
            }
            unique.add(point);
        }

        List<double[]> rising = longestRising(unique);
        while (rising.size() > MAX_POINTS) {
            rising.remove(leastMissed(rising));
        }
        return rising;
    }

    /** 电流那一列的最长严格递增子序列。n 最多几十,平方的写法够用也看得懂。 */
    private List<double[]> longestRising(List<double[]> sorted) {
        int n = sorted.size();
        if (n < 2) {
            return new ArrayList<>(sorted);
        }
        int[] length = new int[n];
        int[] from = new int[n];
        int best = 0;
        for (int i = 0; i < n; i++) {
            length[i] = 1;
            from[i] = -1;
            for (int j = 0; j < i; j++) {
                if (sorted.get(j)[1] < sorted.get(i)[1] && length[j] + 1 > length[i]) {
                    length[i] = length[j] + 1;
                    from[i] = j;
                }
            }
            if (length[i] > length[best]) {
                best = i;
            }
        }
        List<double[]> out = new ArrayList<>();
        for (int i = best; i >= 0; i = from[i]) {
            out.add(sorted.get(i));
        }
        Collections.reverse(out);
        return out;
    }

    /**
     * 去掉哪一个点,对这条折线的改动最小(按相对误差算)。两端不动:它们定的是
     * 表的量程,少一个就要靠外推补。
     */
    private int leastMissed(List<double[]> table) {
        int worst = 1;
        double least = Double.MAX_VALUE;
        for (int i = 1; i < table.size() - 1; i++) {
            double[] a = table.get(i - 1);
            double[] b = table.get(i);
            double[] c = table.get(i + 1);
            double chord = a[1] + (c[1] - a[1]) * (b[0] - a[0]) / (c[0] - a[0]);
            double missed = b[1] == 0 ? 0 : Math.abs(chord - b[1]) / Math.abs(b[1]);
            if (missed < least) {
                least = missed;
                worst = i;
            }
        }
        return worst;
    }

    /** 推送状态机当前该发哪一条:还在发点就发点,点发完了就发 CALEND。 */
    private void sendCurrentPushStep() {
        if (committing) {
            sendCommand(String.format(Locale.ROOT, "CALEND,ADDR=%d,N=%d",
                    address, pushOrder.size()));
            return;
        }
        if (pushingIndex < 0 || pushingIndex >= pushOrder.size()) {
            return;
        }
        double[] point = pushOrder.get(pushingIndex);
        sendCommand(String.format(Locale.ROOT, "CALPT,ADDR=%d,I=%d,X=%.2f,Y=%.4f",
                address, pushingIndex, point[0], point[1]));
    }

    private void abortPush() {
        handler.removeCallbacks(ackTimeout);
        pushingIndex = -1;
        committing = false;
        refreshLive();
    }

    /**
     * 切回出厂表。
     *
     * <p>先落下"不要再自动推了"这个意思,再发命令。顺序反过来的话,{@code CALOFF}
     * 生效后的第一帧就带着 {@code SRC=ROM} 回来,正好是自动重推等的那个条件——
     * 于是刚切回去的表在一两秒内被推了回来,而界面上看不出发生过什么。
     *
     * <p>手机上的点不动:切回来只要按「推送并启用」,不用重新量一遍。
     */
    private void clearMeterTable() {
        store.markPushed(false);
        repushAttempts = 0;
        sendCommand(String.format(Locale.ROOT, "CALOFF,ADDR=%d", address));
        appendLog("→ 切回出厂表(手机上的 " + points.size() + " 个点还在,可随时推回去)");
    }

    private void sendCommand(String line) {
        Intent intent = new Intent(this, MeterPollingService.class);
        intent.setAction(MeterPollingService.ACTION_SEND_LINE);
        intent.putExtra(MeterPollingService.EXTRA_LINE, line);
        startService(intent);
    }

    /* ══════════ 显示 ══════════ */

    private void refreshLive() {
        if (rmsView == null) {
            return;
        }
        boolean fresh = System.currentTimeMillis() - lastFrameAt < 6_000L
                && !Double.isNaN(lastRms);

        rmsView.setText(fresh ? String.format(Locale.CHINA, "%.2f", lastRms) : "--.--");
        rmsView.setTextColor(fresh ? INK : MUTED);
        rmsHint.setText(fresh && recentRms.size() >= SPREAD_WINDOW
                // 只报,不判。抖多少是负载和前端的事,记不记这个点是人的事。
                ? String.format(Locale.CHINA, "LSB · 近%d帧 ±%.2f%%",
                        SPREAD_WINDOW, spread() * 100)
                : fresh ? "LSB" : "等 " + address + " 号表的帧");

        StringBuilder text = new StringBuilder();
        if (fresh) {
            text.append(String.format(Locale.CHINA, "表读 %.3f A", lastMa / 1000.0));
            if (!"OK".equals(lastFlag) && !lastFlag.isEmpty()) {
                text.append("   FLAG=").append(lastFlag);
            }
        } else {
            text.append("表读 ——");
        }
        text.append('\n');
        if (pushingIndex >= 0) {
            text.append("推送中 ").append(pushingIndex + 1).append('/')
                    .append(pushOrder.size());
        } else if (committing) {
            text.append("提交中,表在写 flash…");
        } else if (!points.isEmpty() && !store.pushed()) {
            text.append("手机上 ").append(points.size()).append(" 点,未推");
        } else {
            text.append("手机上 ").append(points.size()).append(" 点");
        }
        liveView.setText(text.toString());

        // 这一行说的是表此刻用哪张表出数,不是手机上有什么——它和 OLED 上的
        // 读数是同一张表算出来的,现场标定改的就是它。
        boolean field = "FIELD".equals(lastSource);
        boolean known = field || "ROM".equals(lastSource);
        sourcePill.setText(field ? "表内 · 现场表"
                : "ROM".equals(lastSource) ? "表内 · 出厂表 CAL_X1" : "表内 · 未知");
        sourcePill.setTextColor(field ? 0xFFFFFFFF : known ? INK_2 : MUTED);
        sourcePill.setBackground(rounded(
                field ? ACCENT : SUNKEN, dp(999), field ? 0 : LINE_2));
    }

    private void refreshPoints() {
        if (pointsView == null) {
            return;
        }
        if (points.isEmpty()) {
            pointsCount.setText("");
            pointsView.setText("还没有点。调好负载,填标定表读数,按「记录此点」。");
            pointsView.setTextColor(MUTED);
            return;
        }
        pointsView.setTextColor(INK);
        // 按记录顺序列,不排序:记进来什么样就是什么样,乱序也照列。推送时才
        // 排序取单调,那一份在这里用「挑掉」标出来。
        List<double[]> keep = pushTable();
        pointsCount.setText(points.size() == keep.size()
                ? points.size() + " 点"
                : points.size() + " 记 · " + keep.size() + " 推");
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < points.size(); i++) {
            double[] point = points.get(i);
            boolean kept = false;
            for (double[] other : keep) {
                if (other == point) {
                    kept = true;
                    break;
                }
            }
            text.append(String.format(Locale.CHINA, "%s %2d  %8.2f  %7.4f A\n",
                    kept ? " " : "×", i + 1, point[0], point[1]));
        }
        if (keep.size() < points.size()) {
            text.append("×  = 推送时挑掉(与单调冲突或 RMS 重复)");
        }
        // 只去尾部换行:首行那个空格是对齐用的,trim() 会把它吃掉,整列错一格。
        while (text.length() > 0 && text.charAt(text.length() - 1) == '\n') {
            text.setLength(text.length() - 1);
        }
        pointsView.setText(text);
    }

    /** 导成 cal.rs 能直接贴的字面量:现场标好的一张表,值得带回台上固化。 */
    private void exportTable() {
        if (points.isEmpty()) {
            toast("没有点可导");
            return;
        }
        StringBuilder text = new StringBuilder("pub static CAL_X1: &[(f32, f32)] = &[\n");
        for (double[] point : points) {
            text.append(String.format(Locale.ROOT, "    (%.2f, %.4f),\n",
                    point[0], point[1]));
        }
        text.append("];\n");
        ClipboardManager clipboard =
                (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboard != null) {
            clipboard.setPrimaryClip(ClipData.newPlainText("CAL_X1", text.toString()));
            toast("已复制到剪贴板");
        }
        appendLog(text.toString());
    }

    private double spread() {
        if (recentRms.isEmpty()) {
            return Double.MAX_VALUE;
        }
        double low = Double.MAX_VALUE;
        double high = -Double.MAX_VALUE;
        double sum = 0;
        for (double value : recentRms) {
            low = Math.min(low, value);
            high = Math.max(high, value);
            sum += value;
        }
        double mean = sum / recentRms.size();
        return mean <= 0 ? Double.MAX_VALUE : (high - low) / mean;
    }


    private void appendLog(String line) {
        // 连着来的同一句合成一行加计数。没连上表的时候服务每 1.5 秒说一次
        // 「没有已连接的电流表」,不合并的话真正有用的那几行会被顶出去。
        if (line.equals(lastLogLine)) {
            repeatedLogLines++;
            int cut = log.lastIndexOf("\n", log.length() - 2);
            log.setLength(cut < 0 ? 0 : cut + 1);
            log.append(line).append("  ×").append(repeatedLogLines + 1).append('\n');
            if (logView != null) {
                logView.setText(log.toString().trim());
            }
            return;
        }
        lastLogLine = line;
        repeatedLogLines = 0;
        log.append(line).append('\n');
        // 只留最近 40 行:这是个操作日志,不是记录。
        int lines = 0;
        int cut = log.length();
        for (int i = log.length() - 1; i >= 0; i--) {
            if (log.charAt(i) == '\n' && ++lines > 40) {
                cut = i + 1;
                break;
            }
        }
        if (cut > 0 && cut < log.length()) {
            log.delete(0, cut);
        }
        if (logView != null) {
            logView.setText(log.toString().trim());
        }
        if (logScroll != null) {
            // scrollTo,不是 fullScroll:后者会把焦点也一起要过来,于是每来一行
            // 日志,正在打字的输入框就被抢一次。
            logScroll.post(() -> logScroll.scrollTo(0, Math.max(0,
                    logView.getHeight() - logScroll.getHeight())));
        }
    }

    /* ══════════ 杂项 ══════════ */

    private void startReader(String action) {
        Intent intent = new Intent(this, MeterPollingService.class);
        intent.setAction(action);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            startForegroundService(intent);
        } else {
            startService(intent);
        }
    }

    private void registerLineReceiver() {
        IntentFilter filter = new IntentFilter();
        filter.addAction(MeterPollingService.ACTION_LINE);
        filter.addAction(MeterPollingService.ACTION_STATE);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(receiver, filter);
        }
    }

    private void ensurePermissions(Runnable action) {
        List<String> missing = new ArrayList<>();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            addIfMissing(missing, Manifest.permission.BLUETOOTH_SCAN);
            addIfMissing(missing, Manifest.permission.BLUETOOTH_CONNECT);
        } else {
            addIfMissing(missing, Manifest.permission.ACCESS_FINE_LOCATION);
        }
        if (missing.isEmpty()) {
            action.run();
            return;
        }
        requestPermissions(missing.toArray(new String[0]), REQUEST_PERMISSIONS);
    }

    private void addIfMissing(List<String> missing, String permission) {
        if (checkSelfPermission(permission) != PackageManager.PERMISSION_GRANTED) {
            missing.add(permission);
        }
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions,
                                           int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        if (requestCode != REQUEST_PERMISSIONS) {
            return;
        }
        boolean granted = grantResults.length > 0;
        for (int result : grantResults) {
            granted &= result == PackageManager.PERMISSION_GRANTED;
        }
        if (granted) {
            startReader(MeterPollingService.ACTION_CONNECT_BASIC);
        } else {
            appendLog("✗ 没有附近设备权限,连不上 HC-42");
        }
    }







    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    private void toast(String text) {
        Toast.makeText(this, text, Toast.LENGTH_SHORT).show();
    }
}
