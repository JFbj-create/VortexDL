// 模拟器顶部功能条 (2026-10-19)
// 独立窗口，always-on-top，浮在模拟器上方；只在需要时读取输入状态。
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
let inputTimer = null;
let aliveTimer = null;

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

// ---------------- 顶部条动作 ----------------
$('ov-max')?.addEventListener('click', async () => {
    // 模拟器是外部进程，最大化 = 让它自己的窗口最大化：用 Win32 通过后端做
    try { await invoke('emu_toggle_max'); } catch (_) {}
});
$('ov-full')?.addEventListener('click', async () => {
    try { await invoke('emu_toggle_fullscreen'); } catch (_) {}
});
$('ov-close')?.addEventListener('click', async () => {
    try { await invoke('emu_stop'); } catch (_) {}
    try { await invoke('emu_overlay_close'); } catch (_) {}
});
$('ov-settings')?.addEventListener('click', () => {
    const p = $('panel');
    if (!p) return;
    p.hidden = !p.hidden;
    // 展开时窗口要变高（简单起见交给 CSS 自适应 + 后端 set_size）
    const h = p.hidden ? 44 : Math.min(700, 44 + p.offsetHeight + 8);
    invoke('emu_overlay_resize', { height: h }).catch(() => {});
});

// ---------------- 设置 ----------------
async function loadSettings() {
    try { settings = await invoke('emu_settings_get'); } catch (_) { settings = null; }
    if (!settings) return;
    const f = $('set-fps'), s = $('set-scale');
    if (f) { f.value = settings.fps_limit; $('set-fps-v').textContent = settings.fps_limit + ' fps'; }
    if (s) { s.value = settings.scale; $('set-scale-v').textContent = settings.scale.toFixed(2) + '×'; }
    if ($('set-rumble')) $('set-rumble').checked = !!settings.rumble;
    if ($('set-hint')) $('set-hint').checked = !!settings.on_screen_hint;
    if ($('set-hide')) $('set-hide').checked = !!settings.auto_hide;
}

async function pushSettings() {
    if (!settings) settings = {};
    try { await invoke('emu_settings_set', { s: settings }); } catch (_) {}
}

$('set-fps')?.addEventListener('input', function () {
    if (!settings) settings = {};
    settings.fps_limit = Number(this.value);
    $('set-fps-v').textContent = this.value + ' fps';
    pushSettings();
});
$('set-scale')?.addEventListener('input', function () {
    if (!settings) settings = {};
    settings.scale = Number(this.value);
    $('set-scale-v').textContent = Number(this.value).toFixed(2) + '×';
    pushSettings();
});
$('set-rumble')?.addEventListener('change', function () { if (settings) { settings.rumble = this.checked; pushSettings(); } });
$('set-hint')?.addEventListener('change', function () { if (settings) { settings.on_screen_hint = this.checked; pushSettings(); } });
$('set-hide')?.addEventListener('change', function () { if (settings) { settings.auto_hide = this.checked; pushSettings(); } });

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
    const order = ['A','B','X','Y','L','R','ZL','ZR','PLUS','MINUS','UP','DOWN','LEFT','RIGHT','LSTICK_UP','LSTICK_DOWN','LSTICK_LEFT','LSTICK_RIGHT','HOME','CAPTURE'];
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
});

$('set-reset')?.addEventListener('click', async () => {
    try {
        binds = await invoke('emu_binds_reset');
        renderBinds();
    } catch (_) {}
});

// 捕获按键：等待绑定时，把按下的物理键写进映射
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

// ---------------- 状态刷新 ----------------
async function refreshInput() {
    try {
        const st = await invoke('emu_input');
        const el = $('set-source');
        if (el) {
            const src = st.source === 'gamepad' ? ('手柄 ' + (st.pad_name || '')) : (st.source === 'keyboard' ? '键盘' : '无输入');
            el.textContent = src;
        }
        const pad = $('ov-pad');
        if (pad) {
            pad.textContent = st.source === 'gamepad' ? '🎮 手柄' : (st.source === 'keyboard' ? '⌨ 键盘' : '无输入');
        }
    } catch (_) {}
}

let sawRunning = false;
async function tickAlive() {
    let alive = false;
    try { alive = await invoke('emu_alive'); } catch (_) {}
    if (alive) { sawRunning = true; return; }
    // ★ 只有「之前确实在跑、现在没了」才自动收起。
    //   否则刚打开功能条（还没起游戏）会被立刻关掉 —— 第一次实现就栽在这。
    if (sawRunning) {
        sawRunning = false;
        try { await invoke('emu_overlay_close'); } catch (_) {}
    }
}

async function boot() {
    await loadSettings();
    try {
        const info = await invoke('emu_info');
        if (info && info.game) {
            const name = String(info.game).replace(/^.*[\\/]/, '').replace(/\.[^.]+$/, '');
            $('ov-title').textContent = name + (info.name ? ' · ' + info.name : '');
        }
    } catch (_) {}
    refreshInput();
    inputTimer = setInterval(refreshInput, 250);
    aliveTimer = setInterval(tickAlive, 3000);
}

boot();
