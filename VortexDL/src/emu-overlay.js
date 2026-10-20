// 模拟器悬浮功能条 (2026-10-19，2026-10-20 重写成完整控制中心)
//
// 设计要点：
//   · 它浮在游戏画面上，所以配色固定深色（不跟随软件主题），保证任何主题下都可读
//   · 顶部条只有把手 + 实时 FPS + 5 个按钮；详细信息在展开面板里
//   · 面板分 4 页：性能 / 模拟器 / 输入 / 帧率记录
//   · 所有 invoke 都做了兜底 —— 某个命令不存在时不能把整个条搞崩
const invoke = (cmd, args) => {
    const t = window.__TAURI__;
    if (t && t.core && t.core.invoke) return t.core.invoke(cmd, args);
    if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
        return window.__TAURI_INTERNALS__.invoke(cmd, args);
    }
    return Promise.reject('TAURI 未就绪');
};

const $ = (id) => document.getElementById(id);
let settings = null;
let recSettings = null;
let liveTimer = null;
let aliveTimer = null;
let recording = false;

const VK_NAMES = {
    0x08: 'Backspace', 0x09: 'Tab', 0x0D: 'Enter', 0x1B: 'Esc', 0x20: 'Space',
    0x25: '←', 0x26: '↑', 0x27: '→', 0x28: '↓',
    0x24: 'Home', 0x2C: 'PrtSc', 0x5B: 'Win',
    0x10: 'Shift', 0x11: 'Ctrl', 0x12: 'Alt',
};
function vkName(vk) {
    if (VK_NAMES[vk]) return VK_NAMES[vk];
    if (vk >= 0x30 && vk <= 0x39) return String.fromCharCode(vk);
    if (vk >= 0x41 && vk <= 0x5A) return String.fromCharCode(vk);
    if (vk >= 0x60 && vk <= 0x69) return 'Num' + (vk - 0x60);
    if (vk >= 0x70 && vk <= 0x87) return 'F' + (vk - 0x6F);
    return '0x' + vk.toString(16).toUpperCase();
}

// ---------------- 标签页 ----------------
document.querySelectorAll('.ptab').forEach((b) => {
    b.addEventListener('click', () => {
        document.querySelectorAll('.ptab').forEach((x) => x.classList.remove('active'));
        document.querySelectorAll('.ppanel').forEach((x) => x.classList.remove('active'));
        b.classList.add('active');
        const p = document.querySelector('.ppanel[data-panel="' + b.dataset.tab + '"]');
        if (p) p.classList.add('active');
        resizeToPanel();
    });
});

function resizeToPanel() {
    const p = $('panel');
    if (!p || p.hidden) {
        invoke('emu_overlay_resize', { height: 40 }).catch(() => {});
        return;
    }
    // 等 DOM 排版完再量
    requestAnimationFrame(() => {
        const h = Math.min(520, 44 + p.scrollHeight + 10);
        invoke('emu_overlay_resize', { height: h }).catch(() => {});
    });
}

// ---------------- 顶部条动作 ----------------
$('ov-max')?.addEventListener('click', () => invoke('emu_toggle_max').catch(() => {}));
$('ov-full')?.addEventListener('click', () => invoke('emu_toggle_fullscreen').catch(() => {}));
$('ov-close')?.addEventListener('click', async () => {
    try { await invoke('emu_stop'); } catch (_) {}
    try { await invoke('emu_overlay_close'); } catch (_) {}
});
$('ov-settings')?.addEventListener('click', () => {
    const p = $('panel');
    if (!p) return;
    p.hidden = !p.hidden;
    resizeToPanel();
});

// 记录开关（顶部条上的 ⏺）
$('ov-record')?.addEventListener('click', async () => {
    await toggleRecord();
});

$('ov-drag')?.addEventListener('mousedown', () => {
    // 交给系统拖拽（窗口用 startDragging）
    invoke('plugin:window|start_dragging').catch(() => {
        const t = window.__TAURI__;
        if (t && t.window && t.window.appWindow) t.window.appWindow.startDragging().catch(() => {});
    });
});

// ---------------- 性能页 ----------------
function fmt(v, digits) {
    if (v == null || !isFinite(v) || v <= 0) return '—';
    return v.toFixed(digits == null ? 0 : digits);
}
function setText(id, s) {
    const el = $(id);
    if (el) el.textContent = s;
}

function renderLive(s) {
    if (!s) return;
    const fps = s.fps > 0 ? s.fps : 0;
    const liveEl = $('ov-live');
    if (liveEl) {
        liveEl.textContent = fps > 0 ? Math.round(fps) + ' FPS' : '— FPS';
        liveEl.classList.toggle('ok', fps >= 55);
    }
    setText('m-fps', fps > 0 ? Math.round(fps) + '' : '—');
    setText('m-ft', s.frametime_ms > 0 ? s.frametime_ms.toFixed(1) + ' ms' : '—');
    setText('m-cpu', fmt(s.cpu_total, 0) + (s.cpu_total > 0 ? '%' : ''));
    setText('m-gpu', fmt(s.gpu_usage, 0) + (s.gpu_usage > 0 ? '%' : ''));
    setText('m-ram', s.ram_used > 0 ? (s.ram_used / 1024).toFixed(1) + ' GB' : '—');
    setText('m-proc', s.proc_cpu > 0 ? s.proc_cpu.toFixed(0) + '%' : '—');

    // 每核
    const box = $('m-cores');
    if (box) {
        const cores = s.cpu_cores || [];
        if (cores.length !== box.children.length) {
            box.innerHTML = cores
                .map(() => '<div class="core-bar"><i style="height:0"></i><b>0</b></div>')
                .join('');
        }
        cores.forEach((v, i) => {
            const el = box.children[i];
            if (!el) return;
            const i2 = el.querySelector('i');
            const b = el.querySelector('b');
            if (i2) i2.style.height = Math.max(2, Math.min(100, v)) + '%';
            if (b) b.textContent = Math.round(v);
        });
    }

    // 数据来源说明（不写清楚会被当成"帧率不准"）
    const note = $('fps-note');
    if (note && !note.dataset.filled) {
        note.dataset.filled = '1';
        note.textContent =
            '帧率只显示**能实测到**的值：模拟器写入性能日志时为其逐帧真值；'
            + '拿不到就显示「—」，不编造数字。CPU/GPU/内存/每核占用始终是实时实测值。';
    }
}

// ---------------- 设置 ----------------
async function loadSettings() {
    try { settings = await invoke('emu_settings_get'); } catch (_) { settings = null; }
    if (!settings) return;
    const r = $('set-rumble'), h = $('set-hint'), hd = $('set-hide'), a = $('set-alpha');
    if (r) r.checked = !!settings.rumble;
    if (h) h.checked = !!settings.on_screen_hint;
    if (hd) hd.checked = !!settings.auto_hide;
    if (a) {
        const alpha = settings.overlay_alpha == null ? 1 : settings.overlay_alpha;
        a.value = alpha;
        setText('set-alpha-v', Math.round(alpha * 100) + '%');
        applyAlpha(alpha);
    }
}

function applyAlpha(v) {
    document.documentElement.style.setProperty('--ov-alpha', String(v));
    const bar = $('bar');
    if (bar) bar.style.opacity = String(0.35 + 0.65 * v);
}

async function pushSettings() {
    if (!settings) settings = {};
    try { await invoke('emu_settings_set', { s: settings }); } catch (_) {}
}

$('set-rumble')?.addEventListener('change', function () { if (settings) { settings.rumble = this.checked; pushSettings(); } });
$('set-hint')?.addEventListener('change', function () { if (settings) { settings.on_screen_hint = this.checked; pushSettings(); } });
$('set-hide')?.addEventListener('change', function () { if (settings) { settings.auto_hide = this.checked; pushSettings(); } });
$('set-alpha')?.addEventListener('input', function () {
    const v = Number(this.value);
    setText('set-alpha-v', Math.round(v * 100) + '%');
    applyAlpha(v);
    if (settings) { settings.overlay_alpha = v; pushSettings(); }
});

// ---------------- 模拟器页 ----------------
$('btn-preset')?.addEventListener('click', async () => {
    try {
        await invoke('emu_apply_preset');
        toast('已重新应用中文/画质预设');
    } catch (e) { toast('失败: ' + e, true); }
});
$('btn-keys')?.addEventListener('click', async () => {
    try {
        const p = await invoke('emu_pick_and_import_keys');
        toast('密钥已导入：' + p);
        refreshKeysState();
    } catch (e) {
        if (String(e).indexOf('已取消') < 0) toast('导入失败: ' + e, true);
    }
});
$('btn-folder')?.addEventListener('click', () => invoke('open_folder', { path: 'D:\\game\\switch' }).catch(() => {}));
$('btn-emu-folder')?.addEventListener('click', () => invoke('open_folder', { path: '' }).catch(async () => {
    try {
        const info = await invoke('emu_info');
        if (info && info.path) {
            const dir = String(info.path).replace(/[\\/][^\\/]+$/, '');
            await invoke('open_folder', { path: dir });
        }
    } catch (_) {}
}));
$('btn-guide')?.addEventListener('click', () => invoke('emu_open_guide').catch((e) => toast('失败: ' + e, true)));
$('btn-install')?.addEventListener('click', async () => {
    try {
        const r = await invoke('emu_ensure_bundled');
        toast(r && r.installed ? '已释出到 ' + r.dir : '内置模拟器已就绪');
    } catch (e) { toast('失败: ' + e, true); }
});
$('btn-refresh')?.addEventListener('click', async () => {
    try { await invoke('switch_downloaded'); toast('游戏列表已刷新'); } catch (_) {}
});

async function refreshKeysState() {
    const el = $('emu-keys-state');
    if (!el) return;
    try {
        const info = await invoke('emu_info');
        if (info && info.keys) {
            el.textContent = '已就绪';
            el.style.color = '#7ee787';
        } else {
            el.textContent = '未导入（官方游戏需要）';
            el.style.color = '#ffb84d';
        }
    } catch (_) {
        el.textContent = '读取失败';
    }
}

// ---------------- 帧率记录页 ----------------
async function loadRecSettings() {
    try { recSettings = await invoke('fps_settings_get'); } catch (_) { recSettings = null; }
    if (!recSettings) return;
    const m = $('fps-mode'), ia = $('fps-inline'), ga = $('fps-global'), pc = $('fps-percore');
    if (m) m.value = recSettings.mode || 'off';
    if (ia) { ia.value = recSettings.inline_ms; setText('fps-inline-v', recSettings.inline_ms + ' ms'); }
    if (ga) { ga.value = recSettings.global_ms; setText('fps-global-v', recSettings.global_ms + ' ms'); }
    if (pc) pc.checked = !!recSettings.per_core;
    await refreshRecState();
}

async function refreshRecState() {
    let active = null;
    try { active = await invoke('fps_active'); } catch (_) {}
    recording = !!active;
    const st = $('fps-state'), btn = $('fps-toggle'), rec = $('ov-record');
    if (st) {
        st.textContent = active
            ? ('正在记录：' + (active.game || '全局') + '（' + (active.mode === 'inline' ? '游戏内' : '全局') + '）')
            : '未在记录';
        st.style.color = active ? '#7ee787' : '';
    }
    if (btn) btn.textContent = active ? '停止记录' : '开始记录';
    if (rec) rec.classList.toggle('recording', !!active);
}

async function toggleRecord() {
    try {
        const active = await invoke('fps_active');
        if (active) {
            const m = await invoke('fps_stop');
            toast(m ? ('已保存记录：' + (m.sample_count || 0) + ' 个采样点') : '已停止');
        } else {
            // 有前台游戏就用它当名字
            let game = '', exe = '';
            try {
                const fg = await invoke('fps_foreground');
                if (fg) { game = fg.game || ''; exe = fg.exe || ''; }
            } catch (_) {}
            await invoke('fps_start', { game, exe, mode: 'inline' });
            toast('开始记录' + (game ? '：' + game : '（全局）'));
        }
    } catch (e) {
        toast('记录失败: ' + e, true);
    }
    await refreshRecState();
}

$('fps-toggle')?.addEventListener('click', toggleRecord);
$('fps-open')?.addEventListener('click', () => {
    invoke('focus_main_page', { page: 'fps' }).catch(() => {
        // 兜底：让主窗口自己导航
        invoke('emit_to_main', { event: 'vx-navigate', payload: 'fps' }).catch(() => {});
    });
});
$('fps-mode')?.addEventListener('change', function () {
    if (!recSettings) recSettings = {};
    recSettings.mode = this.value;
    invoke('fps_settings_set', { s: recSettings }).catch(() => {});
    toast(this.value === 'off' ? '自动记录已关闭' : ('已开启：' + this.selectedOptions[0].text));
    setTimeout(refreshRecState, 400);
});
$('fps-inline')?.addEventListener('input', function () {
    setText('fps-inline-v', this.value + ' ms');
    if (recSettings) { recSettings.inline_ms = Number(this.value); }
});
$('fps-inline')?.addEventListener('change', function () {
    if (recSettings) invoke('fps_settings_set', { s: recSettings }).catch(() => {});
});
$('fps-global')?.addEventListener('input', function () {
    setText('fps-global-v', this.value + ' ms');
    if (recSettings) { recSettings.global_ms = Number(this.value); }
});
$('fps-global')?.addEventListener('change', function () {
    if (recSettings) invoke('fps_settings_set', { s: recSettings }).catch(() => {});
});
$('fps-percore')?.addEventListener('change', function () {
    if (recSettings) { recSettings.per_core = this.checked; invoke('fps_settings_set', { s: recSettings }).catch(() => {}); }
});

// ---------------- 键位 ----------------
let binds = {};
let waitingFor = '';

async function loadBinds() {
    try { binds = await invoke('emu_binds_get'); } catch (_) { binds = {}; }
    renderBinds();
}

function renderBinds() {
    const box = $('binds-list');
    if (!box) return;
    const order = ['A','B','X','Y','L','R','ZL','ZR','PLUS','MINUS','UP','DOWN','LEFT','RIGHT',
                   'LSTICK_UP','LSTICK_DOWN','LSTICK_LEFT','LSTICK_RIGHT','HOME','CAPTURE'];
    box.innerHTML = order.filter((k) => binds[k] != null).map((k) =>
        '<div class="bind-row"><span>' + k + '</span>'
        + '<span class="bind-key" data-bind="' + k + '">' + vkName(binds[k]) + '</span></div>'
    ).join('');
    box.querySelectorAll('[data-bind]').forEach((el) => {
        el.addEventListener('click', () => {
            waitingFor = el.dataset.bind;
            box.querySelectorAll('.bind-key').forEach((x) => x.classList.remove('waiting'));
            el.classList.add('waiting');
            el.textContent = '按键…';
        });
    });
}

$('set-binds')?.addEventListener('click', async () => {
    const box = $('binds-list');
    if (!box) return;
    box.hidden = !box.hidden;
    if (!box.hidden) await loadBinds();
    resizeToPanel();
});
$('set-reset')?.addEventListener('click', async () => {
    try { binds = await invoke('emu_binds_reset'); renderBinds(); } catch (_) {}
});

window.addEventListener('keydown', async (e) => {
    if (!waitingFor) return;
    e.preventDefault();
    const vk = e.keyCode;
    if (!vk) return;
    binds[waitingFor] = vk;
    waitingFor = '';
    await invoke('emu_binds_set', { binds }).catch(() => {});
    renderBinds();
});

// ---------------- 输入设备 ----------------
async function refreshInput() {
    try {
        const st = await invoke('emu_input');
        const el = $('set-source');
        if (el) {
            el.textContent = st.source === 'gamepad'
                ? ('手柄 ' + (st.pad_name || ''))
                : (st.source === 'keyboard' ? '键盘' : '无输入');
        }
    } catch (_) {}
}

// ---------------- 实时数据 ----------------
async function tickLive() {
    try {
        const s = await invoke('fps_live');
        renderLive(s);
    } catch (_) {}
}

// ---------------- 存活检测 ----------------
let sawRunning = false;
async function tickAlive() {
    let alive = false;
    try { alive = await invoke('emu_alive'); } catch (_) {}
    if (alive) { sawRunning = true; return; }
    // ★ 只有「之前确实在跑、现在没了」才自动收起。
    //   否则刚打开功能条（还没起游戏）会被立刻关掉。
    if (sawRunning) {
        sawRunning = false;
        try { await invoke('emu_overlay_close'); } catch (_) {}
    }
}

// ---------------- 小提示 ----------------
let toastTimer = null;
function toast(msg, isErr) {
    let el = $('ov-toast');
    if (!el) {
        el = document.createElement('div');
        el.id = 'ov-toast';
        el.style.cssText =
            'position:fixed;left:50%;bottom:8px;transform:translateX(-50%);'
            + 'padding:6px 14px;border-radius:8px;font-size:11.5px;max-width:92%;'
            + 'background:rgba(16,16,22,.96);border:1px solid rgba(255,255,255,.18);'
            + 'color:#fff;z-index:99;pointer-events:none;opacity:0;transition:opacity .18s';
        document.body.appendChild(el);
    }
    el.textContent = msg;
    el.style.borderColor = isErr ? '#e5484d' : 'rgba(63,185,80,.6)';
    el.style.opacity = '1';
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { el.style.opacity = '0'; }, 2600);
}

// ---------------- 启动 ----------------
async function boot() {
    await loadSettings();
    await loadRecSettings();
    try {
        const info = await invoke('emu_info');
        if (info && info.game) {
            const name = String(info.game).replace(/^.*[\\/]/, '').replace(/\.[^.]+$/, '');
            setText('ov-title', name + (info.name ? ' · ' + info.name : ''));
        }
    } catch (_) {}
    refreshKeysState();
    refreshInput();
    tickLive();
    liveTimer = setInterval(tickLive, 700);
    setInterval(refreshInput, 500);
    aliveTimer = setInterval(tickAlive, 3000);
    setInterval(refreshRecState, 4000);
    resizeToPanel();
}

boot();
