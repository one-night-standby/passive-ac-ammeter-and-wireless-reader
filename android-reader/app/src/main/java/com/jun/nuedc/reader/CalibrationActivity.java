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
import android.graphics.Color;
import android.graphics.Typeface;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
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
 * <p>点数上限 16,而不是十。决定一张折线表能不能压进 0.5% 的是节点<b>放在哪</b>,
 * 不是有几个:拿台上那次扫描拟出的光滑曲线做基准,10 个节点按 log(RMS) 等比放,
 * 最坏弦误差 0.49%,正好是全部预算;同样 10 个点按"0.1/0.2/0.4…"这种整数电流放,
 * 是 1.5%——灵敏度在 0.3 A 附近拐得最厉害,那儿没有节点就跟不上。所以这个界面
 * 报的靶位是按 RMS 等比排的,不是按电流。
 *
 * <p>它是独立的一个图标,和读表器界面分开:自动模式要"一键启动",那个界面上不
 * 该多一个能把表改坏的按钮。两边共用 {@link MeterPollingService} 的那条 BLE 链。
 */
public final class CalibrationActivity extends Activity {
    private static final int REQUEST_PERMISSIONS = 43;

    /** 表内表的点数上限,必须和固件的 {@code cal::FIELD_MAX} 一致。 */
    private static final int MAX_POINTS = 16;

    /**
     * 靶位的两端,单位 LSB。取自台上那次扫描的实际跨度(0.115 A 到 2.38 A),
     * 也就是固件里 CAL_X1 的首尾——现场没必要、也没条件走得比它更宽。
     */
    private static final double SPAN_LO_LSB = 13.5;
    private static final double SPAN_HI_LSB = 1093.0;

    /** 认为一个靶位"已经有点了"的半径。比它更近的两个点,分段斜率就是噪声。 */
    private static final double TARGET_TOLERANCE = 0.15;

    /** 判稳用的窗口:连续这么多帧的 RMS 落在 {@link #SETTLE_SPREAD} 之内才让记点。 */
    private static final int SETTLE_WINDOW = 5;
    private static final double SETTLE_SPREAD = 0.005;

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

    /** 看到 SRC=ROM 之后,最快隔多久自动重推一次。 */
    private static final long REPUSH_COOLDOWN_MS = 10_000L;

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
    private final StringBuilder log = new StringBuilder();

    private CalibrationStore store;
    private int address = 6;
    private int targetCount = 10;

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

    private TextView liveView;
    private TextView pointsView;
    private TextView logView;
    private TextView addressView;
    private TextView targetCountView;
    private EditText referenceInput;
    private CheckBox autoRepush;
    private ScrollView scroller;

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
        targetCount = store.targetCount();
        points.addAll(store.points());
        setContentView(buildUi());
        refreshPoints();
        refreshLive();
        registerLineReceiver();
        ensurePermissions(() -> {
            startReader(MeterPollingService.ACTION_CONNECT_BASIC);
            sendCommand(String.format(Locale.ROOT, "CALGET,ADDR=%d", address));
        });
        appendLog("标定台就绪。靶位按 log(RMS) 等比排,不是按整数电流。");
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

    private View buildUi() {
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        int pad = dp(12);
        root.setPadding(pad, pad, pad, pad);

        LinearLayout header = new LinearLayout(this);
        header.setOrientation(LinearLayout.HORIZONTAL);
        header.setGravity(Gravity.CENTER_VERTICAL);
        header.addView(label("表地址"));
        header.addView(flatButton("−", v -> setAddress(address - 1)));
        addressView = mono(String.valueOf(address), 20);
        addressView.setPadding(dp(10), 0, dp(10), 0);
        header.addView(addressView);
        header.addView(flatButton("+", v -> setAddress(address + 1)));
        header.addView(spacer());
        header.addView(label("靶位数"));
        header.addView(flatButton("−", v -> setTargetCount(targetCount - 1)));
        targetCountView = mono(String.valueOf(targetCount), 20);
        targetCountView.setPadding(dp(10), 0, dp(10), 0);
        header.addView(targetCountView);
        header.addView(flatButton("+", v -> setTargetCount(targetCount + 1)));
        root.addView(header);

        liveView = mono("", 15);
        liveView.setPadding(dp(10), dp(10), dp(10), dp(10));
        liveView.setBackgroundColor(0xFFF1F4F6);
        root.addView(liveView, wide());

        LinearLayout capture = new LinearLayout(this);
        capture.setOrientation(LinearLayout.HORIZONTAL);
        capture.setGravity(Gravity.CENTER_VERTICAL);
        capture.addView(label("标定表读数 A"));
        referenceInput = new EditText(this);
        referenceInput.setInputType(
                InputType.TYPE_CLASS_NUMBER | InputType.TYPE_NUMBER_FLAG_DECIMAL);
        referenceInput.setHint("1.2345");
        referenceInput.setTypeface(Typeface.MONOSPACE);
        capture.addView(referenceInput, new LinearLayout.LayoutParams(0,
                ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
        capture.addView(flatButton("记录此点", v -> capturePoint()));
        root.addView(capture, wide());

        pointsView = mono("", 13);
        pointsView.setPadding(dp(10), dp(6), dp(10), dp(6));
        root.addView(pointsView, wide());

        LinearLayout row1 = new LinearLayout(this);
        row1.setOrientation(LinearLayout.HORIZONTAL);
        row1.addView(flatButton("删最后一点", v -> dropLastPoint()));
        row1.addView(flatButton("清空点", v -> clearPoints()));
        row1.addView(flatButton("导出", v -> exportTable()));
        root.addView(row1, wide());

        // 两个按钮是一个开关的两头,不是"做一件事"和"撤销"。表里两张表随时可以
        // 换着用:出厂表一直在固件里,现场表的点一直在手机上,换过去换回来都不用
        // 重新量。当场要证明现场标定确实在起作用,来回按这两个就是。
        LinearLayout row2 = new LinearLayout(this);
        row2.setOrientation(LinearLayout.HORIZONTAL);
        row2.addView(flatButton("推送并启用", v -> startPush()));
        row2.addView(flatButton("切回出厂表", v -> clearMeterTable()));
        row2.addView(flatButton("读表内状态",
                v -> sendCommand(String.format(Locale.ROOT, "CALGET,ADDR=%d", address))));
        root.addView(row2, wide());

        autoRepush = new CheckBox(this);
        // 只在"现场表已经启用"的前提下才补推。主动切回出厂表会先清掉那个前提,
        // 所以这个勾不会把人刚切回去的表又推回来。
        autoRepush.setText("启用现场表后,掉电退回出厂表时自动补推");
        autoRepush.setChecked(true);
        root.addView(autoRepush);

        logView = mono("", 12);
        logView.setPadding(dp(10), dp(6), dp(10), dp(6));
        logView.setTextColor(0xFF4A5560);
        root.addView(logView, wide());

        scroller = new ScrollView(this);
        scroller.addView(root);
        return scroller;
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

    private void setTargetCount(int value) {
        if (value < 2 || value > MAX_POINTS) {
            return;
        }
        targetCount = value;
        targetCountView.setText(String.valueOf(targetCount));
        store.saveTargetCount(targetCount);
        refreshLive();
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
            while (recentRms.size() > SETTLE_WINDOW) {
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
        if (pushingIndex >= points.size()) {
            pushingIndex = -1;
            committing = true;
            appendLog("… 全部 " + points.size() + " 点已暂存,提交中(表要写 flash)");
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
            } else {
                appendLog("✗ 提交后表仍报 " + source + ",没生效");
            }
        } else {
            appendLog("· 表内:" + source + "," + count + " 点"
                    + (error == null ? "" : ",ERR=" + error));
            // 上一次提交的应答丢了、但表其实装上了,就是这条查询来兜的。
            if ("FIELD".equals(source) && count == points.size()) {
                store.markPushed(true);
            }
        }
        refreshLive();
    }

    /**
     * 表掉过电、现场表没了,就把手机上这张再推一遍。
     *
     * <p>这正是把表放进 flash 之后还要留的那道防线:flash 写坏、或者根本没写成
     * 的那次,表会退回出厂表并在每一帧里说 SRC=ROM——不看这个字段的话,它表现
     * 出来只是"读数差了一点"。
     */
    private void maybeRepush() {
        if (!autoRepush.isChecked() || !"ROM".equals(lastSource)) {
            return;
        }
        if (!store.pushed() || points.size() < 2 || pushingIndex >= 0 || committing) {
            return;
        }
        long now = System.currentTimeMillis();
        if (now - lastRepushAt < REPUSH_COOLDOWN_MS) {
            return;
        }
        lastRepushAt = now;
        appendLog("! 表报 SRC=ROM(应该是掉过电),自动重推");
        startPush();
    }

    /* ══════════ 记点 ══════════ */

    private void capturePoint() {
        if (points.size() >= MAX_POINTS) {
            toast("最多 " + MAX_POINTS + " 点");
            return;
        }
        if (recentRms.size() < SETTLE_WINDOW) {
            toast("还在收帧,等 " + SETTLE_WINDOW + " 帧再记");
            return;
        }
        if (spread() > SETTLE_SPREAD) {
            toast(String.format(Locale.CHINA,
                    "读数还没稳(±%.2f%%),等负载沉降", spread() * 100));
            return;
        }
        if (!"OK".equals(lastFlag)) {
            toast("表说这一帧不可信:" + lastFlag);
            return;
        }
        double reference;
        try {
            reference = Double.parseDouble(referenceInput.getText().toString().trim());
        } catch (NumberFormatException ignored) {
            toast("先填标定表读数,单位 A");
            return;
        }
        if (reference <= 0 || reference > 10) {
            toast("标定表读数不像话:" + reference);
            return;
        }

        double rms = median();
        for (double[] point : points) {
            // 两个点挨得太近,它们之间那一段的斜率就是两端的噪声,不是曲线。
            if (Math.abs(Math.log(point[0] / rms)) < Math.log(1.02)) {
                toast("和已有点差不到 2%,换一个负载再记");
                return;
            }
        }
        points.add(new double[]{rms, reference});
        Collections.sort(points, (a, b) -> Double.compare(a[0], b[0]));
        store.savePoints(points);
        store.markPushed(false);
        referenceInput.setText("");
        appendLog(String.format(Locale.CHINA, "+ 点 %d:RMS %.2f → %.4f A",
                points.size(), rms, reference));
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
        String problem = tableProblem();
        if (problem != null) {
            toast(problem);
            appendLog("✗ 不能推:" + problem);
            return;
        }
        pushingIndex = 0;
        pushRetries = 0;
        appendLog("→ 开始推送 " + points.size() + " 点到 " + address + " 号表");
        sendCurrentPushStep();
        handler.postDelayed(ackTimeout, ACK_TIMEOUT_MS);
    }

    /**
     * 推之前先在手机上判一次:表那边不合格是整张拒收,而拒收信息只有一个词。
     * 在这里判,能说清是哪两个点的问题。
     */
    private String tableProblem() {
        if (points.size() < 2) {
            return "至少要 2 个点";
        }
        if (points.size() > MAX_POINTS) {
            return "超过 " + MAX_POINTS + " 点";
        }
        for (int i = 1; i < points.size(); i++) {
            double[] previous = points.get(i - 1);
            double[] current = points.get(i);
            if (current[0] <= previous[0] || current[1] <= previous[1]) {
                return String.format(Locale.CHINA,
                        "第 %d、%d 点没有一起递增(%.2f→%.2f LSB,%.4f→%.4f A)",
                        i, i + 1, previous[0], current[0], previous[1], current[1]);
            }
        }
        return null;
    }

    private void sendCurrentPushStep() {
        if (committing) {
            sendCommand(String.format(Locale.ROOT, "CALEND,ADDR=%d,N=%d",
                    address, points.size()));
            return;
        }
        if (pushingIndex < 0 || pushingIndex >= points.size()) {
            return;
        }
        double[] point = points.get(pushingIndex);
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
        if (liveView == null) {
            return;
        }
        StringBuilder text = new StringBuilder();
        boolean fresh = System.currentTimeMillis() - lastFrameAt < 6_000L;
        if (!fresh || Double.isNaN(lastRms)) {
            text.append("等 ").append(address).append(" 号表的帧…\n");
        } else {
            text.append(String.format(Locale.CHINA, "RMS   %8.2f LSB", lastRms));
            if (recentRms.size() >= SETTLE_WINDOW) {
                boolean settled = spread() <= SETTLE_SPREAD;
                text.append(String.format(Locale.CHINA, "   %s ±%.2f%%",
                        settled ? "稳" : "动", spread() * 100));
            }
            text.append('\n');
            text.append(String.format(Locale.CHINA, "表读  %8.3f A    FLAG=%s\n",
                    lastMa / 1000.0, lastFlag));
            // 说的是表此刻用哪张表出这个数,不是手机上有什么。这一行和 OLED 上
            // 的读数是同一张表算出来的——现场标定改的就是它。
            text.append("表内  ").append("FIELD".equals(lastSource)
                    ? "现场表(手机推的)"
                    : "ROM".equals(lastSource) ? "出厂表 CAL_X1" : "未知(" + lastSource + ")");
            if (!points.isEmpty() && !store.pushed()) {
                text.append("   手机上 ").append(points.size()).append(" 点未推");
            }
            text.append('\n');
        }
        text.append(nextTargetHint());
        if (pushingIndex >= 0) {
            text.append("\n推送中:点 ").append(pushingIndex + 1)
                    .append('/').append(points.size());
        } else if (committing) {
            text.append("\n提交中(表在写 flash)…");
        }
        liveView.setText(text.toString());
        liveView.setTextColor("FIELD".equals(lastSource) ? 0xFF176B87 : 0xFF8A4B00);
    }

    /**
     * 下一个还没有点的靶位。按 log(RMS) 等比排——不是按整数电流:同样点数下,
     * 按电流排的最坏弦误差是按 RMS 排的三倍,而 0.5% 的预算经不起这个。
     */
    private String nextTargetHint() {
        double[] targets = targets();
        for (double target : targets) {
            boolean covered = false;
            for (double[] point : points) {
                if (Math.abs(Math.log(point[0] / target)) < Math.log(1 + TARGET_TOLERANCE)) {
                    covered = true;
                    break;
                }
            }
            if (covered) {
                continue;
            }
            String nudge = "";
            if (!Double.isNaN(lastRms)) {
                nudge = lastRms > target * (1 + TARGET_TOLERANCE) ? "(调小负载)"
                        : lastRms < target / (1 + TARGET_TOLERANCE) ? "(调大负载)"
                        : "(就在这儿,记点)";
            }
            return String.format(Locale.CHINA, "下一靶位 RMS≈%.0f %s   已记 %d/%d",
                    target, nudge, points.size(), targets.length);
        }
        return String.format(Locale.CHINA, "靶位已铺满,已记 %d 点", points.size());
    }

    private double[] targets() {
        double[] targets = new double[targetCount];
        double ratio = Math.log(SPAN_HI_LSB / SPAN_LO_LSB) / (targetCount - 1);
        for (int i = 0; i < targetCount; i++) {
            targets[i] = SPAN_LO_LSB * Math.exp(ratio * i);
        }
        return targets;
    }

    private void refreshPoints() {
        if (pointsView == null) {
            return;
        }
        if (points.isEmpty()) {
            pointsView.setText("(还没有点)");
            return;
        }
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < points.size(); i++) {
            double[] point = points.get(i);
            text.append(String.format(Locale.CHINA, "%2d  %8.2f LSB  %7.4f A",
                    i + 1, point[0], point[1]));
            if (i > 0) {
                double[] previous = points.get(i - 1);
                double slope = (point[1] - previous[1]) / (point[0] - previous[0]);
                text.append(String.format(Locale.CHINA, "   %6.2f mA/LSB", slope * 1000));
            }
            text.append('\n');
        }
        pointsView.setText(text.toString().trim());
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

    private double median() {
        double[] sorted = new double[recentRms.size()];
        int i = 0;
        for (double value : recentRms) {
            sorted[i++] = value;
        }
        Arrays.sort(sorted);
        return sorted[sorted.length / 2];
    }

    private void appendLog(String line) {
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
        if (scroller != null) {
            scroller.post(() -> scroller.fullScroll(View.FOCUS_DOWN));
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

    private TextView mono(String text, int sizeSp) {
        TextView view = new TextView(this);
        view.setText(text);
        view.setTypeface(Typeface.MONOSPACE);
        view.setTextSize(sizeSp);
        view.setTextColor(Color.parseColor("#182027"));
        return view;
    }

    private TextView label(String text) {
        TextView view = new TextView(this);
        view.setText(text);
        view.setTextSize(13);
        view.setPadding(0, 0, dp(6), 0);
        return view;
    }

    private Button flatButton(String text, View.OnClickListener listener) {
        Button button = new Button(this);
        button.setText(text);
        button.setAllCaps(false);
        button.setTextSize(14);
        button.setOnClickListener(listener);
        return button;
    }

    private View spacer() {
        View view = new View(this);
        view.setLayoutParams(new LinearLayout.LayoutParams(dp(16), 1));
        return view;
    }

    private LinearLayout.LayoutParams wide() {
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT);
        params.topMargin = dp(8);
        return params;
    }

    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }

    private void toast(String text) {
        Toast.makeText(this, text, Toast.LENGTH_SHORT).show();
    }
}
