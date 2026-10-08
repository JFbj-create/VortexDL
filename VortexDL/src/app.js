// VortexDL - 前端应用逻辑
// 使用 Tauri IPC 与 Rust 后端通信

// ★ 前端写日志到 Rust 侧 (不用 CDP 也能 100% 知道前端到底在干啥)
// 每类事件有计数，避免写爆日志文件
const _frontLogCnt = {};
// ★ 启动性能优化 (2026-09-28): localStorage 环形缓冲改为"内存暂存 + 去抖落盘"。
//   原实现每次 frontLog 都 JSON.parse + JSON.stringify 整个 400 条环 (可达数百 KB),
//   启动期 INIT_STEP 等高频调用会在主线程同步做几十次序列化 → 首屏卡顿。
//   现改为内存累积, 600ms 去抖后一次性读-改-写 localStorage。
//   与 index.html Lifeline 的 _ringPush 共享同一 key, 双方都是"读当前值再追加", 不会互相覆盖。
//   后端 append_frontend_log (下方第 2 步) 仍逐条实时落盘, 故不丢关键日志。
const _frontLogPending = [];
let _frontLogFlushTimer = null;
function _frontLogFlush() {
    _frontLogFlushTimer = null;
    if (!_frontLogPending.length) return;
    try {
        const MAX = 400; const KEY = '__vdl_front_log_ring';
        let arr = []; try { arr = JSON.parse(localStorage.getItem(KEY) || '[]'); } catch(_){ arr = []; }
        if (!Array.isArray(arr)) arr = [];
        for (let i = 0; i < _frontLogPending.length; i++) arr.push(_frontLogPending[i]);
        _frontLogPending.length = 0;
        if (arr.length > MAX) arr.splice(0, arr.length - MAX);
        localStorage.setItem(KEY, JSON.stringify(arr));
    } catch(_) { _frontLogPending.length = 0; }
}
try { window.addEventListener('beforeunload', _frontLogFlush); window.addEventListener('pagehide', _frontLogFlush); } catch(_) {}
// ★★ 早期黑盒哨兵 (2026-09-28 修正): 原代码误置于 frontLog() 内部, 导致每次写日志都重复执行 →
//    (a) 每次 frontLog 都往 window 添加一个 error 监听器 (监听器泄漏 + 启动卡顿);
//    (b) 每次都同步 parse+stringify 环形缓冲。现移到顶层, 仅在 app.js 加载时执行一次。
try {
  (function(){
    function ringPush(k,rec,n){try{var a=[];try{a=JSON.parse(localStorage.getItem(k)||'[]')}catch(e){a=[]}if(!Array.isArray(a))a=[];a.push(rec);if(a.length>n)a.splice(0,a.length-n);localStorage.setItem(k,JSON.stringify(a))}catch(e){}}
    ringPush('__vdl_front_log_ring',[Date.now(),'APP_BOOTED','ping_app_booted_anchor_ok readyState='+document.readyState],400);
    try{localStorage.setItem('__VDL_DEBUG_APP_BOOT',JSON.stringify([Date.now(),document.readyState,location.href]))}catch(e){}
    window.addEventListener('error', function(ev){try{var s=JSON.stringify([Date.now(),(ev&&ev.filename||''),(ev&&ev.lineno||''),(ev&&ev.colno||''),(ev&&ev.message||''),(ev&&ev.error&&ev.error.stack||'')]);localStorage.setItem('__VDL_DEBUG_ERR',s)}catch(e){}}, true);
  })();
} catch (_) {}
async function frontLog(kind, detail) {
    const k = String(kind || 'LOG');
    const d = detail === undefined ? '' : (typeof detail === 'string' ? detail : JSON.stringify(detail));
    // 1) localStorage 环形缓冲备用写盘 (内存暂存 + 去抖落盘, 见上)
    try {
        _frontLogPending.push([Date.now(), k, d.slice(0,1200)]);
        if (_frontLogFlushTimer === null) _frontLogFlushTimer = setTimeout(_frontLogFlush, 600);
    } catch(_) {}
    // 2) 后端 invoke (若实现则走原生落盘 frontend_events.log)
    try {
        const invoke = getInvoke();
        if (invoke) {
            await invoke('append_frontend_log', { kind: k, detail: d }).catch(()=>{});
        }
    } catch (_e) {
        // 绝对不能因为写日志而阻塞主逻辑
    }
}

// ★★★ 前端单层 EMA 平滑 (后端已有 TEMA, 前端仅做轻度平滑 + 差值防跳)
// 时间加权 EMA: α_dt = (α_per_200ms) ^ (elapsed_ms / 200)
//   α_per_200ms = 0.80 → 每200ms 吸收 20% 新值, 2秒内可达到真实值 95%
//   后端 TEMA 已经 α=0.96(≈3.3s窗口), 前端不用再叠多层, 否则速度爬不上来 (100MB/s 卡 2MB/s 的元凶)
const FRONT_EMA_ALPHA_PER_200MS = 0.80;
const FRONT_EMA_BASE_INTERVAL_MS = 200;
// 速度文字重绘阈值: 差值≥256B/s 才更新 (project_memory 要求: 避免 DOM 操作)
const SPEED_STR_ABS_DELTA_MIN = 256;
// 返回值: { changed: boolean } → true 表示显示内容变了
function applySpeedEma(dl, rawBps, elapsedMsOverride) {
    if (!dl) return { changed: false };
    // ★ 必须在修改 dl.speed 之前先记旧值, 否则 changed 永远=false (卡死的元凶)
    const oldSpeedStr = typeof dl.speed === 'string' ? dl.speed : '';
    const nowTs = Date.now();
    const dtMs = Number(elapsedMsOverride) > 0 ? Number(elapsedMsOverride)
        : (dl._ema_last_time_ms ? Math.max(50, Math.min(5000, nowTs - dl._ema_last_time_ms)) : FRONT_EMA_BASE_INTERVAL_MS);
    dl._ema_last_time_ms = nowTs;

    // ★ 时间加权 α: dt=200ms → α=0.80; dt=1000ms → 0.80^5≈0.33 (快速吸收新值)
    const alpha = Math.pow(FRONT_EMA_ALPHA_PER_200MS, dtMs / FRONT_EMA_BASE_INTERVAL_MS);

    const safeBps = (typeof rawBps === 'number' && isFinite(rawBps) && rawBps >= 0) ? rawBps : 0;
    // ★★★ 单层 EMA: 后端 TEMA(长窗口) 已经够稳, 前端仅做轻度平滑避免瞬时尖刺
    const prevEma = dl._ema_1_bps || 0;
    const emaBps = prevEma * alpha + safeBps * (1 - alpha);
    dl._ema_1_bps = emaBps;
    dl.speed_bps = Math.round(emaBps);

    // ★ 防重绘抖动: 绝对差≥64B/s 才更新 (原256 B/s, 慢速下载用户反馈"永远不变→看似卡死")
    //   ★ 特殊情况必须强制更新: 0→起速 / 高速→0(stuck) / 终态清空 / 有诊断 subphase 切换 / 停滞≥5s 状态
    const lastRendered = dl._last_render_ema_bps || 0;
    const absDelta = Math.abs(emaBps - lastRendered);
    const isZeroStuck = (safeBps === 0 && dl.state !== 'completed' && dl.state !== 'failed' && dl.state !== 'canceled' && lastRendered > 256);
    const isStartingUp = (lastRendered === 0 && emaBps > 0);
    const isTerminalCleared = (dl.state === 'completed' || dl.state === 'failed' || dl.state === 'canceled') && dl.speed !== '';
    // ★★★ 根治 C3: Stall/慢速 状态强制刷新门槛降低 → 64B/s(原来是 256, 慢/停滞瞬时恢复<256 会永远不更新 dl.speed→UI 死数字)
    const absDeltaPass = absDelta >= 64;
    // 有 subphase 诊断 or phase_stall_secs>=5s → 不管速度变没变都要刷新 (显示诊断信息给用户)
    const hasNewDiag = (dl.subphase && dl.subphase !== dl._last_ema_subphase)
        || (Number(dl.phase_stall_secs) >= 5 && dl._last_shown_stall_secs !== Math.floor(Number(dl.phase_stall_secs) / 5));
    const needUpdateSpeedStr = isZeroStuck || isStartingUp || isTerminalCleared || absDeltaPass || hasNewDiag;
    if (needUpdateSpeedStr) {
        dl.speed = formatSpeed(dl.speed_bps);
        dl._last_render_ema_bps = emaBps;
        if (dl.subphase) dl._last_ema_subphase = dl.subphase;
        if (Number(dl.phase_stall_secs) >= 5) dl._last_shown_stall_secs = Math.floor(Number(dl.phase_stall_secs) / 5);
    }
    // 完成/失败/取消态: 清空速度 + 复位 EMA 状态
    if (dl.state === 'completed' || dl.state === 'failed' || dl.state === 'canceled') {
        dl.speed = '';
        dl._last_render_ema_bps = 0;
        dl._ema_speed_bps = 0;
        dl._ema_1_bps = 0;
        dl._ema_2_bps = 0;
        dl._ema_3_bps = 0;
    }

    const newSpeedStr = typeof dl.speed === 'string' ? dl.speed : '';
    const changed = newSpeedStr !== oldSpeedStr;
    return { changed };
}

// 禁用右键菜单，防止意外触发返回首页
document.addEventListener('contextmenu', function(e) {
    e.preventDefault();
}, true);

// ★★★ 下载卡按钮事件委托 (防止 innerHTML 重建 DOM 后 hover 闪烁/事件丢失)
//   替代内联 onclick: 把 pause/resume/cancel/open/retry 5 种动作绑到 document 级
//   好处: 1) 不用每次 render 重建事件绑定  2) 新增按钮自动支持, 不会漏绑
document.addEventListener('click', function(e) {
    const btn = e.target && e.target.closest ? e.target.closest('.dl-action-btn') : null;
    if (!btn) return;
    const action = btn.dataset.action;
    const taskId = btn.dataset.taskId;
    if (!action || !taskId) return;
    e.preventDefault();
    e.stopPropagation();
    switch (action) {
        case 'pause':  if (typeof window.pauseDownload === 'function')  window.pauseDownload(taskId);  break;
        case 'resume': if (typeof window.resumeDownload === 'function') window.resumeDownload(taskId); break;
        case 'cancel': if (typeof window.cancelDownload === 'function') window.cancelDownload(taskId); break;
        case 'open':   if (typeof window.openDownloadedFile === 'function') window.openDownloadedFile(taskId); break;
        case 'retry':  if (typeof window.retryDownload === 'function')  window.retryDownload(taskId);  break;
        // ★ 新增 (2026-10-03): 删除文件。解压失败时源压缩包不会被自动删除
        //   (delete_archive 只在解压成功时生效), 而软件里原本没有任何删除入口,
        //   用户只能去资源管理器删 —— 所以补一个按钮。
        case 'delete': if (typeof window.deleteDownloadFile === 'function') window.deleteDownloadFile(taskId); break;
    }
}, false);

// 延迟获取 Tauri API, 避免页面加载时 __TAURI__ 未注入导致崩溃
function getInvoke() {
    // Tauri v2: 优先 __TAURI__.core.invoke, 回退 __TAURI_INTERNALS__.invoke
    try { if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) return window.__TAURI__.core.invoke; } catch(_) {}
    try { if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) return window.__TAURI_INTERNALS__.invoke; } catch(_) {}
    return null;
}
function getListen() {
    if (window.__TAURI__ && window.__TAURI__.event) {
        return window.__TAURI__.event.listen;
    }
    return null;
}
// 兼容包装
const invoke = (...args) => {
    const fn = getInvoke();
    if (!fn) return Promise.reject('Tauri API 未就绪');
    return fn(...args);
};
const listen = (...args) => {
    const fn = getListen();
    if (!fn) return Promise.reject('Tauri API 未就绪');
    return fn(...args);
};

// ============================================================
// 全局状态
// ============================================================
const state = {
    currentPage: 1,
    currentSource: 'all',
    currentCategory: '全部类型',
    currentKeyword: '',
    currentDetailGame: null,
    detailFromPage: null, // 详情页来源页 (resource/adult), 返回时回到对应页
    currentDetailDownloads: [], // 当前详情页的下载列表(供弹窗使用)
    downloads: new Map(),       // task_id -> download info
    infiniteScrollLoading: false,
    infiniteScrollEnd: false,
    categoryRetryCount: 0, // 分类筛选缓存未就绪时的重试次数
    pendingDownloadLink: null,  // 待下载的链接(用户在设置弹窗中确认)
    _detailReqToken: 0,  // 详情页请求竞态守卫, 每次进入递增
    initialized: false, // 资源视图是否已加载过, 避免每次切换都刷新
    searchBuffer: null,      // 搜索/分类结果分批渲染缓冲 (性能优化: 60 个/批)
    searchBufferOffset: 0,
    // 成人游戏独立页状态
    adultInitialized: false,
    adultLoading: false,
    adultEnd: false,
    adultPage: 1,
    adultCategory: '全部类型',
    adultKeyword: '',
    adultCategoriesLoaded: false,
    // ★ 视图滚动位置记忆 (2026-10-01): 修复浏览列表后点返回/切页直接回到顶部
    viewScroll: {},        // `${page}:${view}` -> scrollTop
    _curViewKey: null,     // 当前视图键, 用于保存/恢复滚动位置
};

// 当前翻译目标语言 (从后端加载, 默认 zh-CN)
// 用于前端判断游戏名是否已是目标语言, 避免不必要的翻译请求
let currentTranslateLang = 'zh-CN';

// 渲染资源导航（侧边栏来源按钮）
async function renderResourceNav() {
    const nav = document.querySelector('.resource-nav');
    if (!nav) return;
    nav.innerHTML = '';
    const allBtn = document.createElement('button');
    allBtn.className = 'resource-nav-btn active';
    allBtn.innerHTML = '全部来源';
    allBtn.onclick = () => {
        document.querySelectorAll('.resource-nav-btn').forEach(b => b.classList.remove('active'));
        allBtn.classList.add('active');
        document.querySelectorAll('.resource-game-item').forEach(el => el.style.display = '');
    };
    nav.appendChild(allBtn);
    const sources = new Set();
    document.querySelectorAll('.resource-game-item').forEach(el => {
        const s = el.dataset.source || 'unknown';
        if (s) sources.add(s);
    });
    sources.forEach(src => {
        const btn = document.createElement('button');
        btn.className = 'resource-nav-btn';
        btn.innerHTML = src;
        btn.onclick = () => {
            document.querySelectorAll('.resource-nav-btn').forEach(b => b.classList.remove('active'));
            btn.classList.add('active');
            document.querySelectorAll('.resource-game-item').forEach(el => {
                el.style.display = (el.dataset.source === src) ? '' : 'none';
            });
        };
        nav.appendChild(btn);
    });
}

// ============================================================
// 页面导航
// ============================================================

// ★ 滚动位置记忆 (2026-10-01): 修复浏览列表后点返回/切页直接回到顶部
function contentScroller() {
    return document.querySelector('.content');
}
function saveViewScroll() {
    const sc = contentScroller();
    if (!sc || !state._curViewKey) return;
    state.viewScroll[state._curViewKey] = sc.scrollTop;
}
function restoreViewScroll(key) {
    const sc = contentScroller();
    if (!sc) return;
    const pos = state.viewScroll[key] || 0;
    sc.scrollTop = pos;
    // 布局稳定后再恢复一次, 避免视图刚 display 时高度未就绪导致失效
    requestAnimationFrame(() => { sc.scrollTop = pos; });
}

// 通用页面切换函数
function navigateToPage(page) {
    // 离开当前页面前保存滚动位置
    saveViewScroll();
    document.querySelectorAll('.nav-btn').forEach(b => {
        b.classList.toggle('active', b.dataset.page === page);
    });
    document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
    const pageEl = document.getElementById(`page-${page}`);
    if (pageEl) pageEl.classList.add('active');

    if (page === 'game') {
        switchPageView('game', 'cards');
    } else {
        // 非游戏页: 记录视图键并恢复上次滚动位置
        state._curViewKey = `${page}:page`;
        restoreViewScroll(state._curViewKey);
    }
    if (page === 'home') {
        loadHomeStats();
        startMonitor();
    } else if (page === 'downloads') {
        renderDownloadList();
        stopMonitor();
    } else if (page === 'resources') {
        loadNavResourcesTo('resources-grid');
        stopMonitor();
    } else if (page === 'fav') {
        renderFavPage();
        stopMonitor();
    } else {
        stopMonitor();
    }
}

// ============================================================
// DLSS5 画质增强页 (左侧导航"下载"与"游戏"之间的独立入口)
// 集成 DLSS 版本管理: 扫描游戏 → 管理本地版本库 → 备份/替换/还原
// ============================================================
state.dlss = { library: [], games: [], scanned: false, loading: false, pickTarget: null };

const DLSS_KIND_LABEL = { dlss: '超分辨率', frame_gen: '帧生成', ray_recon: '光追重建' };

function dlssKindLabel(kind) {
    return DLSS_KIND_LABEL[kind] || 'DLSS';
}

function dlssFmtSize(bytes) {
    const n = Number(bytes) || 0;
    if (n <= 0) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB'];
    let v = n, i = 0;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
    return v.toFixed(i === 0 ? 0 : 1) + ' ' + units[i];
}

function dlssSetStatus(msg, isError) {
    const el = document.getElementById('dlss-status');
    if (!el) return;
    el.textContent = msg || '';
    el.classList.toggle('dlss-status-error', !!isError);
}

// 进入页面: 加载版本库 + 首次自动扫描
async function loadDlss5Page() {
    await dlssLoadLibrary();
    if (!state.dlss.scanned) {
        await dlssScanGames();
    } else {
        dlssRenderGames();
    }
}

async function dlssLoadLibrary() {
    try {
        const list = await invoke('dlss_library_list');
        state.dlss.library = Array.isArray(list) ? list : [];
    } catch (_) {
        state.dlss.library = [];
    }
    dlssRenderLibrary();
}

function dlssRenderLibrary() {
    const box = document.getElementById('dlss-library');
    const countEl = document.getElementById('dlss-lib-count');
    const lib = state.dlss.library || [];
    if (countEl) countEl.textContent = String(lib.length);
    if (!box) return;
    if (!lib.length) {
        box.innerHTML = '<div class="empty-state">暂无 DLL，点击“导入 DLL 到版本库”添加</div>';
        return;
    }
    box.innerHTML = lib.map(e => `
        <div class="dlss-lib-item">
            <div class="dlss-lib-main">
                <div class="dlss-lib-label">${escapeHtml(e.label)}</div>
                <div class="dlss-lib-meta">
                    <span class="dlss-tag dlss-tag-${escapeHtml(e.kind)}">${escapeHtml(dlssKindLabel(e.kind))}</span>
                    <span>${escapeHtml(e.version || '未知版本')}</span>
                    <span>${dlssFmtSize(e.size)}</span>
                </div>
            </div>
            <button class="btn btn-danger btn-small" data-dlss-del="${escapeHtml(e.id)}">删除</button>
        </div>
    `).join('');
}

function dlssRenderGames() {
    const box = document.getElementById('dlss-games');
    const countEl = document.getElementById('dlss-games-count');
    const games = state.dlss.games || [];
    if (countEl) countEl.textContent = String(games.length);
    if (!box) return;
    if (!games.length) {
        box.innerHTML = '<div class="empty-state">未找到含 DLSS 文件的游戏，可尝试“添加游戏目录”手动指定</div>';
        return;
    }
    box.innerHTML = games.map(g => {
        const files = (g.files || []).map(f => `
            <div class="dlss-file">
                <div class="dlss-file-info">
                    <div class="dlss-file-name">${escapeHtml(f.name)}</div>
                    <div class="dlss-file-meta">
                        <span class="dlss-tag dlss-tag-${escapeHtml(f.kind)}">${escapeHtml(dlssKindLabel(f.kind))}</span>
                        <span>v${escapeHtml(f.version || '未知')}</span>
                        <span>${dlssFmtSize(f.size)}</span>
                        ${f.backed_up ? '<span class="dlss-badge-bak">已备份</span>' : ''}
                    </div>
                </div>
                <div class="dlss-file-actions">
                    <button class="btn btn-primary btn-small" data-dlss-apply="${escapeHtml(f.path)}" data-dlss-kind="${escapeHtml(f.kind)}">替换</button>
                    ${f.backed_up ? `<button class="btn btn-secondary btn-small" data-dlss-restore="${escapeHtml(f.path)}">还原</button>` : ''}
                </div>
            </div>
        `).join('');
        return `
            <div class="dlss-game">
                <div class="dlss-game-head">
                    <div class="dlss-game-name">${escapeHtml(g.name)}</div>
                    <span class="dlss-game-source">${escapeHtml(g.source)}</span>
                </div>
                <div class="dlss-game-dir" title="${escapeHtml(g.dir)}">${escapeHtml(g.dir)}</div>
                <div class="dlss-game-files">${files}</div>
            </div>
        `;
    }).join('');
}

async function dlssScanGames() {
    if (state.dlss.loading) return;
    state.dlss.loading = true;
    const btn = document.getElementById('dlss-scan-btn');
    if (btn) btn.disabled = true;
    dlssSetStatus('正在扫描游戏中的 DLSS 文件…');
    try {
        const res = await invoke('dlss_scan_games');
        state.dlss.games = (res && res.games) || [];
        state.dlss.scanned = true;
        dlssRenderGames();
        const ms = res && res.elapsed_ms != null ? `（耗时 ${res.elapsed_ms} ms）` : '';
        dlssSetStatus(`扫描完成，共找到 ${state.dlss.games.length} 个游戏${ms}`);
    } catch (e) {
        dlssSetStatus('扫描失败: ' + (e && e.message ? e.message : String(e)), true);
    } finally {
        state.dlss.loading = false;
        if (btn) btn.disabled = false;
    }
}

async function dlssImportDll() {
    try {
        const path = await invoke('dlss_pick_dll');
        if (!path) return;
        dlssSetStatus('正在导入…');
        await invoke('dlss_library_import', { srcPath: path, label: null });
        await dlssLoadLibrary();
        dlssSetStatus('已导入到版本库');
    } catch (e) {
        dlssSetStatus('导入失败: ' + (e && e.message ? e.message : String(e)), true);
    }
}

async function dlssDeleteLib(id) {
    try {
        await invoke('dlss_library_delete', { id });
        await dlssLoadLibrary();
        dlssSetStatus('已从版本库删除');
    } catch (e) {
        dlssSetStatus('删除失败: ' + (e && e.message ? e.message : String(e)), true);
    }
}

async function dlssAddDir() {
    try {
        const dir = await invoke('dlss_pick_folder');
        if (!dir) return;
        let settings;
        try { settings = await invoke('dlss_get_settings'); } catch (_) { settings = { custom_dirs: [] }; }
        const dirs = Array.isArray(settings.custom_dirs) ? settings.custom_dirs.slice() : [];
        if (!dirs.some(d => d.toLowerCase() === dir.toLowerCase())) dirs.push(dir);
        await invoke('dlss_save_settings', { settings: { custom_dirs: dirs } });
        dlssSetStatus('已添加目录，正在重新扫描…');
        await dlssScanGames();
    } catch (e) {
        dlssSetStatus('添加目录失败: ' + (e && e.message ? e.message : String(e)), true);
    }
}

function dlssOpenPick(targetDll, kind) {
    state.dlss.pickTarget = targetDll;
    const modal = document.getElementById('dlss-pick-modal');
    const body = document.getElementById('dlss-pick-body');
    if (!modal || !body) return;
    const all = state.dlss.library || [];
    const matched = kind ? all.filter(e => e.kind === kind) : all;
    const pool = matched.length ? matched : all;
    if (!pool.length) {
        body.innerHTML = '<div class="empty-state">版本库为空，请先“导入 DLL 到版本库”</div>';
    } else {
        body.innerHTML = pool.map(e => `
            <div class="dlss-lib-item dlss-lib-pickable" data-dlss-pick="${escapeHtml(e.id)}">
                <div class="dlss-lib-main">
                    <div class="dlss-lib-label">${escapeHtml(e.label)}</div>
                    <div class="dlss-lib-meta">
                        <span class="dlss-tag dlss-tag-${escapeHtml(e.kind)}">${escapeHtml(dlssKindLabel(e.kind))}</span>
                        <span>${escapeHtml(e.version || '未知版本')}</span>
                        <span>${dlssFmtSize(e.size)}</span>
                    </div>
                </div>
                <div class="card-arrow">→</div>
            </div>
        `).join('');
    }
    modal.style.display = 'flex';
}

function closeDlssPick() {
    const modal = document.getElementById('dlss-pick-modal');
    if (modal) modal.style.display = 'none';
    state.dlss.pickTarget = null;
}

async function dlssApplyPick(libraryId) {
    const target = state.dlss.pickTarget;
    if (!target) return;
    closeDlssPick();
    dlssSetStatus('正在替换（原文件将自动备份）…');
    try {
        await invoke('dlss_apply', { targetDll: target, libraryId });
        await dlssScanGames();
        dlssSetStatus('替换完成，原文件已备份');
    } catch (e) {
        dlssSetStatus('替换失败: ' + (e && e.message ? e.message : String(e)), true);
    }
}

async function dlssRestore(targetDll) {
    dlssSetStatus('正在还原…');
    try {
        await invoke('dlss_restore', { targetDll });
        await dlssScanGames();
        dlssSetStatus('已还原为原始 DLL');
    } catch (e) {
        dlssSetStatus('还原失败: ' + (e && e.message ? e.message : String(e)), true);
    }
}

// 处理 DLSS5 页控件点击 (删除 / 选择 / 替换 / 还原)
function handleDlss5Card(card) {
    const del = card.dataset.dlssDel;
    if (del) { dlssDeleteLib(del); return true; }
    const pick = card.dataset.dlssPick;
    if (pick) { dlssApplyPick(pick); return true; }
    const apply = card.dataset.dlssApply;
    if (apply) { dlssOpenPick(apply, card.dataset.dlssKind || ''); return true; }
    const restore = card.dataset.dlssRestore;
    if (restore) { dlssRestore(restore); return true; }
    return false;
}

// DLSS5 页事件委托 (工具栏按钮 / 版本库条目 / 游戏文件操作)
document.addEventListener('click', function(e) {
    if (e.target.closest('#dlss-scan-btn')) { e.preventDefault(); dlssScanGames(); return; }
    if (e.target.closest('#dlss-add-dir-btn')) { e.preventDefault(); dlssAddDir(); return; }
    if (e.target.closest('#dlss-import-btn')) { e.preventDefault(); dlssImportDll(); return; }
    const ctl = e.target.closest('[data-dlss-del], [data-dlss-pick], [data-dlss-apply], [data-dlss-restore]');
    if (ctl) {
        e.preventDefault();
        e.stopPropagation();
        handleDlss5Card(ctl);
    }
}, false);

// 页面内视图切换 (卡片视图/详情视图)
async function switchPageView(pageName, viewName) {
    const pageEl = document.getElementById(`page-${pageName}`);
    if (!pageEl) return;

    // 保存当前视图滚动位置 (离开前)
    saveViewScroll();

    // 隐藏所有视图
    pageEl.querySelectorAll('.view-cards, .view-detail').forEach(v => {
        v.style.display = 'none';
    });

    // 显示目标视图
    if (viewName === 'cards') {
        const cardsView = pageEl.querySelector('.view-cards');
        if (cardsView) cardsView.style.display = 'block';
        currentActiveView = 'cards';
    } else {
        const detailView = pageEl.querySelector(`.view-detail[data-view="${viewName}"]`);
        if (detailView) {
                detailView.style.display = 'block';
                currentActiveView = viewName;
                // 加载对应内容
                if (pageName === 'game' && viewName === 'resource') {
                    // 仅首次打开时刷新, 后续切换保留已有内容
                    if (!state.initialized) {
                        loadResourceGames();
                    }
                } else if (pageName === 'game' && viewName === 'adult') {
                    // 成人游戏独立页: 首次进入加载分类标签 + 游戏列表
                    if (!state.adultInitialized) {
                        loadAdultGames();
                    }
                } else if (pageName === 'game' && viewName === 'gx') {
                    // ★ GX (galgamex 游戏库): 本地全量索引, 首次进入拉状态 + 标签 + 首屏
                    if (window.__vxGx && window.__vxGx.onShow) window.__vxGx.onShow();
                }
            }
    }

    // 记录当前视图键 + 恢复该视图上次的滚动位置
    state._curViewKey = `${pageName}:${viewName}`;
    restoreViewScroll(state._curViewKey);

    // 保存上次选择
    localStorage.setItem(`vortex_${pageName}_view`, viewName);
}

// 侧边栏按钮导航 - 事件委托方式
document.querySelector('.sidebar')?.addEventListener('click', function(e) {
    const btn = e.target.closest('.nav-btn');
    if (!btn) return;
    e.preventDefault();
    e.stopPropagation();
    const page = btn.dataset.page;
    if (page) navigateToPage(page);
});

// 卡片和返回按钮点击 - 事件委托
document.querySelector('.content')?.addEventListener('click', function(e) {
    // 返回按钮
    const backBtn = e.target.closest('.back-to-cards');
    if (backBtn) {
        e.preventDefault();
        e.stopPropagation();
        const pageEl = backBtn.closest('.page');
        if (pageEl) {
            const pageName = pageEl.id.replace('page-', '');
            switchPageView(pageName, 'cards');
        }
        return;
    }

    // 卡片点击 - 支持页面跳转和视图切换
    const card = e.target.closest('.section-card, .clickable-card');
    if (card) {
        e.preventDefault();
        e.stopPropagation();

        // DLSS5 页面卡片: 启动本机程序 / 打开链接 (优先处理)
        if (card.dataset.action && card.closest('#page-dlss5')) {
            handleDlss5Card(card);
            return;
        }

        // 首页统计卡片跳转
        const targetPage = card.dataset.page;
        if (targetPage) {
            navigateToPage(targetPage);
            const targetView = card.dataset.view;
            if (targetView) {
                setTimeout(() => switchPageView(targetPage, targetView), 100);
            }
            return;
        }

        
        // 页面内卡片视图切换
        const pageEl = card.closest('.page');
        const view = card.dataset.view;
        if (pageEl && view) {
            const pageName = pageEl.id.replace('page-', '');
            switchPageView(pageName, view);
        }
        return;
    }
});

// 首页启动时间戳 - 累计运行时间 (跨会话持久保存)
//   读取上次保存的累计秒数 + 本次启动时间戳, 每秒更新时累加
let homeStatsStartTime = Date.now();
let homeStatsAccumulated = 0; // 上次关闭时已累计的秒数
try {
    const saved = localStorage.getItem('vortex_total_runtime');
    if (saved) homeStatsAccumulated = parseInt(saved) || 0;
} catch(e) {}
// 保存本次启动时间戳
try { localStorage.setItem('vortex_start_time', homeStatsStartTime.toString()); } catch(e) {}

// 缓存分类数据，避免重复请求
let cachedCategories = null;

// 当前活跃视图 - 用于详情页返回时正确导航
let currentActiveView = 'cards';

// =================================================================
// 模块化工具系统已移除 (2026-09-13)
// =================================================================

// 加载首页统计数据
async function loadHomeStats() {
    const gamesEl = document.getElementById('stat-games-count');
    const downloadedEl = document.getElementById('home-downloaded-count');
    const runtimeEl = document.getElementById('home-runtime');

    try {
        const invokeFn = getInvoke();
        if (!invokeFn) {
            if (gamesEl) gamesEl.textContent = '—';
            if (downloadedEl) downloadedEl.textContent = '—';
            if (runtimeEl) runtimeEl.textContent = '加载中...';
            return;
        }

        // 获取游戏统计: (快照BY数, 快照KO数, 快照GX数, 快照总数, 内存BY数, 内存KO数)
        const [byCount, koCount, gxCount, total, byMem, koMem] = await invokeFn('snapshot_stats');
        if (gamesEl) {
            // 使用最大值: 内存缓存 or 快照数据, 确保显示真实的资源数量
            const byTotal = Math.max(byCount, byMem);
            const koTotal = Math.max(koCount, koMem);
            const displayCount = byTotal + koTotal + gxCount;
            // ★ 体验 (2026-10-03): 启动瞬间缓存还没灌好, 这里会算出 0。
            //   显示 "0 游戏资源" 会让用户以为资源丢了 (实际只是在后台预热中),
            //   所以 0 时显示 "—" 占位, 等预热完成后由 snapshot-refreshed 刷新成真实数字。
            gamesEl.textContent = displayCount > 0 ? displayCount.toLocaleString() : '—';
        }

        // 获取已下载数量
        //
        // ★ 修复 (2026-10-02): 原来只统计 `get_downloads_status` 里 state==='completed'
        //   的**活动任务**。但后端该接口只返回内存中的在跑任务, 下载完成后任务就被
        //   清理掉了 → 主页"已下载"永远是 0, 而用户明明已经下过很多。
        //   现在改为统计**持久化的下载记录** (localStorage 的 vortex_downloads_v2),
        //   它包含历史条目; 同时把仍在活动的已完成任务并入去重。
        if (downloadedEl) {
            let completedCount = 0;
            const seen = new Set();
            // 1) 持久化历史 (主要来源)
            try {
                const raw = localStorage.getItem(DL_STORAGE_KEY);
                const saved = raw ? JSON.parse(raw) : [];
                if (Array.isArray(saved)) {
                    for (const d of saved) {
                        if (!d) continue;
                        const st = String(d.state || '');
                        if (st !== 'completed') continue;
                        const key = String(d.id || d.task_id || d.file_path || d.url || '');
                        if (key && seen.has(key)) continue;
                        if (key) seen.add(key);
                        completedCount++;
                    }
                }
            } catch (_) { /* 存储不可用时忽略 */ }
            // 2) 内存中的活动任务 (可能还没落盘)
            try {
                const downloads = await invokeFn('get_downloads_status');
                if (downloads && Array.isArray(downloads)) {
                    for (const d of downloads) {
                        if (!d) continue;
                        const st = String(d.state || '');
                        if (st !== 'completed') continue;
                        const key = String(d.task_id || d.id || '');
                        if (key && seen.has(key)) continue;
                        if (key) seen.add(key);
                        completedCount++;
                    }
                }
            } catch (_) { /* 后端未就绪时忽略 */ }
            downloadedEl.textContent = completedCount.toLocaleString();
        }

        // 立即更新运行时长
        if (runtimeEl) {
            updateRuntime(runtimeEl);
        }
    } catch (e) {
        console.log('加载统计数据失败:', e);
        if (gamesEl) gamesEl.textContent = '—';
        if (downloadedEl) downloadedEl.textContent = '—';
    }
}

// 更新运行时长显示 (累计跨会话, 每秒更新)
function updateRuntime(el) {
    if (!el) return;
    const thisSession = Math.floor((Date.now() - homeStatsStartTime) / 1000);
    const total = homeStatsAccumulated + thisSession;
    const days = Math.floor(total / 86400);
    const hours = Math.floor((total % 86400) / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    const seconds = total % 60;
    if (days > 0) {
        el.textContent = `${days}d ${hours}h ${minutes}m`;
    } else if (hours > 0) {
        el.textContent = `${hours}h ${minutes}m ${seconds}s`;
    } else if (minutes > 0) {
        el.textContent = `${minutes}m ${seconds}s`;
    } else {
        el.textContent = `${seconds}s`;
    }
}

// 全局运行时长计时器 - 每 5 秒更新一次 + 持久保存累计值
function startRuntimeTimer() {
    if (window._globalRuntimeTimer) return;
    // 立即更新一次
    const runtimeEl0 = document.getElementById('home-runtime');
    if (runtimeEl0) updateRuntime(runtimeEl0);
    window._globalRuntimeTimer = setInterval(() => {
        const runtimeEl = document.getElementById('home-runtime');
        if (runtimeEl) {
            updateRuntime(runtimeEl);
        }
        // 持久保存累计运行时间 (每 5 秒写一次, 关闭时不会丢失太多)
        const thisSession = Math.floor((Date.now() - homeStatsStartTime) / 1000);
        try { localStorage.setItem('vortex_total_runtime', String(homeStatsAccumulated + thisSession)); } catch(e) {}
    }, 1000);
    // 关闭/刷新时保存
    window.addEventListener('beforeunload', () => {
        const thisSession = Math.floor((Date.now() - homeStatsStartTime) / 1000);
        try { localStorage.setItem('vortex_total_runtime', String(homeStatsAccumulated + thisSession)); } catch(e) {}
        // ★ 下载列表也要落盘（活动任务存成 paused）：否则"下到一半关软件"那条会消失
        try { persistDownloadsToStorage(); } catch (e) {}
    });
    // ★ 活动任务定期落盘：进程被强杀（任务管理器/崩溃）时 beforeunload 不会跑，
    //   这里每 4 秒兜一次，保证列表里那条任务最多只丢 4 秒的进度记录。
    setInterval(function () {
        for (const d of state.downloads.values()) {
            if (d.state === 'running' || d.state === 'starting') { schedulePersistDownloads(); return; }
        }
    }, 4000);
    // ★ 卡在 0 字节的任务要**继续刷新提示文字**（"连接中…(已等 Ns)" → "链接可能已过期"）。
    //   0 字节时进度事件里 downloaded 一直不变，changed=false 不会触发重绘，
    //   所以这里单独补一次低频重绘（只画那一条，开销很小）。
    setInterval(function () {
        for (const [id, d] of state.downloads) {
            if ((d.state === 'running' || d.state === 'starting') && !d.downloaded) {
                try { patchDownloadItemIncrementally(d, id); } catch (e) {}
            }
        }
    }, 5000);
}

// =================================================================
// ★★★ 内置浏览器 (WebView2) 性能优化: 高频资源站域名预连接
// 业界通用手段 (与 Chrome/Edge 的 "预测网络操作以加快浏览速度" 同理):
//   1. 插入 <link rel="preconnect"> 让浏览器内核提前建立 DNS+TCP+TLS 连接
//   2. 插入 <link rel="dns-prefetch"> 作为不支持 preconnect 时的兜底
//   3. 仅针对用户高频访问的 3 大游戏源 + 常用平台预热, 不贪多(连接池大小有限)
//   4. requestIdleCallback 空闲时执行, 绝不抢占下载/UI 渲染资源
// =================================================================
// WebView2 预热 (内置浏览器已移除 → 空函数保留调用点兼容, 不做任何事情)
function warmupWebView2Preconnect() { /* no-op */ }

// 资源导航图标库: emoji → 单色 SVG (stroke=currentColor, 跟随主题色)
// 解决彩色 emoji 在德古拉等主题下不变色、"像一张图片"的问题
const NAV_ICON_SVG = {
    '🎮': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="11" x2="10" y2="11"/><line x1="8" y1="9" x2="8" y2="13"/><circle cx="15.5" cy="11" r="1"/><circle cx="18" cy="13" r="1"/><rect x="2" y="6" width="20" height="12" rx="6"/></svg>',
    '📊': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="20" x2="6" y2="16"/><line x1="12" y1="20" x2="12" y2="10"/><line x1="18" y1="20" x2="18" y2="4"/></svg>',
    '🌐': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>',
    '💬': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z"/></svg>',
    '🔧': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></svg>',
    '🛠️': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></svg>',
    '✨': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9L12 3z"/><path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8L19 15z"/></svg>',
    '📄': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><line x1="9" y1="13" x2="15" y2="13"/><line x1="9" y1="17" x2="15" y2="17"/></svg>',
    '📖': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/></svg>',
    '🖥️': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>',
    '🆓': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M15 9h-4.5a1.5 1.5 0 0 0 0 3h3a1.5 1.5 0 0 1 0 3H9"/><line x1="12" y1="7" x2="12" y2="17"/></svg>',
    '🕹️': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="11"/><circle cx="12" cy="4" r="1.6"/><rect x="4" y="11" width="16" height="8" rx="4"/><line x1="8" y1="15" x2="8.01" y2="15"/><line x1="16" y1="15" x2="16.01" y2="15"/></svg>',
    '☁️': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M18 10h-1.26A8 8 0 1 0 9 20h9a5 5 0 0 0 0-10z"/></svg>',
    '🛒': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="9" cy="21" r="1"/><circle cx="20" cy="21" r="1"/><path d="M1 1h4l2.68 13.39a2 2 0 0 0 2 1.61h9.72a2 2 0 0 0 2-1.61L23 6H6"/></svg>',
    '🔐': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg>',
    '📋': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/></svg>',
    '📦': '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"/><polyline points="3.27 6.96 12 12.01 20.73 6.96"/><line x1="12" y1="22.08" x2="12" y2="12"/></svg>',
};
const NAV_ICON_FALLBACK = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><line x1="3" y1="12" x2="21" y2="12"/></svg>';

// 加载资源导航到指定容器 - 资源站快捷入口 (统一走内置浏览器, 不依靠外部系统浏览器)
// 性能优化: 使用 DocumentFragment + data-url 事件委托, 避免为每个卡片创建独立 onclick 闭包 (减少 60+ 监听器)
function loadNavResourcesTo(containerId) {
    const container = document.getElementById(containerId);
    if (!container) return;

    // 使用 DocumentFragment 批量构建，减少 reflow
    const fragment = document.createDocumentFragment();

    NAV_RESOURCES.forEach(section => {
        // section 标题容器
        const sectionWrap = document.createElement('div');
        sectionWrap.className = 'nav-section';
        sectionWrap.style.gridColumn = '1 / -1';

        const titleEl = document.createElement('div');
        titleEl.className = 'nav-section-title';
        titleEl.textContent = section.section;
        sectionWrap.appendChild(titleEl);

        const grid = document.createElement('div');
        grid.className = 'nav-grid';

        section.items.forEach(item => {
            const card = document.createElement('div');
            card.className = 'nav-card';
            // 使用 dataset 存储 URL/动作, 统一由父容器的事件委托处理 (业界标准性能优化)
            card.dataset.url = item.url || '';
            card.dataset.action = item.action || 'open-browser';
            if (item.path) card.dataset.path = item.path;

            const icon = document.createElement('div');
            icon.className = 'nav-card-icon';
            icon.innerHTML = NAV_ICON_SVG[item.icon] || NAV_ICON_FALLBACK;

            const name = document.createElement('div');
            name.className = 'nav-card-name';
            name.textContent = item.name;

            card.appendChild(icon);
            card.appendChild(name);
            grid.appendChild(card);
        });

        sectionWrap.appendChild(grid);
        fragment.appendChild(sectionWrap);
    });

    // 单次清空 + 单次 append, 只触发 1 次 reflow (原 innerHTML 会先清空再重建)
    container.innerHTML = '';
    container.appendChild(fragment);

    // ★ 事件委托: 整个容器绑定 1 个监听器处理所有卡片点击 (替代原来 60+ 个内联 onclick)
    // 注意: 只绑定一次, 避免重复绑定
    if (!container._navDelegateBound) {
        container._navDelegateBound = true;
        container.addEventListener('click', async (e) => {
            const card = e.target.closest('.nav-card');
            if (!card) return;
            const action = card.dataset.action;
            const url = card.dataset.url;
            // 启动本机已安装的程序 (如 DLSS 5 Swapper); 未安装则回退到下载页
            if (action === 'launch-local') {
                try {
                    await invoke('launch_program', { path: card.dataset.path || '' });
                    showToast('已启动 DLSS5 Swapper', 'success');
                    return;
                } catch (err) {
                    if (url) openBrowserWindow(url);
                    return;
                }
            }
            if (url) openBrowserWindow(url);
        }, { passive: true });
    }
}


// 渲染分类到容器
function renderCategoriesTo(container, categories) {
    if (!categories || categories.length === 0) {
        container.innerHTML = '<div class="empty-state">暂无资源</div>';
        return;
    }

    container.innerHTML = '';
    // 使用 DocumentFragment 批量添加，减少重排
    const fragment = document.createDocumentFragment();
    categories.forEach(cat => {
        const card = document.createElement('div');
        card.className = 'nav-category';
        card.innerHTML = `
            <div class="nav-category-name">${cat.name}</div>
            <div class="nav-category-count">${cat.source}</div>
        `;
        card.addEventListener('click', () => {
            navigateToPage('game');
            setTimeout(() => {
                const filter = document.getElementById('category-filter');
                if (filter) {
                    for (let i = 0; i < filter.options.length; i++) {
                        if (filter.options[i].value === cat.name || filter.options[i].text === cat.name) {
                            filter.value = cat.name;
                            filter.dispatchEvent(new Event('change'));
                            break;
                        }
                    }
                }
            }, 100);
        });
        fragment.appendChild(card);
    });
    container.appendChild(fragment);
}

// 资源导航加载
function loadNavResources() {
    renderResourceNav();
}

(document.getElementById('back-to-resource') || {}).addEventListener && document.getElementById('back-to-resource').addEventListener('click', () => {
    // 根据来源页返回: 成人游戏详情 → 游戏页的成人视图, 游戏详情 → 游戏页的游戏视图
    const targetView = state.detailFromPage || 'resource';
    const targetPage = 'game';
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    const navBtn = document.querySelector(`[data-page="${targetPage}"]`);
    if (navBtn) navBtn.classList.add('active');
    document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
    const targetEl = document.getElementById(`page-${targetPage}`);
    if (targetEl) targetEl.classList.add('active');
    // 切换到对应视图
    switchPageView(targetPage, targetView);
    state.detailFromPage = null;
});

// ============================================================
// 成人游戏已合并到主列表 (带 "成人游戏" 标签, 走分类筛选), 独立分区已移除
// ============================================================

// 中央 Toast 提示 —— 已禁用 (用户要求只保留启动时液态玻璃游戏数量通知, 不显示任何黑色底部提示)
// ★ 2026-10-06 重做: 原来这里是个空函数 (注释写着"不显示任何黑色底部 Toast 提示"),
//   导致整个软件的提示全部被丢弃 —— 用户只看到那些遮罩弹窗, 体验很差。
//   现在统一走 toast: 右下角毛玻璃小条 + 图标 + 自动消失 + 进出动画。
//   kind: 'ok' | 'err' | 'warn' | 'info' (默认 info)
//   ms: 显示时长, 默认按类型
function showToast(msg, kind, ms) {
    try {
        const text = String(msg == null ? '' : msg).trim();
        if (!text) return;
        let host = document.getElementById('vx-toast-host');
        if (!host) {
            host = document.createElement('div');
            host.id = 'vx-toast-host';
            host.className = 'vx-toast-host';
            document.body.appendChild(host);
        }
        const k = kind || 'info';
        const icon = ({ ok: '✓', err: '!', warn: '!', info: 'i' })[k] || 'i';
        const el = document.createElement('div');
        el.className = 'vx-toast is-' + k;
        el.innerHTML = '<span class="vx-toast-icon">' + icon + '</span>' +
                       '<span class="vx-toast-msg"></span>';
        // 用 textContent 塞文本, 避免把消息里的 HTML 当标签执行
        el.querySelector('.vx-toast-msg').textContent = text;
        host.appendChild(el);
        // 进出动画: 下一帧加 is-in
        requestAnimationFrame(function () { el.classList.add('is-in'); });
        const life = typeof ms === 'number' ? ms
            : (k === 'err' ? 5200 : (k === 'warn' ? 4600 : 3200));
        const kill = function () {
            el.classList.remove('is-in');
            el.classList.add('is-out');
            setTimeout(function () { try { el.remove(); } catch (_) {} }, 260);
        };
        const timer = setTimeout(kill, life);
        // 点一下就提前关掉
        el.addEventListener('click', function () { clearTimeout(timer); kill(); });
        // 最多同时留 5 条, 超了把最老的挤掉
        const all = host.querySelectorAll('.vx-toast');
        if (all.length > 5) { try { all[0].remove(); } catch (_) {} }
    } catch (e) {
        console.warn('[toast]', msg, e);
    }
}

// 判断字符串是否已是当前目标语言 (避免不必要的翻译请求)
// 根据当前翻译目标语言动态判断
function isMostlyChinese(text) {
    return isTargetLang(text, currentTranslateLang);
}

// 判断字符串是否已是指定目标语言
function isTargetLang(text, lang) {
    if (!text) return true;
    const chars = [...text];
    if (chars.length === 0) return true;
    // 中文: 中文字符占比 >= 30%
    if (lang.startsWith('zh')) {
        const chinese = chars.filter(c => c >= '\u{4e00}' && c <= '\u{9fff}').length;
        return (chinese / chars.length) >= 0.3;
    }
    // 日文: 平假名/片假名占比 >= 10%
    if (lang.startsWith('ja')) {
        const jp = chars.filter(c =>
            (c >= '\u{3040}' && c <= '\u{309F}') ||  // 平假名
            (c >= '\u{30A0}' && c <= '\u{30FF}')    // 片假名
        ).length;
        return (jp / chars.length) >= 0.1;
    }
    // 俄文: 西里尔字符占比 >= 30%
    if (lang.startsWith('ru')) {
        const cyrillic = chars.filter(c => c >= '\u{0400}' && c <= '\u{04FF}').length;
        return (cyrillic / chars.length) >= 0.3;
    }
    // 韩文: 韩文字符占比 >= 30%
    if (lang.startsWith('ko')) {
        const korean = chars.filter(c => c >= '\u{AC00}' && c <= '\u{D7AF}').length;
        return (korean / chars.length) >= 0.3;
    }
    // 英文: ASCII 字母占比 >= 50%
    if (lang.startsWith('en')) {
        const alpha = chars.filter(c => /[a-zA-Z]/.test(c)).length;
        return (alpha / chars.length) >= 0.5;
    }
    // 其他语言: 无法判断, 返回 false 触发翻译
    return false;
}

// 异步翻译卡片列表中非中文的名字, 翻译完更新 DOM
// 简单 CSS 转义 (用于属性选择器)
function cssEscape(s) {
    return String(s).replace(/["\\]/g, '\\$&');
}

// 强制禁用所有 input 的自动填充和历史记录弹窗
// WebView2 (Edge) 有时忽略 HTML 的 autocomplete=off, 需要额外措施:
// 1. 设置 autocomplete=off/new-password (Edge 识别 new-password 为禁用)
// 2. focus 时清除值再恢复 (阻止历史下拉)
// 3. 给搜索框加 fake name 避免浏览器记忆
function disableInputAutocomplete() {
    const inputs = document.querySelectorAll('input[type="text"], input[type="search"], input[type="number"]');
    inputs.forEach(input => {
        // 强制设置 autocomplete=off
        input.setAttribute('autocomplete', 'off');
        input.setAttribute('autocapitalize', 'off');
        input.setAttribute('autocorrect', 'off');
        input.setAttribute('spellcheck', 'false');
        // 添加随机 name 避免浏览器记忆 (仅对无 name 的输入)
        if (!input.hasAttribute('name')) {
            input.setAttribute('name', 'no-autofill-' + Math.random().toString(36).slice(2));
        }
    });
    // 对搜索框特别处理: focus 时临时改 type 阻止历史下拉
    ['search-input', 'adult-search-input', 'browser-url-input'].forEach(id => {
        const el = document.getElementById(id);
        if (!el) return;
        el.addEventListener('focus', () => {
            // 临时设为 search 类型再设回, 阻止 WebView2 弹历史
            el.setAttribute('autocomplete', 'off');
        });
    });
}

// 成人游戏独立页: 事件绑定 + 无限滚动
function setupAdultPage() {
    // 搜索按钮
    const btn = document.getElementById('adult-search-btn');
    if (btn) {
        btn.addEventListener('click', () => {
            const input = document.getElementById('adult-search-input');
            state.adultKeyword = (input ? input.value : '').trim();
            loadAdultReset();
        });
    }
    // 搜索框 Enter
    const input = document.getElementById('adult-search-input');
    if (input) {
        input.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') {
                state.adultKeyword = input.value.trim();
                loadAdultReset();
            }
        });
    }
    // 分类筛选变化
    const sel = document.getElementById('adult-category-filter');
    if (sel) {
        sel.addEventListener('change', () => {
            state.adultCategory = sel.value || '全部类型';
            state.adultKeyword = '';
            const si = document.getElementById('adult-search-input');
            if (si) si.value = '';
            loadAdultReset();
        });
    }
    // 无限滚动
    setupAdultInfiniteScroll();
}

// ============================================================
// 游戏翻译工具 (魔改自 LibreTranslate + TranslateLocally, T/L 双引擎动态切换)
// ============================================================

/// 浏览选择游戏目录
async function browseTranslatorDir() {
    const dir = await invoke('browse_path');
    if (dir) {
        document.getElementById('translator-dir').value = dir;
    }
}

/// 加载当前翻译引擎和配置 (进入翻译页时调用)
async function loadTranslatorConfig() {
    try {
        const engine = await invoke('game_tr_get_engine');
        const radio = document.querySelector(`input[name="tr-engine"][value="${engine}"]`);
        if (radio) radio.checked = true;
        updateEngineCardHighlight();

        const cfg = await invoke('game_tr_get_config');
        if (cfg.target_lang) {
            document.getElementById('tr-target-lang').value = cfg.target_lang.startsWith('zh_Hant') ? 'zh_Hant' : cfg.target_lang;
        }
        if (typeof cfg.auto_backup === 'boolean') {
            document.getElementById('tr-auto-backup').checked = cfg.auto_backup;
        }
    } catch (e) {
        console.warn('加载翻译配置失败:', e);
    }
}

/// 引擎单选变化时高亮 + 动态切换后端引擎
function setupEngineSwitch() {
    document.querySelectorAll('input[name="tr-engine"]').forEach(radio => {
        radio.addEventListener('change', async () => {
            const engine = radio.value;
            updateEngineCardHighlight();
            try {
                await invoke('game_tr_set_engine', { engine });
                showToast(`已切换到 ${engine} 模式`);
            } catch (e) {
                console.error('切换引擎失败:', e);
                showToast('切换引擎失败: ' + e);
            }
        });
    });
}

function updateEngineCardHighlight() {
    const checked = document.querySelector('input[name="tr-engine"]:checked');
    document.querySelectorAll('.engine-option').forEach(opt => {
        opt.classList.remove('selected');
    });
    if (checked) {
        checked.closest('.engine-option').classList.add('selected');
    }
}

/// 目标语言/备份变化时同步到后端
function setupTranslatorOptions() {
    document.getElementById('tr-target-lang')?.addEventListener('change', async (e) => {
        const lang = e.target.value;
        try {
            await invoke('game_tr_set_config', { targetLang: lang });
            // 切换语言时清空缓存, 强制重新翻译
            await invoke('game_tr_clear_cache');
        } catch (err) {
            console.warn('设置目标语言失败:', err);
        }
    });
    document.getElementById('tr-auto-backup')?.addEventListener('change', async (e) => {
        try {
            await invoke('game_tr_set_config', { autoBackup: e.target.checked });
        } catch (err) {
            console.warn('设置备份选项失败:', err);
        }
    });
}

/// 一键翻译游戏目录
async function launchGameTranslator() {
    const dir = document.getElementById('translator-dir').value.trim();
    if (!dir) {
        showToast('请先选择游戏根目录');
        return;
    }
    const source = document.getElementById('tr-source-lang').value;
    const btn = document.getElementById('tr-launch-btn');
    const progress = document.getElementById('tr-progress');
    const fill = document.getElementById('tr-progress-fill');
    const log = document.getElementById('tr-progress-log');
    const result = document.getElementById('tr-result');

    btn.disabled = true;
    btn.textContent = '翻译中...';
    progress.style.display = 'block';
    result.style.display = 'none';
    fill.style.width = '30%';
    log.textContent = '正在扫描游戏目录文本文件...';

    try {
        fill.style.width = '50%';
        log.textContent = '正在翻译文本 (T=本地词典 / L=在线API)...';
        const report = await invoke('game_tr_translate_dir', { dir, source });
        fill.style.width = '100%';
        log.textContent = '翻译完成 ✓';
        result.style.display = 'block';
        result.innerHTML = `
            <div class="tr-result-card">
                <h4>✅ 翻译完成</h4>
                <div class="tr-result-stats">
                    <div><span class="tr-stat-num">${report.translated_files}</span><span class="tr-stat-label">已翻译文件</span></div>
                    <div><span class="tr-stat-num">${report.translated_lines}</span><span class="tr-stat-label">已翻译行数</span></div>
                    <div><span class="tr-stat-num">${report.skipped}</span><span class="tr-stat-label">跳过</span></div>
                </div>
                <p class="tr-result-hint">原文已备份为 .bak 文件, 翻译已写回游戏文件。</p>
            </div>
        `;
        showToast(`翻译完成: ${report.translated_files} 文件, ${report.translated_lines} 行`);
    } catch (e) {
        log.textContent = '翻译失败: ' + e;
        result.style.display = 'block';
        result.innerHTML = `<div class="tr-result-card tr-result-error">❌ 翻译失败: ${escapeHtml(String(e))}</div>`;
        console.error('游戏翻译失败:', e);
    } finally {
        btn.disabled = false;
        btn.textContent = '🚀 一键翻译';
    }
}

/// 翻译单条文本 (供游戏列表/详情页调用)
async function gameTrTranslate(text, source = 'auto') {
    if (!text || !text.trim()) return text;
    try {
        return await invoke('game_tr_translate', { text, source });
    } catch (e) {
        console.warn('game_tr_translate 失败:', e);
        return text;
    }
}

/// 批量翻译游戏列表名 (返回译文数组, 失败时返回原文)
async function gameTrTranslateBatch(texts, source = 'auto') {
    if (!texts || texts.length === 0) return [];
    try {
        return await invoke('game_tr_translate_batch', { texts, source });
    } catch (e) {
        console.warn('game_tr_translate_batch 失败:', e);
        return texts;
    }
}

/// 判断文本是否需要翻译 (英文/俄文等非中文文本)
function needsTranslation(text) {
    if (!text) return false;
    const lang = (window.currentTranslateLang || 'zh-CN');
    return !isTargetLang(text, lang);
}

window.browseTranslatorDir = browseTranslatorDir;
window.launchGameTranslator = launchGameTranslator;
window.loadTranslatorConfig = loadTranslatorConfig;
window.gameTrTranslate = gameTrTranslate;
window.gameTrTranslateBatch = gameTrTranslateBatch;
window.needsTranslation = needsTranslation;

// ============================================================
// 资源导航页 (从 SteamToolbox 提取的资源站点导航)
// ============================================================
const NAV_RESOURCES = [
    {
        section: '游戏资源',
        items: [
            { name: 'Galgamex (GX)', icon: '🎮', url: 'https://www.galgamex.net' },
            { name: 'Byrut', icon: '🎮', url: 'https://byrutgame.org' },
            { name: 'Koyso (Playzip)', icon: '🎮', url: 'https://playzip.com' },
        ],
    },
    {
        section: '官方',
        items: [
            { name: 'Steam 商店', icon: '🎮', url: 'https://store.steampowered.com' },
            { name: 'Steam 社区', icon: '🎮', url: 'https://steamcommunity.com' },
        ],
    },
    {
        section: '数据库',
        items: [
            { name: 'SteamDB', icon: '📊', url: 'https://steamdb.info' },
        ],
    },
    {
        section: '菜玩社区',
        items: [
            { name: '菜玩社区', icon: '🌐', url: 'https://caigamer.cn' },
        ],
    },
    {
        section: '论坛',
        items: [
            { name: 'CS.RIN.RU', icon: '💬', url: 'https://cs.rin.ru/forum/' },
            { name: '70XPLAY论坛', icon: '💬', url: 'https://70xplay.com' },
            { name: '3DM论坛', icon: '💬', url: 'https://bbs.3dmgame.com' },
            { name: '修改论坛', icon: '💬', url: 'https://fearlessrevolution.com/' },
        ],
    },
    {
        section: '联机',
        items: [
            { name: 'online-fix补丁', icon: '🌐', url: 'https://online-fix.me' },
            { name: 'freetp.org补丁', icon: '🌐', url: 'https://freetp.org' },
            { name: 'REXAGAMES', icon: '🌐', url: 'https://rexagames.com/tags/online/' },
        ],
    },
    {
        section: 'MOD',
        items: [
            { name: '3DM MOD站', icon: '🔧', url: 'https://mod.3dmgame.com' },
        ],
    },
    {
        section: '画质增强',
        items: [
            { name: 'DLSS5 Swapper', icon: '✨', url: 'https://github.com/rakanki911/DLSS5-Swapper' },
            { name: 'Veyra', icon: '🎬', url: 'https://github.com/Likely7/Veyra-NRVideo' },
            { name: '大力喜鹊 Magpie', icon: '🐦', url: 'https://github.com/Blinue/Magpie' },
        ]
    },
    {
        section: '平台',
        items: [
            { name: 'Epic Games', icon: '🖥️', url: 'https://www.epicgames.com/store/' },
            { name: 'GOG 平台', icon: '🖥️', url: 'https://www.gog.com/' },
            { name: 'Ubisoft', icon: '🖥️', url: 'https://store.ubi.com/' },
            { name: 'EA App', icon: '🖥️', url: 'https://www.ea.com/ea-app' },
            { name: 'Xbox PC', icon: '🖥️', url: 'https://www.xbox.com/zh-CN/xbox-game-pass/pc-games' },
        ],
    },
    {
        section: '追番',
        items: [
            { name: 'Animeko', icon: '🌸', url: 'https://github.com/open-ani/animeko' },
        ],
    },
    {
        section: '免费',
        items: [
            { name: 'Epic 免费', icon: '🆓', url: 'https://www.epicgames.com/store/free-games' },
        ],
    },
    {
        section: 'NS模拟器',
        items: [
            { name: '龙神 Ryujinx', icon: '🕹️', url: 'https://git.ryujinx.app/ryubing/ryujinx' },
            { name: 'Citron', icon: '🕹️', url: 'https://git.citron-emu.org/Citron' },
            { name: 'Eden', icon: '🕹️', url: 'https://github.com/eden-emulator/Releases' },
            { name: '模拟器固件', icon: '🕹️', url: 'https://github.com/THZoria/NX_Firmware/releases' },
            { name: '模拟器密钥', icon: '🕹️', url: 'https://caigamer.cn/archives/1793.htm' },
        ],
    },
    {
        section: 'PS模拟器',
        items: [
            { name: 'shadPS4', icon: '🎮', url: 'https://shadps4.net' },
            { name: 'KytyPS5', icon: '🎮', url: 'https://github.com/Nmzik/KytyPS5' },
            { name: 'sharpemu', icon: '🎮', url: 'https://github.com/sharpemu/sharpemu' },
        ],
    },
    {
        section: 'BIAO网盘资源集合',
        items: [
            { name: 'BIAO网盘资源', icon: '☁️', url: 'https://pan.quark.cn/s/77b60755f7b1' },
        ],
    },
    {
        section: 'GOG白嫖',
        items: [
            { name: 'GOG白嫖', icon: '🎮', url: 'https://gog-games.to' },
        ],
    },
    {
        section: '游戏',
        items: [
            { name: '114离线资源文档', icon: '🎮', url: 'https://www.114game.net' },
            { name: '52tt', icon: '🎮', url: 'https://www.52tt.com' },
            { name: 'Gamer520', icon: '🎮', url: 'https://www.gamer520.com' },
            { name: '2468游戏', icon: '🎮', url: 'https://2468c.com' },
            { name: 'KOYSO备份', icon: '🎮', url: 'https://koysobackup.com' },
        ],
    },
    {
        section: '购买',
        items: [
            { name: 'SteamPY', icon: '🛒', url: 'https://steampy.com' },
            { name: '杉果', icon: '🛒', url: 'https://www.sonkwo.cn' },
            { name: 'Humble', icon: '🛒', url: 'https://zh.humblebundle.com' },
            { name: '凤凰游戏', icon: '🛒', url: 'https://www.fhyx.com' },
            { name: '小黑盒', icon: '🛒', url: 'https://www.xiaoheihe.cn/app/topic/game/pc/' },
        ],
    },
    {
        section: 'D加密',
        items: [
            { name: 'D加密列表', icon: '🔐', url: 'https://www.pcgamingwiki.com/wiki/Denuvo#List_of_games_using_Denuvo_Anti-Tamper' },
            { name: 'Denuvo购买', icon: '🔐', url: 'https://aws.amazon.com/marketplace/pp/prodview-x443idlstvufi' },
        ],
    },
    {
        section: '游戏绕过补丁',
        items: [
            { name: '育碧补丁', icon: '🛠️', url: 'https://pan.quark.cn/s/8c9cd98372f5' },
            { name: 'EA补丁', icon: '🛠️', url: 'https://pan.quark.cn/s/a5df5ebc9f1d' },
            { name: '虚拟机补丁集合', icon: '🛠️', url: 'https://pan.quark.cn/s/81ac5631e78a' },
            { name: '免R星补丁', icon: '🛠️', url: 'https://pan.quark.cn/s/d67e24408473' },
        ],
    },
    {
        section: '聊天',
        items: [
            { name: 'X (Twitter)', icon: '💬', url: 'https://x.com/home' },
            { name: 'Discord', icon: '💬', url: 'https://discord.gg/gaming' },
            { name: 'Telegram', icon: '💬', url: 'https://t.me/steam_games' },
            { name: 'QQ群1', icon: '💬', url: 'https://qm.qq.com/q/1FLxfvkWx2' },
            { name: 'QQ群2', icon: '💬', url: 'https://qm.qq.com/q/IgSFee6ZgW' },
            { name: 'QQ群3', icon: '💬', url: 'https://qm.qq.com/q/IukVxW6H4e' },
        ],
    },
    {
        section: '老外清单库',
        items: [
            { name: 'hubcapmanifest', icon: '📋', url: 'https://hubcapmanifest.com/' },
            { name: 'KernelOS Games', icon: '📋', url: 'https://kernelos.org/games/' },
            { name: 'Luatools', icon: '📋', url: 'https://lua.tools/' },
            { name: 'Ryuu Generator', icon: '📋', url: 'https://generator.ryuu.lol/' },
            { name: 'DepotBox', icon: '📋', url: 'https://depotbox.org/' },
            { name: 'gamegen', icon: '📋', url: 'https://gamegen.lol/' },
        ],
    },
    {
        section: 'Manifest',
        items: [
            { name: 'ManifestHub', icon: '📦', url: 'https://manifesthub2.filegear-sg.me/' },
            { name: 'DepotCN', icon: '📦', url: 'https://depotcn.caigamer.cn/manifest' },
            { name: '20770407', icon: '📦', url: 'https://20770407.xyz/manifest' },
        ],
    },
    {
        section: 'SteamToolbox',
        items: [
            { name: 'GitHub仓库', icon: '📦', url: 'https://github.com/BIAO-001/SteamToolbox' },
            { name: 'OpenSteamTool', icon: '📦', url: 'https://github.com/OpenSteam001/OpenSteamTool' },
            { name: '腾讯文档', icon: '📦', url: 'https://docs.qq.com/doc/DRkN1b1VueXVJUGdT' },
        ],
    },
];

// ================================================================
// 内置浏览器已移除。替代方式: 资源导航点击 → 直接打开系统默认浏览器 (无弹窗、无其他方式)
// ================================================================
function _switchToDownloadsTab() {
    try {
        document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
        const tab = document.querySelector('[data-page="downloads"]');
        if (tab) tab.classList.add('active');
        document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
        const pd = document.getElementById('page-downloads');
        if (pd) pd.classList.add('active');
    } catch (_) {}
}

async function showExternalLinkChoice(url, extra) {
    // 用户明确要求: 资源导航/所有打开方式 → 只走系统默认浏览器, 其他方式(下载/复制)全部删除
    if (!url) return;
    try {
        if (typeof openExternal === 'function') { await openExternal(url); }
        else { window.open(String(url), '_blank'); }
    } catch (e) {
        try { window.open(String(url), '_blank'); } catch (_) {
            alert('打开系统浏览器失败: ' + (e && e.message ? e.message : String(e)));
        }
    }
}

// 残留快捷方式/地址栏绑定 (如果有对应 DOM 就绑定, 否则什么都不做) → 统一直接调系统默认浏览器
function _bindLeftoverShortcuts() {
    try {
        const urlInput = document.getElementById('browser-url-input');
        const openBtn = document.getElementById('browser-open-btn');
        if (openBtn && urlInput) {
            openBtn.addEventListener('click', () => {
                const url = urlInput.value.trim();
                if (url) openBrowserWindow(url);
            });
            urlInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') openBtn.click(); });
        }
        document.querySelectorAll('.browser-shortcut').forEach(el => {
            el.addEventListener('click', () => {
                const url = el.dataset.url;
                if (url) openBrowserWindow(url);
            });
        });
    } catch (_) {}
}

// 本地下载历史 fallback (无浏览器接口时显示 localStorage + state)
async function _showLocalHistoryFallback() {
    // 如果已有本地历史弹窗 (id: browser-history-modal) 就不重复创建 (和原函数保持相同 DOM id)
    const existing = document.getElementById('browser-history-modal');
    if (existing) { existing.remove(); return; }
    const DL_KEY = 'vortex_downloads_v2';
    const seen = new Set();
    const merged = [];
    try {
        const raw = localStorage.getItem(DL_KEY);
        if (raw) {
            const arr = JSON.parse(raw);
            if (Array.isArray(arr)) {
                for (let i = arr.length - 1; i >= 0; i--) {
                    const it = arr[i]; if (!it) continue;
                    const u = String(it.url || '');
                    const k = u + '|' + String(it.name || '');
                    if (seen.has(k)) continue;
                    seen.add(k);
                    merged.push({
                        url: u, filename: it.name || '', total: it.total || 0,
                        downloaded: it.downloaded || 0, status: it.state || 'completed',
                        timestamp: Math.floor((it.savedAt || Date.now())/1000), _src: 'storage', _savedAt: it.savedAt || 0,
                    });
                }
            }
        }
    } catch(_) {}
    for (const [,dl] of (state.downloads || new Map())) {
        if (!dl) continue;
        const u = String(dl.url || '');
        const k = u + '|' + String(dl.name || '');
        if (seen.has(k)) continue;
        seen.add(k);
        const st = String(dl.state || 'running');
        merged.push({
            url: u, filename: dl.name || '', total: dl.total || 0,
            downloaded: dl.downloaded || 0, status: st,
            timestamp: Math.floor((dl._savedAt || Date.now())/1000), _src: 'state',
        });
    }
    merged.sort((a,b) => {
        const ta = (a._savedAt || a.timestamp*1000) || 0;
        const tb = (b._savedAt || b.timestamp*1000) || 0;
        return tb - ta;
    });
    const history = merged.slice(0, 100);
    // 复用 showBrowserDownloadHistory 简化版弹窗 (与原 id 相同): 用原生 alert 之外更友好的 DOM 弹窗
    const overlay = document.createElement('div');
    overlay.id = 'browser-history-modal';
    overlay.style.cssText = 'position:fixed;inset:0;background:rgba(0,0,0,0.55);z-index:999999;display:flex;align-items:center;justify-content:center;';
    const box = document.createElement('div');
    box.style.cssText = 'background:#22272e;color:#e6edf3;border:1px solid #444c56;border-radius:10px;width:620px;max-width:94vw;max-height:72vh;display:flex;flex-direction:column;overflow:hidden;';
    const header = document.createElement('div');
    header.style.cssText = 'display:flex;justify-content:space-between;align-items:center;padding:12px 16px;border-bottom:1px solid #373e47;';
    header.innerHTML = `<div style="font-weight:700;font-size:14px;">📜 下载历史 <span style="color:#8b949e;font-weight:400;font-size:12px;">(${history.length} 条, 本地记录)</span></div>`;
    const actions = document.createElement('div');
    actions.style.cssText = 'display:flex;gap:6px;';
    const clearBtn = document.createElement('button');
    clearBtn.textContent = '清空显示';
    clearBtn.style.cssText = 'background:#da3633;color:#fff;border:none;padding:4px 10px;border-radius:5px;cursor:pointer;font-size:12px;';
    clearBtn.onclick = () => {
        try { localStorage.removeItem(DL_KEY); } catch(_) {}
        try { overlay.remove(); } catch(_) {}
        showToast('已清空本地历史显示缓存 (主下载任务未动)', 'success', 2200);
    };
    const closeBtn = document.createElement('button');
    closeBtn.textContent = '✕';
    closeBtn.style.cssText = 'background:none;border:none;color:#8b949e;font-size:16px;cursor:pointer;padding:2px 6px;';
    closeBtn.onclick = () => overlay.remove();
    actions.appendChild(clearBtn);
    actions.appendChild(closeBtn);
    header.appendChild(actions);
    box.appendChild(header);
    const list = document.createElement('div');
    list.style.cssText = 'overflow-y:auto;padding:8px 4px;flex:1;';
    const fmtSize = (n) => {
        const v = Number(n)||0;
        if (v<=0) return '';
        if (v<1024) return v+' B';
        if (v<1048576) return (v/1024).toFixed(1)+' KB';
        if (v<1073741824) return (v/1048576).toFixed(1)+' MB';
        return (v/1073741824).toFixed(2)+' GB';
    };
    const trStatus = (s) => {
        const x = String(s||'').toLowerCase();
        if (['in_progress','downloading','running','connecting','starting'].includes(x)) return '下载中';
        if (['completed','success','finished','done'].includes(x)) return '已完成';
        if (['failed','error','fail'].includes(x)) return '失败';
        if (['canceled','cancelled','aborted','stopped'].includes(x)) return '已取消';
        if (['paused','suspended'].includes(x)) return '已暂停';
        if (['extracting'].includes(x)) return '解压中';
        return String(s||'未知');
    };
    const fmtDate = (ts) => {
        if (!ts) return '';
        const t = Number(ts)>1e12 ? Number(ts) : Number(ts)*1000;
        try { return new Date(t).toLocaleString(); } catch(_) { return String(ts); }
    };
    if (history.length === 0) {
        list.innerHTML = '<div style="text-align:center;color:#8b949e;padding:24px;font-size:13px;">暂无下载记录</div>';
    } else {
        const urlsInMain = new Set();
        for (const [,d] of state.downloads) if (d && d.url) urlsInMain.add(d.url);
        history.forEach(item => {
            const row = document.createElement('div');
            row.style.cssText = 'padding:8px 12px;border-bottom:1px solid rgba(255,255,255,0.05);font-size:13px;display:flex;gap:8px;justify-content:space-between;align-items:flex-start;';
            const info = document.createElement('div');
            info.style.cssText = 'flex:1;min-width:0;';
            const fn = String(item.filename || '(无文件名)');
            const raw = String(item.status||'').toLowerCase();
            const done = ['completed','success','finished','done'].includes(raw);
            const failed = ['failed','error','fail'].includes(raw);
            const color = done ? '#2da44e' : (failed ? '#f85149' : '#d29922');
            info.innerHTML = `<div style="font-weight:600;word-break:break-all;margin-bottom:2px;">${escapeHtml(fn.length>80?fn.slice(0,80)+'…':fn)}</div>
                <div style="color:#8b949e;font-size:11px;">
                    <span style="color:${color};font-weight:500;">${trStatus(item.status)}</span>
                    ${fmtSize(item.total)?' · '+escapeHtml(fmtSize(item.total)):''}
                    ${fmtDate(item.timestamp)?' · '+escapeHtml(fmtDate(item.timestamp)):''}
                    · <span style="color:#6e7681;">${item._src==='storage'?'本地存储':'内存'}</span>
                </div>
                ${item.url?`<div style="color:#6e7681;font-size:10px;word-break:break-all;margin-top:2px;" title="${escapeHtml(item.url)}">${escapeHtml(item.url.length>120?item.url.slice(0,120)+'…':item.url)}</div>`:''}`;
            const act = document.createElement('div');
            act.style.cssText = 'display:flex;flex-direction:column;gap:4px;flex-shrink:0;';
            const already = item.url && urlsInMain.has(item.url);
            const rb = document.createElement('button');
            if (already) {
                rb.textContent = '✓ 已在列表';
                rb.disabled = true;
                rb.style.cssText = 'background:#484f58;color:#fff;border:none;padding:4px 10px;border-radius:4px;cursor:not-allowed;font-size:11px;white-space:nowrap;';
            } else {
                rb.textContent = done ? '导入主列表' : '↓ 加入下载';
                rb.disabled = !item.url;
                rb.style.cssText = (done?'background:#1f6feb;':'background:#2da44e;font-weight:600;') + 'color:#fff;border:none;padding:4px 10px;border-radius:4px;cursor:pointer;font-size:11px;white-space:nowrap;' + (item.url?'':'opacity:0.5;cursor:not-allowed;');
                rb.onclick = async () => {
                    if (!item.url) return;
                    rb.disabled = true; rb.textContent = '处理中...';
                    try {
                        await showExternalLinkChoice(item.url, { filename: item.filename, total: item.total });
                        try { overlay.remove(); } catch(_) {}
                    } catch (e) {
                        rb.disabled = false; rb.textContent = '重试';
                        alert('失败: ' + (e && e.message ? e.message : String(e)));
                    }
                };
            }
            act.appendChild(rb);
            if (item.url) {
                const cb = document.createElement('button');
                cb.textContent = '复制链接';
                cb.style.cssText = 'background:#484f58;color:#fff;border:none;padding:3px 10px;border-radius:4px;cursor:pointer;font-size:10px;white-space:nowrap;';
                cb.onclick = async () => {
                    try { await invoke('copy_to_clipboard', { text: item.url }); showToast('链接已复制', 'success'); }
                    catch (_) { try { await navigator.clipboard.writeText(item.url); showToast('链接已复制', 'success'); } catch(e){ alert('复制失败:\n'+item.url); } }
                };
                act.appendChild(cb);
            }
            row.appendChild(info);
            row.appendChild(act);
            list.appendChild(row);
        });
    }
    box.appendChild(list);
    overlay.appendChild(box);
    overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };
    document.body.appendChild(overlay);
}

// 打开浏览器窗口 (内置浏览器已移除 → 直接打开系统默认浏览器, 不弹窗、不提供其他方式)
let _lastOpenBrowser = { url: '', ts: 0 };
async function openBrowserWindow(url) {
    if (!url) return;
    const now = Date.now();
    if (_lastOpenBrowser.url === url && now - _lastOpenBrowser.ts < 300) return; // 300ms 同URL去抖动
    _lastOpenBrowser = { url, ts: now };
    try {
        if (typeof openExternal === 'function') { await openExternal(url); }
        else { window.open(String(url), '_blank'); }
    } catch (e) {
        // 最后兜底: 直接 window.open
        try { window.open(String(url), '_blank'); } catch (_) {
            alert('打开系统浏览器失败: ' + (e && e.message ? e.message : String(e)));
        }
    }
}

// 浏览器页初始化 (内置浏览器已移除 → 绑定残留地址栏/快捷方式到 openBrowserWindow, browser-show-history 事件改为本地历史)
function initBrowserPage() {
    if (window.__initBrowserPageDone) return;
    window.__initBrowserPageDone = true;
    // 地址栏 + 快捷方式绑定 → 直接调系统默认浏览器
    _bindLeftoverShortcuts();
    // 如果还会收到 browser-show-history 事件 → 改为显示本地历史 fallback
    try {
        if (window.__TAURI__ && window.__TAURI__.event && typeof window.__TAURI__.event.listen === 'function') {
            window.__TAURI__.event.listen('browser-show-history', () => _showLocalHistoryFallback());
        }
    } catch (_) {}
}

// 显示浏览器下载历史弹窗 (内置浏览器已移除 → 直接显示本地 localStorage+state 下载历史 fallback)
async function showBrowserDownloadHistory() {
    await _showLocalHistoryFallback();
}

// aria2 开关切换事件 (实时预览状态, 自动保存会触发实际启停)
document.addEventListener('DOMContentLoaded', () => {
    // ★ 这两个 id 在 HTML 里已经不存在了（内置浏览器 / aria2 面板已移除），
    //   `if (toggle)` 会直接跳过 —— 保留是为了兼容可能的老配置文件，不会报错。
    const toggle = document.getElementById('use-aria2-toggle');
    if (toggle) {
        toggle.addEventListener('change', () => {
            // 刷新提示文案 (实际启停由 autoSaveSettings 在 400ms 后触发)
            const badge = document.getElementById('aria2-status-badge');
            if (badge) {
                if (toggle.checked) {
                    badge.textContent = '○ 启动中...';
                    badge.style.color = '#ffa726';
                } else {
                    badge.textContent = '○ 停止中...';
                    badge.style.color = '#888';
                }
            }
        });
    }
});

// initBrowserPage() — (内置浏览器已移除 → init 流程已改为由主init统一调用, 这里不再额外提前触发)

async function hotRefreshIndexOnStart() {
    const invokeFn = (typeof window.__TAURI__ !== 'undefined' && window.__TAURI__.core && window.__TAURI__.core.invoke)
        ? window.__TAURI__.core.invoke
        : (typeof window.__TAURI__ !== 'undefined' && window.__TAURI__.invoke ? window.__TAURI__.invoke : null);
    if (!invokeFn) return;
    try {
        const getRealTotal = (stats) => {
            if (!stats || !Array.isArray(stats) || stats.length < 5) return 0;
            const by = Math.max(Number(stats[0]) || 0, Number(stats[4]) || 0);
            const ko = Math.max(Number(stats[1]) || 0, Number(stats[5]) || 0);
            const gx = Number(stats[2]) || 0;
            return by + ko + gx;
        };

        let oldStats = null;
        try { oldStats = await invokeFn('snapshot_stats'); } catch (_e) { oldStats = null; }
        const oldTotal = getRealTotal(oldStats);
        const t0 = Date.now();

        await Promise.all([
            invokeFn('preload_byko').catch(() => {}),
            invokeFn('refresh_snapshot').catch(() => {}),
        ]);

        let newStats = null;
        try { newStats = await invokeFn('snapshot_stats'); } catch (_e) { newStats = null; }
        const newTotal = getRealTotal(newStats);
        const diff = newTotal - oldTotal;
        const used = (Date.now() - t0) / 1000;

        // 主页游戏计数立即更新
        try { if (typeof updateHomeStats === 'function') updateHomeStats(); } catch (_e) {}

        // (已禁用 showToast —— 用户不要求显示热爬结果提示)

        // 有新增且当前在"游戏资源"第一页 → 立即重新渲染 (新增资源按 update_time DESC 会排最前)
        if (diff > 0 && state.currentPage === 1) {
            try {
                // ★ 修 (2026-10-08)：HTML 里的 section 是 `page-resources`（复数），
                //   这里原来写成 `page-resource` → getElementById 永远返回 null →
                //   "后台拉到新数据时自动重载资源列表"这条**从来没生效过**（静默跳过）。
                const pg = document.getElementById('page-resources');
                if (pg && pg.classList.contains('active')) {
                    loadGamesReset();
                }
            } catch (_e) {}
        }
    } catch (e) {
        console.error('[hotRefreshIndexOnStart] 失败:', e);
    }
}


// SwiftFetch 引擎开关切换事件
function updateHomeStats() {
    // 资源站点数量 = 读取 NAV_RESOURCES 中所有网址条目总数
    let sourcesCount = 0;
    if (typeof NAV_RESOURCES !== 'undefined' && Array.isArray(NAV_RESOURCES)) {
        sourcesCount = NAV_RESOURCES.reduce((sum, section) => sum + (Array.isArray(section.items) ? section.items.length : 0), 0);
    }
    if (sourcesCount <= 0) sourcesCount = 8;
    const sourcesEl = document.getElementById('stat-sources-count');
    if (sourcesEl) sourcesEl.textContent = sourcesCount;

    // ★ 工具页已删除 (2026-09-13): 不再调用 updateToolCount()

    // 璁＄畻娓告垙璧勬簮鏁伴噺
    const gamesEl = document.getElementById('stat-games-count');
    if (gamesEl) {
        const savedGames = JSON.parse(localStorage.getItem('game_list') || '[]');
        if (savedGames.length > 0) {
            gamesEl.textContent = savedGames.length;
        }
    }
}
document.addEventListener('DOMContentLoaded', () => {
    // 加载并应用保存的主题 (默认深海蓝)
    const savedTheme = localStorage.getItem('vortex_theme') || 'ocean';
    applyTheme(savedTheme);

    // 收藏数量角标 + 清空按钮
    favUpdateBadge();
    document.addEventListener('click', (e) => {
        const clearBtn = e.target.closest('#fav-clear-btn');
        if (!clearBtn) return;
        e.preventDefault();
        if (favLoad().length === 0) return;
        if (!confirm('确定清空所有收藏吗？')) return;
        favSave([]);
        renderFavPage();
    });

    // ★ 预加载优化: 打开软件后立即后台加载游戏列表 + 统计
    //    (用户点首页"游戏资源"卡片进入时就直接显示, 不用等 3-5 秒)
    try {
        // 1) 资源游戏预热: 只在 state.initialized=false 时跑 (避免重复)
        //    成人游戏已合并进主列表 (带标签), 由同一次预热覆盖
        if (!state.initialized) {
            Promise.resolve().then(() => loadResourceGames()).catch(err => {
                console.warn('[预加载] 资源列表预热失败:', err);
            });
        }
    } catch (e) { console.warn('[预加载] 异常:', e); }
});

// ============================================================
// 主题切换功能 (下拉框)
// ============================================================
function switchTheme(themeName) {
    // ★ 授权分级 (2026-10-02): 免费版只能用 3 个主题。
    //   这里的拦截是"第二道" —— 第一道是 applyThemeGating() 直接把这些
    //   option 从下拉里移除; 但程序化调用 (window.switchTheme) 也必须挡住。
    if (LICENSE && LICENSE.activated && !LICENSE.all_themes &&
        (LICENSE.themes || []).indexOf(themeName) === -1) {
        licenseToast('该主题需要付费版或开发者密钥');
        return;
    }
    applyTheme(themeName);
    localStorage.setItem('vortex_theme', themeName);
    
    // 显示切换反馈动画
    document.body.classList.add('theme-transitioning');
    setTimeout(() => {
        document.body.classList.remove('theme-transitioning');
    }, 500);
}

function applyTheme(themeName) {
    // ★ 主题清单以 DOM 里 <select id="theme-select"> 的选项为唯一来源 —— 不要再硬编码数组。
    //   曾经这里硬编码 41 个主题名, 结果新增主题后选中它们会被静默回退成 ocean。
    const selEl = document.getElementById('theme-select');
    const validThemes = selEl
        ? Array.prototype.map.call(selEl.options, function (o) { return o.value; })
        : ['purple'];
    if (!validThemes.includes(themeName)) {
        themeName = validThemes.indexOf('ocean') !== -1 ? 'ocean' : validThemes[0];
    }
    
    // purple 为默认主题, 不设置 data-theme 属性
    if (themeName === 'purple') {
        document.documentElement.removeAttribute('data-theme');
    } else {
        document.documentElement.setAttribute('data-theme', themeName);
    }
    
    // 更新下拉框选中状态
    const themeSelect = document.getElementById('theme-select');
    if (themeSelect && themeSelect.value !== themeName) {
        themeSelect.value = themeName;
    }
}

// 将 switchTheme 暴露给全局 onclick
window.switchTheme = switchTheme;

// 下拉框主题选择事件
document.addEventListener('DOMContentLoaded', () => {
    const themeSelect = document.getElementById('theme-select');
    if (themeSelect) {
        themeSelect.addEventListener('change', (e) => {
            switchTheme(e.target.value);
        });
    }
});

// ============================================================
// 性能模式（关毛玻璃 + 关入场动效）
// ------------------------------------------------------------
// ★ 界面上有 156 处 backdrop-filter：每一处都要把背后的内容重新采样一遍，
//   在集成显卡/老 CPU 上是最贵的一项，滚动和切页都会掉帧。
//   低配机器（≤4 核 或 内存 ≤4GB）首次启动自动开启，也可以在设置里手动开关。
// ============================================================
const PERF_MODE_KEY = 'vortex_perf_mode';
function applyPerfMode(on) {
    try { document.body.classList.toggle('lite-mode', !!on); } catch (e) {}
    const cb = document.getElementById('perf-mode-toggle');
    if (cb) cb.checked = !!on;
}
function initPerfMode() {
    let on = null;
    try {
        const saved = localStorage.getItem(PERF_MODE_KEY);
        if (saved === '1') on = true;
        else if (saved === '0') on = false;
    } catch (e) {}
    if (on === null) {
        // 首次启动：按硬件自动判定
        try {
            const cores = navigator.hardwareConcurrency || 8;
            const mem = navigator.deviceMemory || 8;
            on = (cores <= 4) || (mem <= 4);
            localStorage.setItem(PERF_MODE_KEY, on ? '1' : '0');
        } catch (e) { on = false; }
    }
    applyPerfMode(on);
}
initPerfMode();
document.addEventListener('DOMContentLoaded', () => {
    const cb = document.getElementById('perf-mode-toggle');
    if (cb) {
        cb.checked = document.body.classList.contains('lite-mode');
        cb.addEventListener('change', () => {
            const on = !!cb.checked;
            applyPerfMode(on);
            try { localStorage.setItem(PERF_MODE_KEY, on ? '1' : '0'); } catch (e) {}
            if (typeof showToast === 'function') {
                showToast(on ? '已开启性能模式（关闭毛玻璃与动效）' : '已关闭性能模式');
            }
        });
    }
});

// ============================================================
// 认证 / 离线密钥授权
// ------------------------------------------------------------
// 后端 (Rust licensing.rs) 才是权威判定: 它用内置公钥验 Ed25519 签名,
// 并维护"已用密钥"清单。这里只负责展示与拦截。
// 注意 start_download 在 Rust 侧也会再判一次 —— 前端可被绕过。
// ============================================================
let LICENSE = null;
// 主题下拉的完整选项快照 (免费版裁剪后仍能恢复)
let THEME_OPTIONS_MASTER = null;

function licenseToast(msg) {
    // 轻量提示: 优先复用页面已有的 toast, 没有就退化为控制台
    try {
        if (typeof window.showToast === 'function') { window.showToast(msg); return; }
    } catch (_) {}
    console.warn('[认证]', msg);
}

async function initLicense() {
    try {
        LICENSE = await invoke('license_status');
    } catch (e) {
        console.warn('[认证] 读取授权状态失败:', e);
        LICENSE = null;
    }
    renderLicenseUI();
    applyThemeGating();
    if (!LICENSE || !LICENSE.activated) showLicenseGate();
    else hideLicenseGate();
}

function licenseAllowsTheme(name) {
    if (!LICENSE || !LICENSE.activated) return true;
    if (LICENSE.all_themes) return true;
    return (LICENSE.themes || []).indexOf(name) !== -1;
}

function renderLicenseUI() {
    const info = LICENSE;
    const activated = !!(info && info.activated);
    const tier = activated ? info.tier : 'none';

    const badge = document.getElementById('license-badge');
    if (badge) {
        badge.textContent = activated ? info.tier_label : '未激活';
        badge.className = 'license-badge license-badge--' + tier;
    }
    const detail = document.getElementById('license-detail');
    if (detail) {
        detail.textContent = activated
            ? ('下载速度 ' + info.speed_percent + '% · ' +
               (info.all_themes ? '全部主题' : ('主题 ' + (info.themes || []).length + ' 个')))
            : '';
    }
    if (info && info.free_key) {
        const a = document.getElementById('license-free-key');
        if (a) a.textContent = info.free_key;
        const b = document.getElementById('license-gate-free');
        if (b) b.textContent = info.free_key;
    }
    // 设备码: 付费密钥就绑定在它上面
    const dev = document.getElementById('license-device');
    if (dev) dev.textContent = (info && info.device_code) ? info.device_code : '读取失败';
}

// 免费版把不允许的主题从下拉里直接移除 (而非置灰) —— 用户看到的就是"只有 3 个"
function applyThemeGating() {
    const sel = document.getElementById('theme-select');
    if (!sel) return;
    if (!THEME_OPTIONS_MASTER) {
        THEME_OPTIONS_MASTER = Array.prototype.map.call(sel.options, function (o) {
            return { value: o.value, text: o.textContent };
        });
    }
    const full = !!(LICENSE && LICENSE.activated && LICENSE.all_themes);
    const allowed = (LICENSE && LICENSE.themes) || [];
    const want = full
        ? THEME_OPTIONS_MASTER
        : THEME_OPTIONS_MASTER.filter(function (o) { return allowed.indexOf(o.value) !== -1; });

    if (want.length && sel.options.length !== want.length) {
        sel.innerHTML = '';
        want.forEach(function (o) {
            const opt = document.createElement('option');
            opt.value = o.value;
            opt.textContent = o.text;
            sel.appendChild(opt);
        });
    }
    // 当前主题若已被锁 → 切到第一个可用主题
    const cur = localStorage.getItem('vortex_theme') || 'ocean';
    if (want.length && !want.some(function (o) { return o.value === cur; })) {
        switchTheme(want[0].value);
    }
}

// ★ 2026-10-06 改版: 未激活**不再弹遮罩/黑框**要人输密钥,
//   改成底部一条小 toast 提示"尚未激活", 几秒后自动消失。
//   激活入口保留在「设置 → 认证」里, 不打断用户浏览。
let _gateToastTimer = null;
function showLicenseGate() {
    // 防抖: 重复调用只刷一次 toast
    if (_gateToastTimer) { clearTimeout(_gateToastTimer); _gateToastTimer = null; }
    try {
        if (typeof window.showToast === 'function') {
            window.showToast('尚未激活 —— 可前往「设置 → 认证」输入密钥', 'warn', 4200);
        }
    } catch (_) {}
    _gateToastTimer = setTimeout(function () { _gateToastTimer = null; }, 4000);
}
function hideLicenseGate() {
    // 已激活 → 不需要提示, 什么都不做 (保留函数名以免调用点报错)
}

// 激活。返回 { ok, msg }
async function activateLicense(rawKey) {
    const key = (rawKey || '').trim();
    if (!key) return { ok: false, msg: '请输入密钥' };
    try {
        LICENSE = await invoke('license_activate', { key: key });
        renderLicenseUI();
        applyThemeGating();
        hideLicenseGate();
        return { ok: true, msg: '' };
    } catch (e) {
        const msg = (typeof e === 'string') ? e : ((e && e.message) || String(e));
        return { ok: false, msg: msg };
    }
}

function copyFreeKey() {
    const k = (LICENSE && LICENSE.free_key) || 'VXDL-FREE-8888';
    const done = function () { licenseToast('免费密钥已复制'); };
    try {
        if (navigator.clipboard && navigator.clipboard.writeText) {
            navigator.clipboard.writeText(k).then(done, function () { fallbackCopy(k, done); });
            return;
        }
    } catch (_) {}
    fallbackCopy(k, done);
}
function fallbackCopy(text, done) {
    try {
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
        if (done) done();
    } catch (e) { console.warn('[认证] 复制失败:', e); }
}
window.copyFreeKey = copyFreeKey;

document.addEventListener('DOMContentLoaded', () => {
    const gateBtn = document.getElementById('license-gate-btn');
    const gateInput = document.getElementById('license-gate-input');
    const gateMsg = document.getElementById('license-gate-msg');
    const setBtn = document.getElementById('license-activate-btn');
    const setInput = document.getElementById('license-key-input');
    const setMsg = document.getElementById('license-msg');
    const setMsgRow = document.getElementById('license-msg-row');

    function paint(el, ok, msg) {
        if (!el) return;
        el.textContent = msg || '';
        const base = el.id === 'license-gate-msg' ? 'license-gate-msg' : 'license-msg';
        el.className = base + (msg ? (ok ? ' ok' : ' err') : '');
    }

    async function run(inputEl, msgEl) {
        const r = await activateLicense(inputEl ? inputEl.value : '');
        paint(msgEl, r.ok, r.ok ? '' : r.msg);
        if (r.ok && inputEl) inputEl.value = '';
        return r;
    }

    if (gateBtn) gateBtn.addEventListener('click', function () { run(gateInput, gateMsg); });
    if (gateInput) gateInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); run(gateInput, gateMsg); }
    });
    if (setBtn) setBtn.addEventListener('click', async function () {
        if (setMsgRow) setMsgRow.style.display = '';
        await run(setInput, setMsg);
    });
    if (setInput) setInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') { e.preventDefault(); if (setBtn) setBtn.click(); }
    });

    initLicense();
});

// ============================================================
// ============================================================
// 运行库修复 (VC++ / DirectX / DLL 注册)
// ============================================================
document.getElementById('runtime-repair-btn')?.addEventListener('click', async () => {
    const btn = document.getElementById('runtime-repair-btn');
    const logBox = document.getElementById('runtime-repair-log');
    if (!btn) return;

    if (!confirm('即将开始运行库一键修复:\n\n' +
        '1. 静默安装 VC++ 运行库 (2005-2022 全版本)\n' +
        '2. 修复 DirectX 组件 (d3dx9_xx.dll / xinput1_3.dll)\n' +
        '3. 重新注册系统 DLL (修复 0xc000007b 等错误)\n\n' +
        '⚠️ 过程需要管理员权限, 可能需要几分钟\n' +
        '⚠️ 完成后需要重启电脑才能生效\n\n确认继续?')) return;

    btn.disabled = true;
    btn.textContent = '修复中...';
    if (logBox) {
        logBox.style.display = 'block';
        logBox.textContent = '[1/3] 正在安装 VC++ 运行库...\n';
    }

    try {
        const r = await invoke('repair_runtime', {});
        // 输出详细结果
        const lines = [];
        if (Array.isArray(r.results)) {
            for (const item of r.results) {
                const status = item.success ? '✅' : '❌';
                lines.push(`${status} ${item.name}: ${item.message}`);
            }
        }
        if (logBox) {
            logBox.textContent = lines.join('\n') + '\n\n修复流程已完成。';
        }

        const allOk = r.success;
        if (allOk) {
            // 全部成功 → 提示重启
            if (confirm('✅ 运行库修复全部完成!\n\n' +
                '已完成: VC++ 运行库安装 + DirectX 修复 + DLL 注册\n\n' +
                '⚠️ 必须重启电脑才能使所有修复生效。\n\n是否立即重启?')) {
                // 调用系统重启
                try {
                    await invoke('restart_system');
                } catch (e) {
                    alert('重启命令执行失败, 请手动重启电脑。\n\n错误: ' + e);
                }
            }
        } else {
            // 部分失败 → 仍提示重启
            if (confirm('⚠️ 运行库修复已完成 (部分步骤未成功, 详情见上方日志)。\n\n' +
                '⚠️ 仍建议重启电脑使已成功的修复生效。\n\n是否立即重启?')) {
                try {
                    await invoke('restart_system');
                } catch (e) {
                    alert('重启命令执行失败, 请手动重启电脑。\n\n错误: ' + e);
                }
            }
        }
    } catch (e) {
        if (logBox) {
            logBox.textContent = `修复失败: ${e}`;
        }
        alert(`运行库修复失败: ${e}`);
    } finally {
        btn.textContent = '🔧 运行库一键修复';
        btn.disabled = false;
    }
});

// initBrowserPage() — (内置浏览器已移除 → 由 init() STEP24 统一触发)

// 在系统默认浏览器打开外部链接
// 统一走后端 open_folder 命令 (内部用 cmd /c start 调用系统默认浏览器)
// shell.open 在 Tauri 2.x 受 capabilities 权限限制, 容易静默失败, 不再优先使用
async function openExternal(url) {
    console.log('[openExternal] 尝试打开:', url);
    // 优先走后端命令 (最可靠, 不受前端权限限制)
    if (window.__TAURI__ && window.__TAURI__.core) {
        try {
            await invoke('open_folder', { path: url });
            console.log('[openExternal] 后端命令成功');
            return;
        } catch (e) {
            console.warn('[openExternal] 后端命令失败, 尝试 shell.open:', e);
        }
    }
    // 降级: shell 插件
    if (window.__TAURI__ && window.__TAURI__.shell && window.__TAURI__.shell.open) {
        try {
            await window.__TAURI__.shell.open(url);
            console.log('[openExternal] shell.open 成功');
            return;
        } catch (e) {
            console.warn('[openExternal] shell.open 失败, 尝试 window.open:', e);
        }
    }
    // 最终降级
    window.open(url, '_blank');
}
window.openExternal = openExternal;

// ============================================================
// 资源浏览/搜索(无限滚动)
// ============================================================
// 包装函数: 标签页切换时调用
function loadResourceGames() {
    loadGamesReset();
}

// 防重入锁: init() 的 loadGamesReset 与用户切换视图触发的 loadResourceGames 可能并发,
// 导致 grid 被二次覆盖为 "加载中..." 而第二次 loadGamesAppend 因 infiniteScrollLoading=true 直接 return,
// 最终 grid 残留 "加载中..." 文字 (用户看到 "没有资源"), 手动刷新才正常。
// 修复: 并发调用时复用同一个 Promise, 不重复设置 grid / 不重复触发 loadGamesAppend。
let _loadGamesResetPromise = null;

function loadGamesReset() {
    if (_loadGamesResetPromise) {
        return _loadGamesResetPromise;
    }
    _loadGamesResetPromise = _doLoadGamesReset().finally(() => {
        _loadGamesResetPromise = null;
    });
    return _loadGamesResetPromise;
}

// 首次加载或切换筛选条件时重置列表
async function _doLoadGamesReset() {
    // ★ 修复 (2026-10-03): 用 generation token 作废"正在飞行中"的旧请求。
    //
    //   原实现只清了内容与去重集合, 却**没有重置 state.infiniteScrollLoading**。
    //   而 loadGamesAppend 开头有 `if (state.infiniteScrollLoading) return;` ——
    //   于是当无限滚动的那次请求还没回来时 (用户此时输入搜索词 / 切分类),
    //   重置流程自己那一发请求被这个守卫直接跳过, 网格停在"加载中..."或空白;
    //   随后旧请求 resolve, 把**上一个查询的结果**追加进刚清空的网格,
    //   并把 currentPage 往前推 —— 用户看到的是过期内容, 且页码错乱。
    //
    //   token 的作用: 每次 reset 自增一次; loadGamesAppend 在发请求前记下当时的
    //   token, 拿到结果后若 token 已变 (说明期间发生过 reset/换查询), 就丢弃这批
    //   结果 —— 从根上避免过期数据污染新查询。
    state._listGen = (state._listGen || 0) + 1;
    state.infiniteScrollLoading = false;   // 允许本次重置自己那一发请求通过守卫
    state.currentPage = 1;
    state.infiniteScrollEnd = false;
    state.searchBuffer = null;
    state.searchBufferOffset = 0;
    state.emptyPageRun = 0;
    const grid = document.getElementById('game-grid');
    grid.innerHTML = '<div class="loading">加载中...</div>';
    _gameGridSeenIds.clear();
    await loadGamesAppend();
    state.initialized = true;
}

// 追加加载下一页(无限滚动用)
async function loadGamesAppend() {
    if (state.infiniteScrollLoading || state.infiniteScrollEnd) return;
    state.infiniteScrollLoading = true;
    // ★ 记下本次请求所属的"世代"; await 之后若已换代, 说明期间发生了
    //   reset (换搜索词/切分类), 这批结果必须丢弃, 否则会把旧查询的数据
    //   追加进已经被清空的网格 (见 _doLoadGamesReset 的说明)。 (2026-10-03)
    const _gen = state._listGen || 0;

    const grid = document.getElementById('game-grid');
    const loader = document.getElementById('scroll-loader');
    // 首次加载时清空 "加载中..."
    if (state.currentPage === 1) {
        grid.innerHTML = '';
        _gameGridSeenIds.clear();
    }
    // 显示加载中
    if (loader) {
        loader.innerHTML = '<div class="loading">加载中...</div>';
    }

    try {
        let games;
        // 有分类筛选(非"全部类型")且无关键词: 走 search 路径
        // 后端在空关键词+有分类时从 BY/KO 内存缓存按分类筛选, KO 优先排序
        if (state.currentKeyword || state.currentCategory !== '全部类型') {
            games = await invoke('search', {
                keyword: state.currentKeyword,
                source: state.currentSource,
                category: state.currentCategory,
            });
            // ★ 性能: 搜索/分类结果可能数千条, 一次性渲染会瞬间创建数千 DOM 节点导致卡顿
            //   改为分批渲染: 缓存全量结果, 每次渲染 60 个, 滚动到底继续渲染下一批
            state.searchBuffer = Array.isArray(games) ? games : [];
            state.searchBufferOffset = 0;
            if ((!games || games.length === 0) && state.currentPage === 1) {
                // ★ 有关键词搜索: 结果为空直接显示"无搜索结果", 不重试
                //   无关键词仅分类筛选: 缓存可能未就绪, 延迟重试 (最多 60 次, 共 48 秒)
                if (state.currentKeyword && state.currentKeyword.trim()) {
                    grid.innerHTML = '<div class="loading">未找到相关游戏</div>';
                    if (loader) { loader.innerHTML = ''; }
                    state.infiniteScrollLoading = false;
                    return;
                }
                if (state.categoryRetryCount < 60) {
                    state.categoryRetryCount++;
                    grid.innerHTML = '<div class="loading">正在加载游戏，请稍候...</div>';
                    if (loader) { loader.innerHTML = ''; }
                    state.infiniteScrollLoading = false;
                    setTimeout(() => { loadGamesAppend(); }, 800);
                    return;
                }
                // 超过重试上限: 缓存已就绪但该分类无资源
                grid.innerHTML = '<div class="loading">该分类暂无资源</div>';
                if (loader) { loader.innerHTML = ''; }
                state.infiniteScrollLoading = false;
                return;
            }
            // 渲染本批 60 个; 还有剩余时保持无限滚动继续取批
            const batch = state.searchBuffer.slice(state.searchBufferOffset, state.searchBufferOffset + 60);
            state.searchBufferOffset += batch.length;
            state.infiniteScrollEnd = state.searchBufferOffset >= state.searchBuffer.length;
            appendGameGrid(batch);
            if (state.infiniteScrollEnd) {
                if (loader) { loader.innerHTML = '<div class="loading">已加载全部</div>'; }
            } else if (loader) {
                loader.innerHTML = '<div class="loading">向下滚动加载更多...</div>';
            }
            state.infiniteScrollLoading = false;
            return;
        }
        // 无筛选: 走 browse 分页路径
        games = await invoke('browse', {
            source: state.currentSource,
            page: state.currentPage,
        });
        console.log(`[DEBUG] browse source=${state.currentSource} page=${state.currentPage} 返回 ${games ? games.length : 0} 个`);
        if (!games || games.length === 0) {
            // 缓存可能正在后台加载, 首页延迟重试 (不限制次数, 直到缓存就绪)
            if (state.currentPage === 1) {
                grid.innerHTML = '<div class="loading">正在加载游戏，请稍候...</div>';
                if (loader) { loader.innerHTML = ''; }
                state.infiniteScrollLoading = false;
                // 800ms 后重试 (加快响应, 后台预加载通常 3-5 秒完成)
                setTimeout(() => {
                    loadGamesAppend();
                }, 800);
                return;
            }
            // ★ 后端过滤成人游戏后可能出现真空页: 跳页续取, 连续 3 页空才认为到底
            //   (否则列表会提前停止加载, 后续游戏永远显示不出来)
            state.emptyPageRun = (state.emptyPageRun || 0) + 1;
            if (state.emptyPageRun < 3) {
                state.currentPage++;
                state.infiniteScrollLoading = false;
                loadGamesAppend();
                return;
            }
            // 连续空页: 已到底
            state.infiniteScrollEnd = true;
            if (loader) {
                loader.innerHTML = '<div class="loading">已加载全部</div>';
            }
            state.infiniteScrollLoading = false;
            return;
        }

        // ★ 世代校验 (2026-10-03): 发请求期间若发生过 reset (换搜索词/切分类),
        //   这批结果已经过期 —— 直接丢弃, 否则会把旧查询的数据追加进刚清空的网格。
        if ((state._listGen || 0) !== _gen) {
            console.warn('[games] 丢弃过期结果 (期间已切换查询)');
            return;
        }
        appendGameGrid(games);
        state.emptyPageRun = 0; // 成功页: 重置连续空页计数
        state.currentPage++;
        // 加载完成后, 若未到底则提示继续滚动
        if (loader && !state.infiniteScrollEnd) {
            loader.innerHTML = '<div class="loading">向下滚动加载更多...</div>';
        }
    } catch (e) {
        if (state.currentPage === 1) {
            grid.innerHTML = `<div class="loading">加载失败: ${e}</div>`;
        }
        console.error('加载失败:', e);
        if (loader) {
            loader.innerHTML = `<div class="loading">加载失败, 点击重试</div>`;
            loader.onclick = () => { loader.onclick = null; loadGamesAppend(); };
        }
    } finally {
        state.infiniteScrollLoading = false;
    }
}

// ★ 性能优化 (2026-09-30): 用持久化 Set 记录已渲染卡片的去重键, 替代每次追加时
//   对全部已有卡片做 querySelectorAll 全量扫描 (O(N))。卡片规模上万时, 每次无限滚动
//   追加都要扫描全部节点, 是 "点击游戏卡一会 / 滚动卡顿" 的主因之一。
//   grid 被清空/重置时必须在对应位置同步 clear() (见 _doLoadGamesReset / loadGamesAppend
//   page-1 / loadAdultReset / init)。
const _gameGridSeenIds = new Set();
const _adultGridSeenIds = new Set();

// ★ 卡片点击性能优化: 用注册表存储 game 对象, onclick 只传 key,
//   避免每张卡片内联 JSON.stringify(game) (数百张卡片时是启动卡顿主因之一),
//   同时避免 source/detail_url 直接拼进 onclick 造成的注入风险
const _gameCardRegistry = new Map();
let _gameCardSeq = 0;
function _registerCardGame(game) {
    const key = 'gc_' + (++_gameCardSeq);
    _gameCardRegistry.set(key, game);
    // 防止长期运行无限增长: 超过阈值时丢弃最早的一半
    if (_gameCardRegistry.size > 2000) {
        let drop = 1000;
        for (const k of _gameCardRegistry.keys()) {
            if (drop-- <= 0) break;
            _gameCardRegistry.delete(k);
        }
    }
    return key;
}
// ============================================================
// 喜欢 (收藏) — 卡片右下角爱心 + 收藏页
// ------------------------------------------------------------
// 收藏里**不区分成人/普通游戏**: 统一按游戏处理, 点开进同一个介绍页
// (介绍页内部仍按"成人/普通 + 当前游戏标签"做推荐, 与现在一致)。
// 持久化用 localStorage, 键带版本号便于以后迁移。
// ============================================================
const FAV_STORE_KEY = 'vortexdl.favorites.v1';
// key -> game, 供爱心点击时反查 (卡片 DOM 上只放 key, 不放整个对象)
const _favCardGames = new Map();

function favKey(game) {
    if (!game) return '';
    const id = game.appid || game.name_original || game.name || '';
    const src = game.source || '';
    return `${src}::${id}`.toLowerCase();
}

function favLoad() {
    try {
        const raw = localStorage.getItem(FAV_STORE_KEY);
        const arr = raw ? JSON.parse(raw) : [];
        return Array.isArray(arr) ? arr : [];
    } catch (_) { return []; }
}

function favSave(list) {
    try { localStorage.setItem(FAV_STORE_KEY, JSON.stringify(list)); } catch (_) {}
    favUpdateBadge();
}

function isFav(key) {
    return favLoad().some(it => it && it.key === key);
}

/** 收藏 / 取消收藏。返回 true 表示"现在是已收藏" */
function toggleFav(game) {
    const key = favKey(game);
    if (!key) return false;
    const list = favLoad();
    const idx = list.findIndex(it => it && it.key === key);
    if (idx >= 0) {
        list.splice(idx, 1);
        favSave(list);
        return false;
    }
    // 只存渲染与介绍页需要的字段, 不把整包数据写进 localStorage
    // ★ detail_url 必须存: showDetail() 要靠它调 fetch_detail,
    //   缺了它打开收藏就会报错。
    list.unshift({
        key,
        game: {
            appid: game.appid,
            name: game.name,
            name_original: game.name_original,
            header_image: game.header_image,
            source: game.source,
            detail_url: game.detail_url,
            tags: game.tags,
            is_adult: game.is_adult,
        },
        at: Date.now(),
    });
    favSave(list);
    return true;
}

function favUpdateBadge() {
    const n = favLoad().length;
    const badge = document.getElementById('fav-nav-badge');
    if (badge) {
        badge.textContent = String(n);
        badge.hidden = n === 0;
    }
    const count = document.getElementById('fav-count');
    if (count) count.textContent = `共 ${n} 个收藏`;
}

/** 卡片右下角的透明小爱心 (已收藏时为实心粉红) */
function favHeartHtml(game) {
    const key = favKey(game);
    if (key) _favCardGames.set(key, game);
    const on = isFav(key);
    return `<button class="game-card-fav${on ? ' active' : ''}" data-fav-key="${escapeHtml(key)}"`
        + ` title="${on ? '取消收藏' : '收藏'}" aria-label="收藏"`
        + ` onclick="event.stopPropagation(); window.__favToggle(this)">`
        + `<svg viewBox="0 0 24 24"><path d="M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.6l-1-1a5.5 5.5 0 0 0-7.8 7.8l1 1L12 21.2l7.8-7.8 1-1a5.5 5.5 0 0 0 0-7.8z"/></svg>`
        + `</button>`;
}

/** 爱心点击: 就地切换状态, 不触发卡片的"打开详情" */
window.__favToggle = function (btn) {
    if (!btn) return;
    const key = btn.dataset.favKey;
    const stored = favLoad().find(it => it && it.key === key);
    const game = _favCardGames.get(key) || (stored && stored.game);
    if (!game) return;
    const nowOn = toggleFav(game);
    btn.classList.toggle('active', nowOn);
    btn.title = nowOn ? '取消收藏' : '收藏';
    // 收藏页里取消收藏 → 立刻从列表移除
    if (!nowOn && document.getElementById('page-fav') &&
        document.getElementById('page-fav').classList.contains('active')) {
        renderFavPage();
    }
};

/** 收藏页卡片 (结构与游戏页一致, 复用同样的 class) */
function favCardHtml(game, cardKey) {
    const displayName = game.name || game.name_original || '未命名';
    const tags = Array.isArray(game.tags) ? game.tags.slice(0, 4) : [];
    const tagsHtml = tags.length
        ? `<div class="game-card-tags">${tags.map(t =>
            `<span class="game-tag${t === '成人游戏' ? ' game-tag-adult' : ''}">${escapeHtml(t)}</span>`
        ).join('')}</div>`
        : '';
    return `
        <div class="game-card" data-appid="${escapeHtml(game.appid || '')}" onclick="showDetailByKey('${cardKey}')">
            <div class="game-card-image">
                ${game.header_image
                    ? `<img src="${game.header_image}" loading="lazy" onerror="this.style.display='none'">`
                    : ''}
                ${favHeartHtml(game)}
            </div>
            <div class="game-card-info">
                <div class="game-card-name">${escapeHtml(displayName)}</div>
                ${tagsHtml}
            </div>
        </div>
    `;
}

/** 从已注册的卡片游戏里按 appid / 名字找回一条完整记录 (用于补齐旧收藏缺失的字段) */
function _favFindRegistryGame(game) {
    const wantId = String(game.appid || '');
    const wantName = String(game.name || game.name_original || '').toLowerCase();
    for (const g of _gameCardRegistry.values()) {
        if (!g || !g.detail_url) continue;
        if (wantId && String(g.appid || '') === wantId) return g;
    }
    if (!wantName) return null;
    for (const g of _gameCardRegistry.values()) {
        if (!g || !g.detail_url) continue;
        const n = String(g.name || g.name_original || '').toLowerCase();
        if (n && n === wantName) return g;
    }
    return null;
}

function renderFavPage() {
    const grid = document.getElementById('fav-grid');
    const empty = document.getElementById('fav-empty');
    if (!grid) return;
    const list = favLoad();
    grid.innerHTML = '';
    if (empty) empty.hidden = list.length > 0;
    favUpdateBadge();
    let patched = 0;
    // 倒序 = 最近收藏在前
    list.forEach(item => {
        const game = item.game || {};
        // ★ 旧版本收藏没存 detail_url → 从已加载的游戏列表里补齐,
        //   这样老收藏不用重新收藏也能直接打开介绍页。
        if (!game.detail_url) {
            const found = _favFindRegistryGame(game);
            if (found) {
                game.detail_url = found.detail_url;
                if (!game.source) game.source = found.source;
                if (!game.header_image) game.header_image = found.header_image;
                item.game = game;
                patched++;
            }
        }
        _favCardGames.set(item.key, game);
        const cardKey = _registerCardGame(game);
        grid.insertAdjacentHTML('beforeend', favCardHtml(game, cardKey));
    });
    // 补齐成功就写回存储, 下次不必再找
    if (patched > 0) { try { localStorage.setItem(FAV_STORE_KEY, JSON.stringify(list)); } catch (_) {} }
}

window.showDetailByKey = function(key) {
    const g = _gameCardRegistry.get(key);
    if (!g) { showToast('资源信息已过期, 请刷新列表'); return; }
    // ★ 收藏页的条目来自 localStorage, 早期版本没存 detail_url。
    //   直接调 showDetail 会因为 detailUrl=undefined 而报错, 这里给一个明确提示。
    if (!g.detail_url) {
        showToast('这条收藏缺少详情链接, 请在游戏列表里重新收藏一次');
        return;
    }
    // ★ GX(galgamex) 的条目走它自己的详情页：资源列表和签名直链是另一套链路，
    //   不能丢给通用的 fetch_detail（那条路不认识 galgamex）。
    if (g.source === 'galgamex' && typeof window.__vxGxOpenDetail === 'function') {
        const gid = Number((g.extra || {}).gx_id) || 0;
        window.__vxGxOpenDetail(g.detail_url, gid);
        return;
    }
    showDetail(g.source, g.detail_url, g);
};

function appendGameGrid(games) {
    const list = Array.isArray(games) ? games : [];
    const grid = document.getElementById('game-grid');
    // 兜底: 清理残留的 .loading 占位符
    grid.querySelectorAll(':scope > .loading').forEach(el => el.remove());

    // 去重: 按 appid 去重(已有卡片不再追加), 避免资源多次出现
    // ★ 性能: 用持久化 Set (_gameGridSeenIds) 替代对全部卡片做全量 DOM 扫描
    const unique = [];
    for (const g of list) {
        const id = String(g.appid || g.detail_url || g.name || '');
        if (!id || _gameGridSeenIds.has(id)) continue;
        _gameGridSeenIds.add(id);
        unique.push(g);
    }
    if (unique.length === 0) return;

    // 保留后端返回顺序(后端按发布时间排列), 不按抓取时间重新排序
    const html = unique.map(game => {
        // 优先显示中文名 (name_cn), 其次是 name; 原始名显示在下方小字
        const displayName = game.name_cn || game.name;
        // ★ byrut 的 name_original 是从 URL slug 提取的英文, 信息量低且与俄文标题重复,
        //   用户要求 "by 资源在名字和分类中间有一串字这个不要" → byrut 不显示 name_original.
        //   (name_original 仍保留在数据里供英文名搜索)
        const isByrut = (game.source === 'byrut');
        const originalNameStr = (!isByrut && game.name_original && game.name_original !== displayName)
            ? game.name_original
            : (!isByrut && game.name && game.name !== displayName ? game.name : '');
        const originalHtml = originalNameStr
            ? `<div class="game-card-name-original">${escapeHtml(originalNameStr)}</div>`
            : '';
        // 标签: 后端合并去重后的分类/成人标签, 卡片最多显示 4 个 (成人游戏排最前)
        const tags = Array.isArray(game.tags) ? game.tags.slice() : [];
        tags.sort((a, b) => (a === '成人游戏' ? -1 : 0) - (b === '成人游戏' ? -1 : 0));
        const tagsHtml = tags.length > 0
            ? `<div class="game-card-tags">${tags.slice(0, 4).map(t =>
                `<span class="game-tag${t === '成人游戏' ? ' game-tag-adult' : ''}">${escapeHtml(t)}</span>`
            ).join('')}</div>`
            : '';
        // data-original-name: 保存原始名字(优先 name_original), 用于"翻译"按钮重新翻译
        const originalName = (game.name_original || game.name || '').replace(/"/g, '&quot;');
        const cardKey = _registerCardGame(game);
        return `
            <div class="game-card" data-appid="${escapeHtml(game.appid)}" data-original-name="${originalName}" onclick="showDetailByKey('${cardKey}')">
                <div class="game-card-image">
                    ${game.header_image
                        ? `<img src="${game.header_image}" loading="lazy" onerror="this.style.display='none'">`
                        : ''}
                    ${favHeartHtml(game)}
                </div>
                <div class="game-card-info">
                    <div class="game-card-name">${escapeHtml(displayName)}</div>
                    ${originalHtml}
                    ${tagsHtml}
                </div>
            </div>
        `;
    }).join('');
    grid.insertAdjacentHTML('beforeend', html);
    // ★ 性能: 从 grid 末尾直接取本次新增卡片节点 (O(新增数)), 供翻译时 O(1) 定位名称元素,
    //   替代 autoTranslateCards 内每张卡片一次 querySelector (O(批量 × 总数))
    const nameEls = _collectNewCardNameEls(grid, unique.length);
    // ★ 入场动效 (2026-10-03): 只给**本次新追加**的卡片打标记。
    //   用 O(新增数) 的尾部取节点, 不做全量 querySelectorAll —— 否则每次追加
    //   都要扫全网格 (上千张卡片), 反而拖慢滚动加载。动画结束后立刻摘掉 class,
    //   避免长期持有动画状态影响后续合成层。
    try {
        const all = grid.children;
        const startIdx = all.length - unique.length;
        for (let i = Math.max(0, startIdx); i < all.length; i++) {
            const el = all[i];
            if (!el || el.nodeType !== 1) continue;
            el.classList.add('vx-enter');
            el.addEventListener('animationend', function once() {
                el.classList.remove('vx-enter');
                el.removeEventListener('animationend', once);
            });
        }
    } catch (_) {}
    // 异步自动翻译: 英文/俄文游戏名 → 中文 (用 T/L 引擎, 不阻塞加载)
    autoTranslateCards(games, 'game-grid', nameEls);
}

/// 异步批量翻译卡片中的游戏名 (英文/俄文 → 中文), 翻译完更新 DOM
/// 不阻塞页面加载, 翻译失败保持原文
/// T 模式(本地词典): 毫秒级, 极快
/// L 模式(在线API): 8路并发, 较快
// ★ 从 grid 末尾向前收集本次新增卡片的名称元素, 返回 appid -> 名称元素 的 Map
//   (只遍历新增节点, 复杂度 O(新增数), 避免对全量卡片重复查询)
function _collectNewCardNameEls(grid, count) {
    const map = new Map();
    if (!grid || count <= 0) return map;
    let node = grid.lastElementChild;
    for (let i = 0; i < count && node; i++) {
        if (!node.classList || !node.classList.contains('game-card')) break;
        const id = node.getAttribute('data-appid');
        const nameEl = node.querySelector('.game-card-name');
        if (id && nameEl) map.set(String(id), nameEl);
        node = node.previousElementSibling;
    }
    return map;
}

async function autoTranslateCards(games, gridId, nameEls) {
    // 收集需要翻译的名字 (非中文, 且没有已保存的中文名)
    const toTranslate = [];
    const appids = [];
    for (const g of games) {
        const name = g.name || '';
        // 已有中文名则跳过翻译 (直接显示 name_cn)
        if (g.name_cn) continue;
        if (name && needsTranslation(name)) {
            toTranslate.push(name);
            appids.push(g.appid);
        }
    }
    if (toTranslate.length === 0) return;
    try {
        const translated = await gameTrTranslateBatch(toTranslate, 'auto');
        // 优先使用渲染时缓存的名称元素 (O(1) 查找); 未提供时回退到 DOM 查询
        const grid = nameEls ? null : document.getElementById(gridId);
        if (!nameEls && !grid) return;
        const pairs = [];
        for (let i = 0; i < appids.length && i < translated.length; i++) {
            const card = nameEls
                ? nameEls.get(String(appids[i]))
                : grid.querySelector(`.game-card[data-appid="${cssEscape(appids[i])}"] .game-card-name`);
            let result = translated[i];
            // 翻译失败 (空或与原文相同): 单独重试一次
            if (!result || result === toTranslate[i]) {
                try {
                    const retry = await gameTrTranslate(toTranslate[i], 'auto');
                    if (retry && retry !== toTranslate[i]) result = retry;
                } catch (_) {}
            }
            if (card && result && result !== toTranslate[i]) {
                card.textContent = result;
                // 收集翻译名, 循环结束后一次性批量回写后端索引 (支持中文名搜索)
                pairs.push([appids[i], result]);
            }
        }
        // ★ 性能修复 (2026-09-30): 由「每张卡片一次 IPC」改为「整批一次 IPC」,
        //   避免每张卡片都在 UI 线程触发一次 O(n) 全量卡片扫描导致点击卡顿
        if (pairs.length > 0 && typeof invoke === 'function') {
            invoke('save_translated_names', { pairs }).catch(() => {});
        }
    } catch (e) {
        console.warn('自动翻译卡片名失败:', e);
    }
}

const _HTML_ESCAPE_MAP = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
const _HTML_ESCAPE_RE = /[&<>"']/g;
function escapeHtml(text) {
    if (text === null || text === undefined) return '';
    return String(text).replace(_HTML_ESCAPE_RE, c => _HTML_ESCAPE_MAP[c]);
}

// 搜索 (实时搜索: 输入时防抖触发, 300ms 无新输入后搜索)
let searchDebounceTimer = null;
function debounceSearch() {
    if (searchDebounceTimer) clearTimeout(searchDebounceTimer);
    searchDebounceTimer = setTimeout(() => {
        state.currentKeyword = document.getElementById('search-input').value.trim();
        loadGamesReset();
    }, 300);
}

document.getElementById('search-btn')?.addEventListener('click', () => {
    state.currentKeyword = document.getElementById('search-input').value.trim();
    loadGamesReset();
});

document.getElementById('search-input')?.addEventListener('input', debounceSearch);
document.getElementById('search-input')?.addEventListener('keypress', (e) => {
    if (e.key === 'Enter') {
        if (searchDebounceTimer) { clearTimeout(searchDebounceTimer); searchDebounceTimer = null; }
        state.currentKeyword = e.target.value.trim();
        loadGamesReset();
    }
});

// 来源过滤
document.getElementById('source-filter')?.addEventListener('change', (e) => {
    state.currentSource = e.target.value;
    loadGamesReset();
});

// 分类过滤
document.getElementById('category-filter')?.addEventListener('change', (e) => {
    state.currentCategory = e.target.value;
    state.categoryRetryCount = 0; // 切换分类时重置重试计数
    // 选了具体分类: 清空关键词, 走 search 路径 (后端在空关键词+有分类时
    // 从 BY/KO 内存缓存按分类筛选, KO 优先排序)
    if (state.currentCategory !== '全部类型') {
        state.currentKeyword = '';
    }
    loadGamesReset();
});

// 无限滚动
function setupInfiniteScroll() {
    const loader = document.getElementById('scroll-loader');
    if (!loader) return;
    const observer = new IntersectionObserver((entries) => {
        if (entries[0].isIntersecting && !state.infiniteScrollLoading && !state.infiniteScrollEnd) {
            // 搜索/分类结果分批渲染: buffer 还有剩余时直接渲染下一批 (不重新请求)
            if (Array.isArray(state.searchBuffer) && state.searchBufferOffset < state.searchBuffer.length) {
                renderNextSearchBatch();
                return;
            }
            loadGamesAppend();
        }
    }, { threshold: 0.1, rootMargin: '200px' });
    observer.observe(loader);
}

// 渲染搜索/分类结果的下一批 (60 个/批)
function renderNextSearchBatch() {
    if (state.infiniteScrollLoading || state.infiniteScrollEnd) return;
    state.infiniteScrollLoading = true;
    try {
        const loader = document.getElementById('scroll-loader');
        const batch = state.searchBuffer.slice(state.searchBufferOffset, state.searchBufferOffset + 60);
        state.searchBufferOffset += batch.length;
        state.infiniteScrollEnd = state.searchBufferOffset >= state.searchBuffer.length;
        appendGameGrid(batch);
        if (loader) {
            loader.innerHTML = state.infiniteScrollEnd
                ? '<div class="loading">已加载全部</div>'
                : '<div class="loading">向下滚动加载更多...</div>';
        }
    } finally {
        state.infiniteScrollLoading = false;
    }
}

// ============================================================
// 成人游戏独立页 (本地缓存筛选, 毫秒级响应)
// ============================================================

// 成人页卡片渲染 (与 appendGameGrid 相同结构, 目标 grid 不同)
function appendAdultGrid(games) {
    const list = Array.isArray(games) ? games : [];
    const grid = document.getElementById('adult-game-grid');
    if (!grid) return;
    grid.querySelectorAll(':scope > .loading').forEach(el => el.remove());

    // 去重
    // ★ 性能: 用持久化 Set (_adultGridSeenIds) 替代对全部卡片做全量 DOM 扫描
    const unique = [];
    for (const g of list) {
        const id = String(g.appid || g.detail_url || g.name || '');
        if (!id || _adultGridSeenIds.has(id)) continue;
        _adultGridSeenIds.add(id);
        unique.push(g);
    }
    if (unique.length === 0) return;

    const html = unique.map(game => {
        const displayName = game.name_cn || game.name;
        // ★ byrut 不显示 name_original (与主列表一致, 用户反馈 "by 资源名字和分类中间有一串字")
        const isByrut = (game.source === 'byrut');
        const originalNameStr = (!isByrut && game.name_original && game.name_original !== displayName)
            ? game.name_original
            : (!isByrut && game.name && game.name !== displayName ? game.name : '');
        const originalHtml = originalNameStr
            ? `<div class="game-card-name-original">${escapeHtml(originalNameStr)}</div>`
            : '';
        // 成人页标签: 不显示 "成人游戏" 本身 (整页都是), 显示其余分类标签
        const tags = (Array.isArray(game.tags) ? game.tags : []).filter(t => t !== '成人游戏');
        const tagsHtml = tags.length > 0
            ? `<div class="game-card-tags">${tags.slice(0, 4).map(t =>
                `<span class="game-tag">${escapeHtml(t)}</span>`
            ).join('')}</div>`
            : '';
        const originalName = (game.name_original || game.name || '').replace(/"/g, '&quot;');
        const cardKey = _registerCardGame(game);
        return `
            <div class="game-card" data-appid="${escapeHtml(game.appid)}" data-original-name="${originalName}" onclick="showDetailByKey('${cardKey}')">
                <div class="game-card-image">
                    ${game.header_image
                        ? `<img src="${game.header_image}" loading="lazy" onerror="this.style.display='none'">`
                        : ''}
                    ${favHeartHtml(game)}
                </div>
                <div class="game-card-info">
                    <div class="game-card-name">${escapeHtml(displayName)}</div>
                    ${originalHtml}
                    ${tagsHtml}
                </div>
            </div>
        `;
    }).join('');
    grid.insertAdjacentHTML('beforeend', html);
    // ★ 性能: 缓存本次新增卡片名称元素, 翻译时 O(1) 定位 (替代每卡一次全量查询)
    const nameEls = _collectNewCardNameEls(grid, unique.length);
    autoTranslateCards(games, 'adult-game-grid', nameEls);
}

// 加载成人页分类标签 (从后端动态统计成人游戏 tags)
async function loadAdultCategories() {
    if (state.adultCategoriesLoaded) return;
    try {
        const cats = await invoke('adult_categories');
        const sel = document.getElementById('adult-category-filter');
        if (!sel) return;
        // ★ 缓存未就绪时返回空列表: 不置 loaded 标记, 下次进入重试
        //   (否则首次进入时缓存没加载完, 分类下拉会永远只有 "全部类型")
        if (!Array.isArray(cats) || cats.length === 0) return;
        const cur = sel.value || '全部类型';
        sel.innerHTML = '<option value="全部类型">全部类型</option>' +
            cats.map(c => `<option value="${escapeHtml(c)}">${escapeHtml(c)}</option>`).join('');
        // 恢复之前的选择 (若仍存在)
        sel.value = [...sel.options].some(o => o.value === cur) ? cur : '全部类型';
        state.adultCategory = sel.value;
        state.adultCategoriesLoaded = true;
    } catch (e) {
        console.warn('[adult] 分类标签加载失败:', e);
    }
}

// 成人页首次进入: 加载分类 + 重置列表
async function loadAdultGames() {
    frontLog('ADULT', `loadAdultGames 开始 adultInitialized=${state.adultInitialized}`);
    await loadAdultCategories();
    frontLog('ADULT', 'loadAdultCategories 完成, 进入 loadAdultReset');
    await loadAdultReset();
}

// ============================================================
// GX 预签名链接过期检测 / 自动换新
// ------------------------------------------------------------
// ★ 用户报「GX 资源提示一直连接中」的真因之一：galgamex 给的是
//   AWS 预签名 URL（`X-Amz-Date=20261008T143123Z` + `X-Amz-Expires=3600`），
//   **1 小时后失效**。任务建好放一会儿再下、或者暂停很久再继续，
//   链接已经过期 → CDN 对所有分块返回 403 → 引擎一直重试，
//   前端一直显示"连接中..."（因为一个字节都没收到）。
//   所以：开下之前先看链接还剩多久，快过期/已过期就重新要一条。
// ============================================================

/// 解析预签名 URL 的失效时间（返回毫秒时间戳；不是预签名 URL 或解析失败返回 0）
function signedUrlExpiry(url) {
    try {
        const u = new URL(String(url));
        const d = u.searchParams.get('X-Amz-Date');
        const exp = u.searchParams.get('X-Amz-Expires');
        if (!d || !exp) return 0;
        // 形如 20261008T143123Z
        const m = /^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/.exec(d);
        if (!m) return 0;
        const t = Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]);
        return t + (Number(exp) || 0) * 1000;
    } catch (e) { return 0; }
}

/// 快过期（<60s）或已过期就重新取一条；返回可用的 url
async function refreshGxUrlIfExpired(dl) {
    const url = String((dl && dl.url) || '');
    const rid = dl && (dl.gxResourceId || dl.gx_resource_id);
    if (!rid) return url;
    const exp = signedUrlExpiry(url);
    if (!exp) return url;                       // 不是预签名链接，不动
    const left = exp - Date.now();
    if (left > 60000) return url;               // 还很新，直接用
    try {
        const p = await invoke('gx_pick_download', {
            resourceId: rid,
            index: (dl.gxIndex == null ? null : dl.gxIndex),
        });
        if (p && p.url) {
            frontLog('GX_REFRESH', '链接' + (left <= 0 ? '已过期' : '即将过期')
                + '（剩 ' + Math.round(left / 1000) + 's）→ 已重新取一条');
            showToast('下载链接已过期，已自动换一条新的', 'success', 3000);
            return p.url;
        }
    } catch (e) {
        frontLog('GX_REFRESH_FAIL', String(e && e.message ? e.message : e).slice(0, 200));
    }
    return url;
}

// 成人库总数（显示在成人页工具栏上）
async function loadAdultCount() {
    const el = document.getElementById('adult-count');
    if (!el) return;
    try {
        const n = await invoke('adult_total', {
            category: state.adultCategory || '全部类型',
            keyword: state.adultKeyword || '',
        });
        el.textContent = '共 ' + Number(n || 0).toLocaleString() + ' 款';
    } catch (e) {
        el.textContent = '';
    }
}

// 重置成人列表 (切换分类/搜索时)
async function loadAdultReset() {
    state.adultPage = 1;
    state.adultEnd = false;
    state.adultRetryCount = 0; // 切换分类/搜索时重置重试计数
    const grid = document.getElementById('adult-game-grid');
    frontLog('ADULT', `loadAdultReset: grid=${grid ? 'found' : 'MISSING'}`);
    if (grid) grid.innerHTML = '<div class="loading">加载中...</div>';
    _adultGridSeenIds.clear();
    await loadAdultAppend();
    state.adultInitialized = true;
    frontLog('ADULT', 'loadAdultReset 完成, adultInitialized=true');
}

// 追加加载成人页下一页
async function loadAdultAppend() {
    if (state.adultLoading || state.adultEnd) {
        frontLog('ADULT', `loadAdultAppend 跳过 loading=${state.adultLoading} end=${state.adultEnd}`);
        return;
    }
    state.adultLoading = true;
    const grid = document.getElementById('adult-game-grid');
    const loader = document.getElementById('adult-scroll-loader');
    try {
        frontLog('ADULT', `调用 adult_browse_local cat=${state.adultCategory} kw=${state.adultKeyword} page=${state.adultPage}`);
        const games = await invoke('adult_browse_local', {
            category: state.adultCategory,
            keyword: state.adultKeyword,
            page: state.adultPage,
        });
        frontLog('ADULT', `adult_browse_local 返回 ${games ? games.length : 'null'} 条`);
        if (!games || games.length === 0) {
            if (state.adultPage === 1) {
                // 有搜索词: 结果为空直接提示, 不重试 (与主列表行为一致)
                if (state.adultKeyword && state.adultKeyword.trim()) {
                    if (grid) grid.innerHTML = '<div class="loading">未找到相关游戏</div>';
                    if (loader) loader.innerHTML = '';
                    state.adultLoading = false;
                    return;
                }
                // 无搜索词: 缓存可能未就绪, 有限次延迟重试 (最多 15 次, 共 ~15 秒)
                state.adultRetryCount = (state.adultRetryCount || 0) + 1;
                if (state.adultRetryCount <= 15) {
                    if (grid) grid.innerHTML = '<div class="loading">正在加载游戏，请稍候...</div>';
                    if (loader) loader.innerHTML = '';
                    state.adultLoading = false;
                    setTimeout(() => {
                        // 重试游戏列表的同时重试分类标签 (缓存就绪后分类才可用)
                        loadAdultCategories().finally(() => loadAdultAppend());
                    }, 1000);
                    return;
                }
                // 超过重试上限: 缓存就绪但该分类/列表无资源
                if (grid) grid.innerHTML = '<div class="loading">该分类暂无资源</div>';
                if (loader) loader.innerHTML = '';
                state.adultLoading = false;
                return;
            }
            state.adultEnd = true;
            if (loader) loader.innerHTML = '<div class="loading">已加载全部</div>';
            state.adultLoading = false;
            return;
        }
        state.adultRetryCount = 0; // 成功页: 重置重试计数
        appendAdultGrid(games);
        // ★ 总数只在第一页算一次（后台遍历全部卡片，别每页都算）
        if (state.adultPage === 1) loadAdultCount();
        state.adultPage++;
        if (loader) loader.innerHTML = '<div class="loading">向下滚动加载更多...</div>';
    } catch (e) {
        console.error('[adult] 加载失败:', e);
        if (grid) grid.innerHTML = `<div class="loading">加载失败: ${e}</div>`;
        if (loader) {
            loader.innerHTML = '<div class="loading">加载失败, 点击重试</div>';
            loader.onclick = () => { loader.onclick = null; loadAdultAppend(); };
        }
    } finally {
        state.adultLoading = false;
    }
}

// 成人页无限滚动
function setupAdultInfiniteScroll() {
    const loader = document.getElementById('adult-scroll-loader');
    if (!loader) return;
    const observer = new IntersectionObserver((entries) => {
        if (entries[0].isIntersecting && !state.adultLoading && !state.adultEnd) {
            loadAdultAppend();
        }
    }, { threshold: 0.1, rootMargin: '200px' });
    observer.observe(loader);
}

// ============================================================
// 游戏详情
// ============================================================
window.showDetail = async function(source, detailUrl, gameJson) {
    const game = typeof gameJson === 'string' ? JSON.parse(gameJson.replace(/&quot;/g, '"')) : gameJson;
    state.currentDetailGame = game;

    // 竞态守卫: 每次进入详情页递增 token, 异步回调时检查是否仍是当前请求
    const reqToken = ++state._detailReqToken;

    // 使用保存的视图状态来判断来源页面
    // currentActiveView 由 switchPageView 函数维护
    state.detailFromPage = currentActiveView || 'resource';

    // ★ 记录列表滚动位置, 返回列表时恢复 (修复点返回直接回顶部)
    saveViewScroll();
    state._curViewKey = 'detail:page';
    const detailScroller = contentScroller();
    if (detailScroller) detailScroller.scrollTop = 0;

    // 切换到详情页
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
    document.getElementById('page-detail').classList.add('active');

    const content = document.getElementById('detail-content');
    content.innerHTML = '<div class="loading">加载中...</div>';

    try {
        // 策略优化 (修复 "一直加载中"):
        // 1) 只等 fetch_detail (基础信息), 拿到后立即渲染页面 — 下载链接异步补
        // 2) fetch_downloads 与 fetch_detail 并行发起; 详情渲染后把结果合并进下载区
        //    (下载链接冷启动慢/源站抖动时, 用户先看到介绍/截图, 不再卡 "加载中")
        const req_downloads_p = invoke('fetch_downloads', {
            source: source,
            detailUrl: detailUrl,
        }).catch(err => {
            console.warn('[loadDetail] fetch_downloads 失败:', err);
            return null;
        });

        let detail = await invoke('fetch_detail', {
            source: source,
            detailUrl: detailUrl,
        });
        if (reqToken !== state._detailReqToken) return;

        // ★ 立即渲染详情 (下载区先显示 "获取下载链接中...")
        // 深拷贝避免异步合并覆盖正在展示的数据
        const initialDetail = Object.assign({}, detail || {}, { downloads: [] });
        state.currentDetailDownloads = [];
        renderDetail(game, initialDetail);

        // 异步补下载链接 (不阻塞页面)
        (async () => {
            try {
                let dlsResult = await req_downloads_p;
                if (reqToken !== state._detailReqToken) return;
                let merged = [];
                if (Array.isArray(dlsResult) && dlsResult.length > 0) {
                    // 合并: 优先 fetch_downloads (更精确), 再合并 fetch_detail 的结果
                    // 去重规则: 先按 url 去重, 再按 label 去重 (同版本多格式合并为一个)
                    const seenUrl = new Set();
                    const seenLabel = new Set();
                    const pushDedup = (d) => {
                        if (!d) return;
                        const url = String(d.url || '').trim();
                        const label = String(d.label || '').trim();
                        if (!url) return;
                        if (seenUrl.has(url)) return;
                        seenUrl.add(url);
                        if (label && seenLabel.has(label)) return;
                        if (label) seenLabel.add(label);
                        merged.push(d);
                    };
                    for (const d of dlsResult) pushDedup(d);
                    const exist = (detail && Array.isArray(detail.downloads)) ? detail.downloads : [];
                    for (const d of exist) pushDedup(d);
                } else {
                    // fetch_downloads 为空 → 用 fetch_detail 自带的 + 多次递增延迟重试
                    merged = (detail && Array.isArray(detail.downloads)) ? detail.downloads.slice() : [];
                    if (merged.length === 0) {
                        for (let attempt = 1; attempt <= 3; attempt++) {
                            if (reqToken !== state._detailReqToken) return;
                            await new Promise(r => setTimeout(r, attempt * 500)); // 500ms → 1000ms → 1500ms
                            if (reqToken !== state._detailReqToken) return;
                            try {
                                const retryN = await invoke('fetch_downloads', { source: source, detailUrl: detailUrl });
                                if (reqToken === state._detailReqToken && Array.isArray(retryN) && retryN.length > 0) {
                                    merged = retryN;
                                    frontLog('DETAIL_RETRY_OK', `第${attempt}次重试成功, 获取${retryN.length}个链接`);
                                    break;
                                }
                            } catch (_e2) { /* 忽略, 继续下一次重试 */ }
                        }
                    }
                }
                if (reqToken !== state._detailReqToken) return;
                // 更新下载区 DOM (详情页已渲染, 只替换下载选项部分)
                state.currentDetailDownloads = merged;
                const wrap = document.getElementById('detail-downloads-wrap');
                if (wrap) {
                    wrap.innerHTML = renderDownloads(game, merged);
                }
            } catch (e) {
                console.warn('[loadDetail] 异步补下载链接失败:', e);
            }
        })();

    } catch (e) {
        if (reqToken !== state._detailReqToken) return;
        content.innerHTML = `<div class="loading">加载失败: ${e}</div>`;
        console.error('详情加载失败:', e);
    }
};

function renderDetail(game, detail) {
    const sourceLabel = {
        'byrut': 'BY (Byrut)',
        'koyso': 'KO (Koyso)',
        'galgamex': 'GX (Galgamex)',
    }[game.source] || game.source;

    const content = document.getElementById('detail-content');
    // 描述支持 HTML 渲染(Galgamex 返回 HTML), 纯文本则保留换行
    const descHtml = detail.description
        ? (/<[a-z][\s\S]*>/i.test(detail.description)
            ? detail.description
            : escapeHtml(detail.description).replace(/\n/g, '<br>'))
        : '暂无介绍';

    // 标签行: 后端合并去重后的标签 (成人游戏高亮排最前)
    const tags = Array.isArray(game.tags) ? game.tags.slice() : [];
    tags.sort((a, b) => (a === '成人游戏' ? -1 : 0) - (b === '成人游戏' ? -1 : 0));
    const tagsHtml = tags.length > 0
        ? `<div class="detail-tags">${tags.map(t =>
            `<span class="game-tag${t === '成人游戏' ? ' game-tag-adult' : ''}">${escapeHtml(t)}</span>`
        ).join('')}</div>`
        : '';

    // 截图画廊: 仅 byrut 来源显示 (koyso 的图片/视频已在简介 HTML 中直接渲染, 无需重复画廊)
    const isByrut = game.source === 'byrut';
    const screenshots = (isByrut && detail && detail.extra && detail.extra.images && Array.isArray(detail.extra.images))
        ? detail.extra.images.filter(u => typeof u === 'string' && u.startsWith('http'))
        : [];
    const galleryHtml = screenshots.length > 0
        ? `<div class="detail-screenshots"><h3>游戏截图</h3><div class="screenshot-grid">${
            screenshots.map((url, idx) =>
                `<img class="screenshot-thumb" src="${escapeHtml(url)}" data-index="${idx}" loading="lazy" onerror="this.style.display='none'">`
            ).join('')
        }</div></div>`
        : '';

    content.innerHTML = `
        <div class="detail-left">
            <h1 class="detail-title">${escapeHtml(game.name)}</h1>
            <div class="detail-meta">
                <span>来源: ${sourceLabel}</span>
                <span>·</span>
                <span>ID: ${game.appid}</span>
            </div>
            ${tagsHtml}
            <div class="detail-image">
                ${detail.header_image_large
                    ? `<img src="${detail.header_image_large}" onerror="this.style.display='none'">`
                    : game.header_image
                        ? `<img src="${game.header_image}" onerror="this.style.display='none'">`
                        : ''}
            </div>
            <div class="detail-description">${descHtml}</div>
            ${galleryHtml}
        </div>
        <div class="detail-right">
            <h3>下载选项</h3>
            <div id="detail-downloads-wrap">
                ${renderDownloads(game, detail.downloads, true)}
            </div>
            <div id="recommend-section" class="recommend-section">
                <div class="loading">加载推荐中...</div>
            </div>
        </div>
    `;

    // 截图点击放大查看
    if (screenshots.length > 0) {
        const grid = content.querySelector('.screenshot-grid');
        if (grid) {
            grid.addEventListener('click', (e) => {
                const img = e.target.closest('.screenshot-thumb');
                if (!img) return;
                const viewer = document.createElement('div');
                viewer.className = 'screenshot-viewer';
                viewer.innerHTML = `<img src="${img.src}"><span class="viewer-close">✕</span>`;
                viewer.addEventListener('click', () => viewer.remove());
                document.body.appendChild(viewer);
            });
        }
    }

    // 异步加载相关推荐
    loadRecommendations(game);

    // 异步自动翻译: 标题 + 简介 (英文/俄文 → 中文)
    // 后端持久缓存: 首次打开联网翻译并落盘, 后续打开缓存命中秒回, 仅新游戏联网翻译
    autoTranslateDetail(game, detail, content);
}

// 详情页翻译: 标题与简介非中文时自动翻译, 翻译完原地更新 DOM (不阻塞渲染)
async function autoTranslateDetail(game, detail, contentEl) {
    try {
        const texts = [];
        const titleNeed = game.name && needsTranslation(game.name);
        // 简介去 HTML 标签后判断是否需要翻译
        const descRaw = detail && detail.description ? String(detail.description) : '';
        const descText = descRaw.replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim();
        const descNeed = descText && needsTranslation(descText) && descText.length <= 8000;
        if (titleNeed) texts.push(game.name);
        if (descNeed) texts.push(descText);
        if (texts.length === 0) return;

        const translated = await gameTrTranslateBatch(texts, 'auto');
        if (!translated || translated.length === 0) return;

        // 竞态守卫: 用户已切到其他游戏详情则丢弃本次结果
        const cur = state.currentDetailGame;
        if (!cur || String(cur.appid) !== String(game.appid)) return;

        let idx = 0;
        if (titleNeed && translated[idx] && translated[idx] !== game.name) {
            const t = translated[idx];
            const el = contentEl.querySelector('.detail-title');
            if (el && t) el.textContent = t;
            idx++;
        } else if (titleNeed) {
            idx++;
        }
        if (descNeed && translated[idx] && translated[idx] !== descText) {
            const el = contentEl.querySelector('.detail-description');
            if (el) {
                el.textContent = translated[idx];
                el.style.whiteSpace = 'pre-wrap';
            }
        }
    } catch (e) {
        console.warn('详情自动翻译失败:', e);
    }
}

// 加载相关推荐(基于快照同分类)
async function loadRecommendations(game) {
    const section = document.getElementById('recommend-section');
    if (!section) return;
    try {
        const recs = await invoke('recommend', {
            appid: game.appid,
            category: game.category || '',
            limit: 6,
        });
        if (!recs || recs.length === 0) {
            section.innerHTML = '';
            return;
        }
        section.innerHTML = `
            <h3 class="recommend-title">相关推荐</h3>
            <div class="recommend-grid">
                ${recs.map(g => {
                    const sourceClass = `source-${g.source}`;
                    const sourceLabel = {
                        'byrut': 'BY', 'koyso': 'KO', 'galgamex': 'GX',
                    }[g.source] || g.source;
                    const cardKey = _registerCardGame(g);
                    return `
                        <div class="recommend-card" onclick="showDetailByKey('${cardKey}')">
                            <div class="recommend-card-img">
                                ${g.header_image
                                    ? `<img src="${g.header_image}" loading="lazy" onerror="this.style.display='none'">`
                                    : ''}
                            </div>
                            <div class="recommend-card-name">${escapeHtml(g.name)}</div>
                            <span class="recommend-card-source ${sourceClass}">${sourceLabel}</span>
                        </div>
                    `;
                }).join('')}
            </div>
        `;
    } catch (e) {
        section.innerHTML = '';
        console.error('推荐加载失败:', e);
    }
}

function renderDownloads(game, downloads, loading = false) {
    if (!downloads || downloads.length === 0) {
        // 异步获取下载链接中: 显示加载指示 (不显示重试按钮, 避免误点打断)
        if (loading) {
            return '<div class="loading">正在获取下载链接...</div>';
        }
        // 冷启动源站索引未就绪时 downloads 可能为空, 提供"重新获取"按钮让用户手动触发重试
        const g = game || (state.currentDetailGame) || {};
        if (g.source && g.detail_url) {
            return `<button class="download-main-btn retry-fetch-btn" onclick="retryFetchDownloads()">重新获取下载链接</button>`;
        }
        return '<div class="empty-state">暂无下载链接</div>';
    }
    // 合并为一个下载按钮: 多链接点击后弹出选择, 单链接直接打开下载设置
    const count = downloads.length;
    const btnText = count > 1 ? `下载 (共 ${count} 个链接)` : '下载';
    // 汇总提取码/解压码 (所有链接共用时显示)
    const firstWithCode = downloads.find(d => d.code || d.unzip_code);
    const codesHtml = firstWithCode ? `
        <div class="download-codes">
            ${firstWithCode.code ? `<span class="code-badge extract">提取码: ${escapeHtml(firstWithCode.code)}</span>` : ''}
            ${firstWithCode.unzip_code ? `<span class="code-badge decompress">解压码: ${escapeHtml(firstWithCode.unzip_code)}</span>` : ''}
        </div>
    ` : '';
    return `
        <button class="download-main-btn" onclick="onDownloadClick(0)">${btnText}</button>
        ${codesHtml}
    `;
}

// ============================================================
// 下载流程: 链接选择 -> 设置弹窗 -> 启动下载
// ============================================================
// 点击下载按钮: 网盘链接直接打开浏览器, 其他走链接选择 -> 设置弹窗 -> 启动下载
// 重新获取下载链接: 冷启动源站索引未就绪时, 用户手动触发重新加载详情页
window.retryFetchDownloads = function() {
    const g = state.currentDetailGame;
    if (!g || !g.source || !g.detail_url) {
        showToast('无法重新获取: 缺少详情页信息, 请返回列表重试');
        return;
    }
    // 复用 showDetail 重新加载 (会递增 reqToken, 自动取消旧的异步请求)
    showDetail(g.source, g.detail_url, g);
};
window.onDownloadClick = function(downloadIdx) {
    try {
        const downloads = state.currentDetailDownloads;
        // ★ 不再静默 return, 给用户明确提示 (否则竞态覆盖为[]时点按钮"没反应"=弹窗没弹出)
        if (!downloads || downloads.length === 0) {
            showToast('暂无可下载的链接，请稍等几秒或刷新详情页重试');
            return;
        }

        const dl = downloads[downloadIdx] || downloads[0];
        if (!dl || !dl.url) {
            showToast('下载链接无效，请返回上一页重试');
            return;
        }
    // 网盘链接: 不打开浏览器, 改为复制到剪贴板 (需登录/验证码的网盘爬虫抓不到)
            // 网盘链接: 统一走内置浏览器打开 (不依靠外部系统浏览器)
    if (dl && dl.type === 'netdisk') {
        openBrowserWindow(dl.url);
        return;
    }

    // 如果该游戏有多个下载链接, 先让用户选择
    if (downloads.length > 1) {
        showDownloadLinks(downloads);
    } else {
        showDownloadModal(downloads[0]);
    }
    } catch (e) {
        console.error('onDownloadClick 异常:', e);
        showToast('弹出下载设置失败: ' + (e && e.message ? e.message : String(e)));
    }
};

// 显示下载链接选择弹窗(多链接时)
window.showDownloadLinks = function(downloads) {
    if (!downloads || downloads.length === 0) return;
    const body = document.getElementById('link-select-body');
    body.innerHTML = downloads.map((dl, idx) => {
        // ★ 判空 (2026-10-03): 原写法 `dl.filename.split('.')` 在后端未返回
        //   filename 时会抛 TypeError。本文件其它处都做了防护
        //   (3629 行 `dl.filename || '未知文件名'`、3677 行 `String(dl.filename || '')`),
        //   说明 filename 确实可能缺失。而这个异常会让整个链接选择弹窗弹不出来
        //   (调用方只显示一句笼统的"弹出下载设置失败")。
        const safeFn = String(dl.filename || '');
        const ext = (safeFn.split('.').pop() || '').toLowerCase();
        const metaParts = [];
        if (dl.size) metaParts.push(`大小: ${escapeHtml(dl.size)}`);
        if (dl.version) metaParts.push(`版本: ${escapeHtml(dl.version)}`);
        const meta = metaParts.join(' · ') || escapeHtml(dl.label);
        // 显示提取码/解压码
        const codesHtml = (dl.code || dl.unzip_code) ? `
            <div class="link-option-codes">
                ${dl.code ? `<span class="code-badge extract">提取码: ${escapeHtml(dl.code)}</span>` : ''}
                ${dl.unzip_code ? `<span class="code-badge decompress">解压码: ${escapeHtml(dl.unzip_code)}</span>` : ''}
            </div>
        ` : '';
        return `
            <div class="link-option" onclick="selectDownloadLink(${idx})">
                <div class="link-option-icon ${ext}">${ext.toUpperCase()}</div>
                <div class="link-option-info">
                    <div class="link-option-label">${escapeHtml(dl.label)}</div>
                    <div class="link-option-meta">${meta}</div>
                    ${codesHtml}
                </div>
                <div class="link-option-arrow">→</div>
            </div>
        `;
    }).join('');
    document.getElementById('link-select-modal').style.display = 'flex';
};

/// 关闭「选择下载线路」弹窗。
///
/// ★★ 这个函数以前**根本没定义** —— 而 HTML 的遮罩/✕ 按钮 onclick 直接调它，
///   `selectDownloadLink` 也先调它再开下载弹窗。于是：
///     点遮罩/✕ → 抛 "closeLinkSelect is not defined"，弹窗关不掉；
///     点某条线路 → 同样抛错，**下载弹窗压根不会开**（多线路的游戏完全下不了）。
///   用户报的「好多按钮都是无效的」就包括这一类"调用了不存在的函数"。
window.closeLinkSelect = function() {
    const m = document.getElementById('link-select-modal');
    if (m) m.style.display = 'none';
};

window.selectDownloadLink = function(idx) {
    const dl = state.currentDetailDownloads[idx];
    if (!dl) return;
    closeLinkSelect();
    showDownloadModal(dl);
};



// ============================================================
// GX —— galgamex 游戏库（2026-10-07）
//
// 后端 src-tauri/src/gx.rs 把站点全量索引（7000+ 条）拉到本地：
//   分类 = 同人游戏(doujin) / Galgame(galgame)
//   年龄 = 每个游戏一个 isNsfw 布尔（站点右上角齿轮的「全年龄/全部」）
//   标签 = 站点 120 个标签，点一下筛选
//   下载 = 只要 zip/rar（站点图标 1=zip 2=rar 3=百度网盘 4=apk，用户要求只要 1、2）
//
// 和「游戏」「成人游戏」两页同构：入口卡片 → 列表（网格）→ 详情页 → 下载弹窗。
// 全流程纯 HTTP，不需要浏览器（协议见 _gx_protocol.md）。
// ============================================================
(function () {
    const $ = (id) => document.getElementById(id);
    const esc = (s) => (typeof escapeHtml === 'function' ? escapeHtml(String(s == null ? '' : s))
                                                         : String(s == null ? '' : s));

    const st = {
        kind: 'all',
        tag: '',
        keyword: '',
        nsfw: 'all',
        sort: 'updated',
        page: 0,
        pageSize: 60,
        total: 0,
        items: [],
        loading: false,
        done: false,
        synced: false,
        name: '',   // 当前详情页的游戏名 —— 下载时带给快捷方式命名用
        resFiles: {},   // resourceId → 该资源的可直连文件列表（下载弹窗要用）
    };

    function fmtCount(n) {
        n = Number(n) || 0;
        if (n >= 10000) return (n / 10000).toFixed(1) + 'w';
        if (n >= 1000) return (n / 1000).toFixed(1) + 'k';
        return String(n);
    }

    /// 从 ISO 串里取 YYYY-MM-DD（站点给的是 "$D2026-10-06T07:39:45.645Z" 这种）
    function fmtDate(s) {
        const m = /(\d{4})-(\d{2})-(\d{2})/.exec(String(s || ''));
        return m ? (m[1] + '-' + m[2] + '-' + m[3]) : '';
    }

    async function loadStatus() {
        try {
            const s = await invoke('gx_status');
            st.synced = !!(s && s.ready);
            const el = $('gx-status');
            if (el) {
                if (st.synced) {
                    const d = s.synced_at ? new Date(s.synced_at) : null;
                    el.textContent = '本地索引 ' + s.total + ' 个游戏（同人 ' + s.doujin
                        + ' / Galgame ' + s.galgame + '，标签 ' + s.tags + '）'
                        + (d ? ' · 同步于 ' + d.toLocaleString('zh-CN', { hour12: false }) : '');
                } else {
                    el.textContent = '尚未同步 —— 点右上角「同步」从站点拉取全量索引（约 10 秒）';
                }
            }
            return s;
        } catch (e) {
            const el = $('gx-status');
            if (el) el.textContent = '读取索引状态失败: ' + (e && e.message ? e.message : e);
            return null;
        }
    }

    async function loadTags() {
        const rail = $('gx-tag-rail');
        if (!rail) return;
        try {
            const tags = await invoke('gx_tags');
            if (!Array.isArray(tags) || !tags.length) { rail.innerHTML = ''; return; }
            rail.innerHTML = tags.map(function (t) {
                return '<button class="gx-tag-chip' + (st.tag === t.name ? ' active' : '')
                    + '" data-gx-tag="' + esc(t.name) + '">' + esc(t.name)
                    + '<span class="gx-tag-n">' + fmtCount(t.count) + '</span></button>';
            }).join('');
        } catch (e) { rail.innerHTML = ''; }
    }

    function cardHtml(g) {
        const cover = g.cover || g.header || '';
        const tags = (g.tags || []).slice(0, 4);
        const tagsHtml = tags.length
            ? '<div class="game-card-tags">' + tags.map(function (x) { return '<span class="game-tag">' + esc(x) + '</span>'; }).join('') + '</div>'
            : '';
        return ''
        + '<div class="game-card gx-card" data-gx-slug="' + esc(g.slug) + '" data-gx-id="' + g.id + '">'
        +   '<div class="game-card-image">'
        +     (cover ? '<img src="' + esc(cover) + '" loading="lazy" onerror="this.style.display=\'none\'">' : '')
        +     (g.nsfw ? '<span class="gx-badge gx-badge-r18">R18</span>' : '<span class="gx-badge">全年龄</span>')
        +     (g.size ? '<span class="gx-size">' + esc(g.size) + '</span>' : '')
        +   '</div>'
        +   '<div class="game-card-info">'
        +     '<div class="game-card-name" title="' + esc(g.name) + '">' + esc(g.name) + '</div>'
        +     '<div class="game-card-name-original">' + esc(g.version || '') + (g.updated_at ? ' · ' + fmtDate(g.updated_at) : '') + '</div>'
        +     tagsHtml
        +   '</div>'
        + '</div>';
    }

    function renderGrid(append) {
        const grid = $('gx-grid');
        if (!grid) return;
        if (!append) {
            if (!st.items.length) {
                grid.innerHTML = st.synced
                    ? '<div class="mod-empty">没有符合条件的游戏（换个分类或清掉标签筛选试试）</div>'
                    : '<div class="mod-empty">本地还没有索引，点右上角「同步」拉取</div>';
                return;
            }
            grid.innerHTML = st.items.map(cardHtml).join('');
        } else {
            grid.insertAdjacentHTML('beforeend', st.items.map(cardHtml).join(''));
        }
    }

    async function fetchPage(reset) {
        if (st.loading) return;
        if (!st.synced) { renderGrid(false); return; }
        st.loading = true;
        const loader = $('gx-scroll-loader');
        if (loader) loader.style.display = '';
        try {
            const page = reset ? 1 : st.page + 1;
            const res = await invoke('gx_browse', {
                query: {
                    kind: st.kind, tag: st.tag, keyword: st.keyword,
                    nsfw: st.nsfw, sort: st.sort,
                    page: page, page_size: st.pageSize,
                },
            });
            const items = (res && res.items) || [];
            st.total = (res && res.total) || 0;
            st.page = page;
            st.items = reset ? items.slice() : st.items.concat(items);
            st.done = items.length < st.pageSize || st.items.length >= st.total;
            renderGrid(!reset);
            if (loader) loader.style.display = st.done ? 'none' : '';
            const bar = $('gx-status');
            if (bar && st.synced) {
                bar.textContent = '匹配 ' + st.total + ' 个游戏，已显示 ' + st.items.length
                    + (st.tag ? ' · 标签「' + st.tag + '」' : '')
                    + (st.keyword ? ' · 关键词「' + st.keyword + '」' : '');
            }
        } catch (e) {
            showToast('读取 GX 列表失败: ' + (e && e.message ? e.message : e));
        } finally {
            st.loading = false;
        }
    }

    function reset() { st.page = 0; st.done = false; return fetchPage(true); }

    async function syncIndex() {
        const btn = $('gx-sync-btn');
        if (btn) { btn.disabled = true; btn.textContent = '同步中…'; }
        const bar = $('gx-status');
        if (bar) bar.textContent = '正在从 galgamex 拉取全量索引（约 10 秒）…';
        try {
            const r = await invoke('gx_sync');
            showToast('同步完成：' + r.total + ' 个游戏（同人 ' + r.doujin + ' / Galgame ' + r.galgame + '）');
            await loadStatus();
            await loadTags();
            await reset();
        } catch (e) {
            const msg = e && e.message ? e.message : String(e);
            showToast('同步失败: ' + msg);
            if (bar) bar.textContent = '同步失败: ' + msg;
        } finally {
            if (btn) { btn.disabled = false; btn.textContent = '同步'; }
        }
    }

    // ---------- 详情页 ----------
    function resourceHtml(r) {
        const files = r.files || [];
        const unzip = r.unzip_code || 'galgamex.com';
        // ★ 用户要求「点击下载按钮然后选择哪个下载，而不是这样摊开来」
        //   → 资源块只放**一个**下载按钮，文件列表挪到弹窗里（含重复的全部列出）。
        // ★ 用户要求删掉「已排除 N 条（网盘等）」—— 那是内部过滤细节，用户不需要看。
        st.resFiles[r.id] = files;
        const filesHtml = files.length
            ? '<div class="gx-dl-row">'
                + '<button class="gx-dl-btn gx-dl-pick" data-gx-pick="' + r.id + '">下载</button>'
                + '<span class="gx-file-hint">' + files.length + ' 个文件，点击后选择</span>'
              + '</div>'
            : '<div class="gx-file-none">这条资源没有可直连的文件（只剩网盘）</div>';
        const remark = r.remark
            ? '<details class="gx-remark"><summary>更新日志 / 说明</summary><pre>' + esc(String(r.remark).slice(0, 4000)) + '</pre></details>'
            : '';
        return ''
        + '<div class="gx-res-block">'
        +   '<div class="gx-res-head">'
        +     '<span class="gx-res-title">' + esc(r.size || '') + (r.version ? ' · ' + esc(r.version) : '') + '</span>'
        +     '<span class="gx-res-tags">' + (r.kind ? '<span class="game-tag">' + esc(r.kind) + '</span>' : '') + (r.tested === 'tested' ? '<span class="game-tag">已测试</span>' : '') + '</span>'
        +   '</div>'
        +   '<div class="gx-unzip">解压码：<code>' + esc(unzip) + '</code><button class="gx-copy" data-gx-copy="' + esc(unzip) + '">复制</button></div>'
        +   filesHtml + remark
        + '</div>';
    }

    // ★ 暴露给全局：成人页合并了 GX 卡片，点进去要落到 GX 详情页
    window.__vxGxOpenDetail = function (slug, id) { return openDetail(slug, id); };

    /// 从 GX 详情页返回：回到**进来时那个视图**（成人页 / GX 列表），不是死板回 GX。
    function closeDetail() {
        const dp = $('page-gx-detail');
        if (dp) dp.classList.remove('active');
        const gp = document.getElementById('page-game');
        if (gp) gp.classList.add('active');
        const to = st.fromView || 'gx';
        if (typeof switchPageView === 'function') switchPageView('game', to);
    }

    async function openDetail(slug, id) {
        // ★ 记住从哪个视图进来的：成人页合并了 GX 卡片，从那儿点进来返回要回成人页
        st.fromView = (typeof currentActiveView === 'string' && currentActiveView) ? currentActiveView : 'gx';
        const page = $('page-gx-detail');
        const box = $('gx-detail-content');
        if (!page || !box) return;
        document.querySelectorAll('.page').forEach(function (p) { p.classList.remove('active'); });
        page.classList.add('active');
        box.innerHTML = '<div class="loading">加载中...</div>';
        try {
            const d = await invoke('gx_detail', { slug });
            let rs = [];
            let rsErr = '';
            try { rs = await invoke('gx_resources', { slug: slug, gameId: id }); }
            catch (e) { rsErr = String(e && e.message ? e.message : e); }
            const c = (d && d.card) || {};
            st.name = c.name || '';
            st.cover = c.cover || c.header || '';   // 快捷方式弹窗要显示封面
            // ★ 下载弹窗的「创建以游戏名命名的文件夹 / 桌面快捷方式」都读 state.currentDetailGame，
            //   GX 这条链路以前不设它 → 平铺解压到 D:\game，快捷方式只能退回压缩包名。
            state.currentDetailGame = { name: c.name || '', source: 'galgamex', detail_url: slug, extra: { gx_id: id } };
            const shots = (c.screenshots || []).slice(0, 8);
            const resHtml = rsErr
                ? '<div class="mod-empty">读取下载资源失败：' + esc(rsErr) + '</div>'
                : (rs.length ? rs.map(resourceHtml).join('') : '<div class="mod-empty">这个游戏暂时没有可下载资源</div>');
            // ★ 用户要求删掉右上角那排「R18 / 同人游戏 / 264 MB / 作者 / 日期」标签
            //   （meta 那段整个不要了）；标签云保留，那是用来找同类游戏的。
            box.innerHTML = ''
            + '<div class="detail-left">'
            +   '<h2 class="gx-detail-name">' + esc(c.name || slug) + '</h2>'
            +   '<div class="game-card-tags" style="margin:10px 0">'
            +     (c.tags || []).map(function (t) { return '<span class="game-tag">' + esc(t) + '</span>'; }).join('')
            +   '</div>'
            +   ((c.header || c.cover) ? '<img class="gx-detail-banner" src="' + esc(c.header || c.cover) + '" onerror="this.style.display=\'none\'">' : '')
            +   '<h3>游戏介绍</h3>'
            +   '<div class="gx-desc">' + esc(d.description || c.short_desc || '（站点没有提供简介）') + '</div>'
            +   (shots.length ? '<h3>截图</h3><div class="gx-shots">' + shots.map(function (s) { return '<img src="' + esc(s) + '" loading="lazy" onerror="this.style.display=\'none\'">'; }).join('') + '</div>' : '')
            + '</div>'
            + '<div class="detail-right">'
            +   '<h3>下载选项</h3>'
            +   resHtml
            +   '<h3>相关推荐</h3>'
            +   '<div class="gx-related" id="gx-related"><div class="loading">加载中...</div></div>'
            + '</div>';
            invoke('append_frontend_log', { kind: 'GX', detail: '详情 ' + slug + ' 资源 ' + rs.length + ' 条' }).catch(function () {});
            // 相关推荐异步填（同标签最多的其它游戏），失败不影响详情页主体
            invoke('gx_related', { slug: slug, limit: 8 }).then(function (rel) {
                const el = $('gx-related');
                if (!el) return;
                el.innerHTML = (rel && rel.length)
                    ? rel.map(function (g) {
                        const key = _registerCardGame(g);
                        return '<div class="gx-rel-item" onclick="showDetailByKey(\'' + key + '\')">'
                            + '<img src="' + esc(g.header_image || '') + '" loading="lazy" onerror="this.style.display=\'none\'">'
                            + '<div class="gx-rel-name">' + esc(g.name || '') + '</div>'
                            + '</div>';
                    }).join('')
                    : '<div class="mod-empty">暂无相关推荐</div>';
            }).catch(function () {
                const el = $('gx-related');
                if (el) el.innerHTML = '<div class="mod-empty">暂无相关推荐</div>';
            });
        } catch (e) {
            box.innerHTML = '<div class="mod-empty">加载详情失败: ' + esc(e && e.message ? e.message : e) + '</div>';
        }
    }

    /// ★ 下载选择弹窗：一个资源里可能有好几个文件（zip / 7z / apk / 不同版本），
    ///   用户要求「点下载按钮然后选择哪个下载」。**不去重** —— 站点同一份文件常有多个
    ///   镜像，用户要看到全部再自己挑。
    function openPickDialog(resourceId) {
        const modal = $('pick-modal');
        const body = $('pick-modal-body');
        if (!modal || !body) return;
        const files = (st.resFiles || {})[resourceId] || [];
        const title = $('pick-modal-title');
        if (title) title.textContent = '选择要下载的文件（' + files.length + ' 个）';
        body.innerHTML = files.length
            ? files.map(function (f, i) {
                return ''
                + '<div class="gx-file-row">'
                +   '<div class="gx-file-info">'
                +     '<div class="gx-file-name" title="' + esc(f.name) + '">' + esc(f.name || ('文件 ' + (i + 1))) + '</div>'
                +     '<div class="gx-file-meta">' + esc(f.size || '') + '</div>'
                +   '</div>'
                +   '<button class="gx-dl-btn" data-gx-dl="' + resourceId + '" data-gx-dl-idx="' + i + '">下载</button>'
                + '</div>';
            }).join('')
            : '<div class="mod-empty">这个资源没有可直连的文件</div>';
        modal.style.display = '';
    }

    function closePickDialog() {
        const modal = $('pick-modal');
        if (modal) modal.style.display = 'none';
    }

    async function doDownload(resourceId, index, btn) {
        const old = btn ? btn.textContent : '';
        if (btn) { btn.disabled = true; btn.textContent = '换链中…'; }
        try {
            const p = await invoke('gx_pick_download', {
                resourceId: resourceId,
                index: (index == null ? null : index),
            });
            invoke('append_frontend_log', {
                kind: 'GX',
                detail: '选中 resource=' + p.resource_id + ' 候选=' + p.candidates + ' 排除=' + p.pan_skipped + ' url=' + p.url,
            }).catch(function () {});
            if (typeof showDownloadModal === 'function') {
                showDownloadModal({
                    url: p.url,
                    filename: p.name || 'download.zip',
                    label: 'GX 直链',
                    size: p.size || '',
                    unzip_code: p.unzip_code || 'galgamex.com',
                    source: 'galgamex-direct',
                    signed: true, // 放行 galgamex 的"复制到剪贴板"老逻辑，并带上正确 Referer
                    // ★ 记下 GX 的资源 id：预签名链接 1 小时过期，重试/续传时要能重新取一条
                    gx_resource_id: resourceId,
                    gx_index: (index == null ? null : index),
                    // ★ 站点的文件名常是 `#A9667.zip` 这种没意义的串，带上真游戏名给快捷方式命名
                    game_name: st.name || '',
                    cover: st.cover || '',
                });
            } else {
                showToast('下载弹窗不可用');
            }
            closePickDialog();
            if (p.pan_skipped > 0) {
                showToast('已排除 ' + p.pan_skipped + ' 条网盘，从 ' + p.candidates + ' 条可直连文件里取了这条');
            }
        } catch (e) {
            showToast('取下载链接失败: ' + (e && e.message ? e.message : e));
        } finally {
            if (btn) { btn.disabled = false; btn.textContent = old || '下载'; }
        }
    }

    // ---------- 事件 ----------
    // ★★ 文档级事件委托必须在**模块加载时**就挂上，不能等 bind()。
    //   bind() 是「首次进入 GX 视图」才调的，而 GX 卡片/详情现在也出现在成人页上，
    //   从成人页点进 GX 详情时 bind() 从没跑过 → 下载选择弹窗、下载按钮、
    //   复制解压码这些委托监听压根没注册，表现就是「点下载没反应」。
    function bindDelegation() {
        document.addEventListener('click', function (e) {
            const card = e.target.closest('.gx-card');
            if (card) {
                openDetail(card.dataset.gxSlug, Number(card.dataset.gxId));
                return;
            }
            const chip = e.target.closest('[data-gx-tag]');
            if (chip) {
                const tag = chip.dataset.gxTag;
                st.tag = (st.tag === tag) ? '' : tag;
                document.querySelectorAll('[data-gx-tag]').forEach(function (c) {
                    c.classList.toggle('active', c.dataset.gxTag === st.tag);
                });
                const clearBtn = $('gx-tag-clear');
                if (clearBtn) clearBtn.hidden = !st.tag;
                reset();
                return;
            }
            if (e.target.closest('[data-pick-close]')) { closePickDialog(); return; }
            // ★ 详情页「返回」必须挂在**模块级**委托里：它原来在 bind() 里挂，
            //   而 bind() 只有"首次进入 GX 视图"才跑 —— 从成人页点进 GX 详情时
            //   那个监听压根没注册，按钮点了没反应（用户报的「右上角返回没有用」）。
            if (e.target.closest('#gx-detail-back')) { closeDetail(); return; }
            const pk = e.target.closest('[data-gx-pick]');
            if (pk) { openPickDialog(Number(pk.dataset.gxPick)); return; }
            const dl = e.target.closest('[data-gx-dl]');
            if (dl) {
                const idx = dl.dataset.gxDlIdx;
                doDownload(Number(dl.dataset.gxDl), idx == null ? null : Number(idx), dl);
                return;
            }
            const cp = e.target.closest('[data-gx-copy]');
            if (cp) {
                invoke('copy_to_clipboard', { text: cp.dataset.gxCopy })
                    .then(function () { showToast('解压码已复制'); }).catch(function () {});
                return;
            }
        });
    }

    /// 只在「进入 GX 视图」时才需要挂的东西：搜索框/标签清除/返回/无限下滑/同步事件。
    /// 委托部分见 bindDelegation()（那个是模块加载就挂的）。
    function bind() {
        const sbtn = $('gx-search-btn');
        if (sbtn) sbtn.addEventListener('click', function () {
            st.keyword = ($('gx-search-input') || {}).value || '';
            reset();
        });
        const sinp = $('gx-search-input');
        if (sinp) sinp.addEventListener('keypress', function (e) {
            if (e.key === 'Enter') { st.keyword = sinp.value; reset(); }
        });
        const clearBtn = $('gx-tag-clear');
        if (clearBtn) clearBtn.addEventListener('click', function () {
            st.tag = '';
            clearBtn.hidden = true;
            document.querySelectorAll('[data-gx-tag]').forEach(function (c) { c.classList.remove('active'); });
            reset();
        });
        const syncBtn = $('gx-sync-btn');
        if (syncBtn) syncBtn.addEventListener('click', syncIndex);
        // ★ 详情页返回键的监听已挪到 bindDelegation()（模块加载即挂），
        //   这里再挂一次会重复触发；见那边注释。

        // 无限下滑
        const loader = $('gx-scroll-loader');
        if (loader && 'IntersectionObserver' in window) {
            const io = new IntersectionObserver(function (entries) {
                for (let i = 0; i < entries.length; i++) {
                    if (entries[i].isIntersecting && !st.done && !st.loading) fetchPage(false);
                }
            }, { rootMargin: '600px' });
            io.observe(loader);
        }
        // 同步进度事件
        try {
            const t = window.__TAURI__ || {};
            if (t.event && t.event.listen) {
                t.event.listen('gx-sync-progress', function (ev) {
                    const bar = $('gx-status');
                    if (bar && ev && ev.payload && ev.payload.message) bar.textContent = ev.payload.message;
                });
            }
        } catch (e) { /* 没有 event API 就算了 */ }
    }

    // 委托监听立刻挂上（不等进入 GX 视图）—— 见 bindDelegation 的说明
    bindDelegation();

    let inited = false;
    async function onShow() {
        if (!inited) { inited = true; bind(); await loadStatus(); await loadTags(); }
        else { await loadStatus(); }
        if (st.synced && st.items.length === 0 && !st.loading) await reset();
        if (!st.synced) renderGrid(false);
    }

    window.__vxGx = { init: onShow, onShow: onShow, reload: reset, sync: syncIndex };
})();

async function showDownloadModal(dl) {
    try {
        // ★ 入口参数防御 (竞态/点按钮太快可能传 undefined/null)
        if (!dl) {
            showToast('下载信息为空，请重新点击下载');
            return;
        }
        if (!dl.url) {
            showToast('下载链接为空，请重新打开详情页');
            return;
        }
        state.pendingDownloadLink = dl;

        // ★ 快捷方式命名 / 「创建以游戏名命名的文件夹」都要用**真游戏名**。
        //   下载条目本身往往只带压缩包名（byrut 是英文、GX 是 #A9667 这种），
        //   于是桌面出现 "Survival Log.lnk" 而不是「生存日志」（用户报「名字还不对」）。
        //   详情页当前游戏的名字是最可信的来源，缺了就在这里补上。
        if (!dl.game_name && !dl.gameName) {
            const g = state.currentDetailGame;
            if (g && g.name) dl.game_name = g.name;
        }
        // 封面（快捷方式弹窗里显示，让用户确认是不是这个游戏）
        if (!dl.cover) {
            const g = state.currentDetailGame;
            if (g && (g.header_image || g.cover)) dl.cover = g.header_image || g.cover;
        }

        // ★ 安全获取元素 helper: 缺元素立即抛带 ID 的错 (用户可见提示具体缺哪个)
        const DIR_FALLBACK = 'D:\\game';
        const $el = function(id) {
            const el = document.getElementById(id);
            if (!el) throw new Error('页面缺少元素 #' + id + ' (index.html 可能不完整)');
            return el;
        };

        // 填充文件信息 (filename 为 null/undefined 时已防御, 原代码已处理 OK)
        $el('modal-file-name').textContent = dl.filename || '未知文件名';

        // ★★★ 游戏/文件大小显示: 兼容所有后端可能传的字段名 (字节数 → 格式化 MB/GB) ★★★
        let sizeBytes = 0;
        const anySize = dl.size ?? dl.file_size ?? dl.fileSize ?? dl.total_size ?? dl.totalSize ?? dl.filesize ?? dl.length ?? dl.byteLength ?? null;
        const fileSizeEl = $el('modal-file-size');
        if (anySize !== null && anySize !== undefined && anySize !== '') {
            if (typeof anySize === 'string' && /\d+\s*(B|KB|MB|GB|TB)/i.test(anySize)) {
                sizeBytes = -1;
                fileSizeEl.textContent = '游戏大小: ' + anySize;
            } else {
                const n = Number(anySize);
                if (!isNaN(n) && isFinite(n) && n > 0) {
                    sizeBytes = n;
                    const fmt = (typeof formatBytes === 'function') ? formatBytes(n) : (n + ' 字节');
                    fileSizeEl.textContent = '游戏大小: ' + fmt + ' (' + n.toLocaleString() + ' 字节)';
                } else {
                    fileSizeEl.textContent = '';
                }
            }
        } else {
            fileSizeEl.textContent = '';
        }

        // 加载配置填充默认路径 (★ 加固: config=null / invoke抛错 都兜底到 DIR_FALLBACK, 避免空目录被报错)
        try {
            const config = await invoke('get_config');
            if (config) {
                $el('modal-download-dir').value = config.download_dir || DIR_FALLBACK;
                $el('modal-extract-dir').value = config.extract_dir || config.download_dir || DIR_FALLBACK;
            } else {
                $el('modal-download-dir').value = DIR_FALLBACK;
                $el('modal-extract-dir').value = DIR_FALLBACK;
            }
        } catch (e) {
            console.error('加载配置失败, 启用默认目录:', e);
            try {
                $el('modal-download-dir').value = DIR_FALLBACK;
                $el('modal-extract-dir').value = DIR_FALLBACK;
            } catch (_) {}
        }

        // 预填解压密码(如果链接自带)
        $el('modal-extract-code').value = dl.unzip_code || dl.code || '';

        // ============================================================
        // 识别资源类型
        // ============================================================
        const safeFn = String(dl.filename || '');
        const fnLower = safeFn.toLowerCase();
        const urlLower = (dl.url || '').toLowerCase();
        const dlType = String(dl.type || dl.link_type || '').toLowerCase();
        const isBT = dlType === 'torrent'
                  || dlType === 'bt'
                  || fnLower.endsWith('.torrent')
                  || urlLower.startsWith('magnet:')
                  || urlLower.endsWith('.torrent')
                  || String(dl.engine || '').toLowerCase() === 'bt';
        const safeExt = (safeFn.split('.').pop() || '').toLowerCase();
        const isArchive = ['zip', 'rar', '7z'].includes(safeExt);

        // 下载链接展示 (KO资源 / HTTP直链 / BT 均显示原始地址, 地址下方)
        const linkRow = document.getElementById('modal-download-link-row');
        const linkInput = document.getElementById('modal-download-link');
        if (linkRow && linkInput) {
            if (dl.url) {
                linkInput.value = dl.url;
                linkRow.style.display = '';
            } else {
                linkRow.style.display = 'none';
            }
        }

        // 每次打开先重置种子预览区
        const previewSection = document.getElementById('modal-preview-section');
        if (previewSection) previewSection.style.display = 'none';
        const autoExtractRow = document.getElementById('modal-auto-extract-row');

        if (isBT) {
            $el('modal-extract-dir-label').textContent = 'BT下载到';
            $el('modal-extract-code-row').style.display = 'none';
            // 解压/删除/快捷方式选项由种子解析结果决定 (解析出压缩包才显示)
            if (autoExtractRow) autoExtractRow.style.display = 'none';
            $el('modal-auto-extract').checked = false;
            $el('modal-auto-extract').disabled = true;
            $el('modal-delete-archive-row').style.display = 'none';
            // 异步解析种子目录内容 (不阻塞弹窗显示)
            loadTorrentPreview(dl);
        } else if (isArchive) {
            $el('modal-extract-dir-label').textContent = '解压到';
            $el('modal-auto-extract-label').textContent = '下载后自动解压(zip/rar/7z)';
            if (autoExtractRow) autoExtractRow.style.display = '';
            $el('modal-auto-extract').checked = true;
            $el('modal-auto-extract').disabled = false;
            $el('modal-extract-code-row').style.display = '';
            $el('modal-delete-archive-row').style.display = '';
        } else {
            $el('modal-extract-dir-label').textContent = '下载到';
            $el('modal-auto-extract-label').textContent = '下载后自动解压(zip/rar/7z)';
            if (autoExtractRow) autoExtractRow.style.display = 'none';
            $el('modal-auto-extract').checked = false;
            $el('modal-auto-extract').disabled = true;
            $el('modal-extract-code-row').style.display = 'none';
            $el('modal-delete-archive-row').style.display = 'none';
        }

        // ★ 最后才把弹窗显示出来 (中途任何一步报错都不会显示半截弹窗)
        $el('download-modal').style.display = 'flex';
    } catch (e) {
        console.error('showDownloadModal 异常:', e);
        showToast('无法打开下载设置弹窗: ' + (e && e.message ? e.message : String(e)));
    }
}

// BT 种子预解析: 下载前弹窗中列出目录内容并识别压缩包 (zip/rar/7z...)
async function loadTorrentPreview(dl) {
    const section = document.getElementById('modal-preview-section');
    const listEl = document.getElementById('modal-preview-list');
    const titleEl = document.getElementById('modal-preview-title');
    const metaEl = document.getElementById('modal-preview-meta');
    if (!section || !listEl) return;
    section.style.display = '';
    if (titleEl) titleEl.textContent = '种子内容';
    if (metaEl) metaEl.textContent = '正在解析…';
    listEl.innerHTML = '<div class="modal-preview-loading">正在解析种子内容…</div>';
    const fmt = (typeof formatBytes === 'function') ? formatBytes : function (n) { return n + ' B'; };
    try {
        const res = await invoke('parse_torrent_preview', { url: dl.url, headers: dl.headers || null });
        if (!res || !res.ok) {
            if (metaEl) metaEl.textContent = '';
            listEl.innerHTML = '<div class="modal-preview-empty">' + escapeHtml((res && res.error) || '解析失败') + '</div>';
            return;
        }
        if (res.name && titleEl) titleEl.textContent = res.name;
        if (metaEl) metaEl.textContent = res.file_count + ' 个文件 · ' + fmt(res.total_size || 0);
        if (!res.files || res.files.length === 0) {
            listEl.innerHTML = '<div class="modal-preview-empty">磁力链接需连接后才能获取文件列表</div>';
        } else {
            const maxShow = 200;
            const shown = res.files.slice(0, maxShow);
            listEl.innerHTML = shown.map(function (f) {
                const badge = f.is_archive
                    ? '<span class="modal-preview-badge">' + escapeHtml(String(f.ext || '压缩包').toUpperCase()) + '</span>'
                    : '';
                return '<div class="modal-preview-item"><span class="modal-preview-name" title="' + escapeHtml(f.name) + '">'
                    + escapeHtml(f.name) + '</span>' + badge
                    + '<span class="modal-preview-size">' + fmt(f.size || 0) + '</span></div>';
            }).join('') + (res.files.length > maxShow
                ? '<div class="modal-preview-more">… 其余 ' + (res.files.length - maxShow) + ' 个文件未显示</div>'
                : '');
        }
        // 解析出压缩包 → 显示自动解压/删除压缩包/建快捷方式选项, 供用户在弹窗中自选确认
        const autoExtractRow = document.getElementById('modal-auto-extract-row');
        const autoExtractEl = document.getElementById('modal-auto-extract');
        const delRow = document.getElementById('modal-delete-archive-row');
        if (res.has_archive && autoExtractEl) {
            if (autoExtractRow) autoExtractRow.style.display = '';
            autoExtractEl.disabled = false;
            autoExtractEl.checked = true;
            const lbl = document.getElementById('modal-auto-extract-label');
            if (lbl) lbl.textContent = '下载后自动解压压缩包';
            if (delRow) delRow.style.display = '';
        } else {
            if (autoExtractRow) autoExtractRow.style.display = 'none';
            if (delRow) delRow.style.display = 'none';
        }
    } catch (e) {
        if (metaEl) metaEl.textContent = '';
        listEl.innerHTML = '<div class="modal-preview-empty">解析种子失败: ' + escapeHtml(e && e.message ? e.message : String(e)) + '</div>';
    }
}

window.closeDownloadModal = function() {
    document.getElementById('download-modal').style.display = 'none';
    state.pendingDownloadLink = null;
    // 清除按钮 loading 状态 (下次打开弹窗恢复)
    const startBtn = document.getElementById('modal-start-download');
    if (startBtn) {
        startBtn.disabled = false;
        startBtn.dataset.loading = 'false';
        startBtn.textContent = startBtn.dataset.origText || startBtn.textContent;
    }
};

// 浏览目录按钮
window.browsePath = async function(inputId) {
    try {
        const path = await invoke('browse_path');
        if (path) {
            document.getElementById(inputId).value = path;
        }
    } catch (e) {
        console.error('浏览目录失败:', e);
    }
};

// 开始下载按钮(弹窗中) - 防重入 + 兜底目录 + extractDir兜底
document.getElementById('modal-start-download')?.addEventListener('click', async () => {
    const dl = state.pendingDownloadLink;
    if (!dl) return;

    const startBtn = document.getElementById('modal-start-download');
    // ★ 防重入: 防止双击/快速点击 -> 重复下载
    if (startBtn.disabled || startBtn.dataset.loading === 'true') return;
    if (!startBtn.dataset.origText) startBtn.dataset.origText = startBtn.textContent;
    startBtn.disabled = true;
    startBtn.dataset.loading = 'true';
    const originalText = startBtn.dataset.origText;
    startBtn.textContent = '启动中...';
    try {

    const DEFAULT_DIR = 'D:\\game';
    let downloadDir = (document.getElementById('modal-download-dir').value || '').trim();
    let extractDir = (document.getElementById('modal-extract-dir').value || '').trim();
    // ★ Bug#4 兜底: get_config 慢/失败时目录为空 → 用 D:\game
    if (!downloadDir) { downloadDir = DEFAULT_DIR; document.getElementById('modal-download-dir').value = downloadDir; }
    if (!extractDir)  { extractDir  = downloadDir;  document.getElementById('modal-extract-dir').value  = extractDir;  }
    // ★ Bug#5 兜底: extractDir 即使有值也保证非空, 否则 = downloadDir
    const effectiveExtractDir = extractDir || downloadDir;
    const extractCode = document.getElementById('modal-extract-code').value.trim();
    const autoExtract = document.getElementById('modal-auto-extract').checked;
    const deleteArchive = document.getElementById('modal-delete-archive').checked;
    const createFolder = document.getElementById('modal-create-folder').checked;
    // ★ 新增: 桌面快捷方式开关 (默认勾选, 仅在解压成功后才会创建)
    const createShortcut = document.getElementById('modal-create-shortcut') ? document.getElementById('modal-create-shortcut').checked : true;

    // 这里再验证一次 (兜底后应不可能空)
    if (!downloadDir) {
        alert('请选择下载目录');
        return;
    }

    // 确定最终文件路径
    let finalDir = downloadDir;
    if (createFolder && state.currentDetailGame) {
        const safeName = (state.currentDetailGame.name || 'game').replace(/[<>:"/\\|?*]/g, '_');
        finalDir = `${downloadDir}\\${safeName}`;
    }
    const filePath = `${finalDir}\\${dl.filename || 'download'}`;

    const taskId = `dl_${Date.now()}_${Math.random().toString(36).substr(2, 8)}`;

    // 关闭弹窗 (按钮 loading 会在 closeDownloadModal 里重置, 但 invoke 之前就关了, 所以正常)
    closeDownloadModal();

    // ★ 立即跳转下载页 + 占位任务: start_download 对 BT (byrut 种子要先 HTTP 下载 .torrent)
    //   可能耗时数秒, 期间页面无反馈像"点了没反应". 先切页展示 starting 占位,
    //   realTaskId 返回后再迁移 Map key (BT 任务 id 是 bt-xxx)
    state.downloads.set(taskId, {
        name: dl.filename,
        // ★ 详情页/GX 带过来的真游戏名 —— 桌面快捷方式优先用它命名（压缩包名往往没意义）
        gameName: dl.game_name || dl.gameName || '',
        cover: dl.cover || '',
        // ★ 来源：只有 GX（galgamex）资源才弹"选程序 + 确认名字"的窗，KO/BY 全自动
        source: dl.source || '',
        // ★ GX 预签名链接 1 小时过期 → 记下资源 id，重试/续传时能重新取一条新链接
        gxResourceId: dl.gx_resource_id || null,
        gxIndex: (dl.gx_index == null ? null : dl.gx_index),
        // ★ 下载根目录 —— 平铺解压时 extractedPath 就是它，那种名字不能当游戏名
        downloadRoot: downloadDir,
        url: dl.url,
        filePath: filePath,
        progress: 0,
        speed: '',
        state: 'starting',
        total: 0,
        downloaded: 0,
        engine: undefined,
        taskId: taskId,
        autoExtract: autoExtract,
        // ★ Bug 修复 (2026-09-12): extractDir 必须用 finalDir (已含游戏名子目录),
        //   而非 effectiveExtractDir (= downloadDir, 无游戏名). 否则 handleAutoExtract
        //   会解压到 D:\game\文件名\, 而下载文件在 D:\game\游戏名\, 路径不一致!
        extractDir: finalDir,
        extractCode: extractCode,
        deleteArchive: deleteArchive,
        createFolder: createFolder,
        createShortcut: createShortcut,
    });
    renderDownloadList();
    // ★ 任务一建出来就立刻落盘（不等 4 秒的兜底计时）：否则"刚点开始下载就
    //   关软件/断电"这种情况，列表里那条还没写进 localStorage，重启后就查无此单。
    schedulePersistDownloads();
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    document.querySelector('[data-page="downloads"]').classList.add('active');
    document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
    document.getElementById('page-downloads').classList.add('active');

    // GX 资源: 复制链接到剪贴板, 提示用户去网站下载
    // ★ 例外: 站外抓取拿到的**签名直链**(dl.signed) 是能直接下的,
    //   不能再走这条"复制去手动下"的老路 —— 那正是我们要解决的问题。
    const source = state.currentDetailGame ? state.currentDetailGame.source : '';
    if (!dl.signed && (source === 'galgamex' || dl.url.includes('galgamex.com'))) {
        try {
            await invoke('copy_to_clipboard', { text: dl.url });
            alert(`GX 资源请前往网站下载\n\n下载链接已复制到剪贴板\n文件: ${dl.filename}\n解压码: ${dl.unzip_code || 'galgamex.com'}\n\n请打开 galgamex.net 手动下载`);
        } catch (e) {
            alert(`下载链接:\n${dl.url}\n\n解压码: ${dl.unzip_code || 'galgamex.com'}`);
        }
        return;
    }

    // BY/KO 走内置下载器
    let headers = [];
    // ★ 站外抓取的签名直链 (2026-10-07 实测): 引擎默认头里的
    //   `Referer: about:blank` 会被这个 CDN 直接 **403**。
    //   实测三种 Referer: about:blank → 403, 空 → 206, https://www.galgamex.net/ → 206。
    //   所以这里必须显式覆盖成站点自身的 Referer。
    if (dl.signed) {
        headers = [
            ['Referer', 'https://www.galgamex.net/'],
            ['User-Agent', 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/154.0.0.0 Safari/537.36'],
        ];
    } else if (source === 'byrut' || dl.url.includes('byrutgame.org')) {
        headers = [
            ['Referer', 'https://byrutgame.org/'],
            ['User-Agent', 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'],
        ];
    } else if (source === 'koyso' || dl.url.includes('playzip.com')) {
        headers = [
            ['Referer', 'https://playzip.com/'],
            ['User-Agent', 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'],
        ];
    }

    try {
        // ★★ GX 的下载链接是**预签名 URL，1 小时就过期**（X-Amz-Expires=3600）。
        //   用户报「GX 资源一直连接中」就是这种：任务建好后过一阵才真正开始下载
        //   （或者暂停后过很久才继续），链接已失效 → CDN 一律 403 → 引擎一直重试、
        //   前端一直显示"连接中..."，永远不动。
        //   这里在**真正开下之前**检查一次，过期就重新取一条新链接。
        dl.url = await refreshGxUrlIfExpired(dl);
        // start_download 返回真实的 task_id (BT 任务会是 bt-xxx, 而非 dl_xxx)
        const realTaskId = await invoke('start_download', {
            taskId: taskId,
            url: dl.url,
            filePath: filePath,
            headers: headers,
        });

        // ★ 占位任务迁移: 后端可能已通过事件/download-started 建立了 realTaskId 条目;
        //   若无则迁移占位条目到 realTaskId (BT 任务 key 必须是 bt-xxx 才能收到进度)
        if (realTaskId && realTaskId !== taskId) {
            if (!state.downloads.has(realTaskId)) {
                const entry = state.downloads.get(taskId);
                if (entry) {
                    entry.taskId = realTaskId;
                    // BT 任务: engine 标记 bt (进度条样式 + 跳过自动解压)
                    if (String(realTaskId).startsWith('bt-')) {
                        entry.engine = 'bt';
                        // ★ 修复: byrut BT 后端会去掉 .zip/.rar/.7z 等扩展名作为下载目录,
                        //   前端也必须同步去掉扩展名, 否则"打开"按钮路径不匹配 → 打开错误的父目录
                        const exts = ['.torrent', '.zip', '.rar', '.7z', '.tar', '.gz', '.bz2'];
                        for (const ext of exts) {
                            if (entry.filePath && entry.filePath.toLowerCase().endsWith(ext)) {
                                entry.filePath = entry.filePath.slice(0, -ext.length);
                                break;
                            }
                        }
                    }
                    state.downloads.set(realTaskId, entry);
                }
            }
            state.downloads.delete(taskId);
            renderDownloadList();
        } else {
            // 非 BT: 更新 engine 标记
            const entry = state.downloads.get(taskId);
            if (entry) { entry.state = 'running'; renderDownloadList(); }
        }
    } catch (e) {
        // 启动失败: 占位任务标记失败 (不再创建新条目)
        const entry = state.downloads.get(taskId);
        if (entry) {
            entry.state = 'failed';
            entry.error = String(e);
            renderDownloadList();
        }
        alert(`下载启动失败: ${e}`);
    }

    } finally {
        // 无论成功失败, 最后都重置按钮状态 (以防 closeDownloadModal 没被调用)
        if (startBtn) {
            startBtn.disabled = false;
            startBtn.dataset.loading = 'false';
            startBtn.textContent = originalText || '开始下载';
        }
    }
});

// ============================================================
// 下载管理
// ============================================================
function renderDownloadList() {
    const list = document.getElementById('download-list');
    // ★ A0: DOM null 检测 → 若 UI 未完成初始化但 adopt 先触发, 给出可见诊断 Toast (只打 1 次)
    if (!list) {
        if (!window._toastRenderMissingShown) {
            window._toastRenderMissingShown = true;
            try { showToast('⚠ 下载列表 DOM 未就绪 (page-downloads/#download-list 尚未挂载)\n请稍等 1-2 秒初始化完成后重试, 或刷新主窗口', 'warn', 3500); } catch(_) {}
        }
        console.warn('[renderDownloadList] #download-list DOM 元素不存在, 稍后再渲染');
        return;
    }
    if (typeof state === 'undefined' || !state || !(state.downloads instanceof Map)) {
        try { showToast('⚠ state.downloads 未初始化 (下载数据丢失)', 'error', 3000); } catch(_) {}
        return;
    }
    if (state.downloads.size === 0) {
        const emptyHtml = '<div class="empty-state">暂无下载任务</div>';
        if (list._lastHtml !== emptyHtml) {
            list.innerHTML = emptyHtml;
            list._lastHtml = emptyHtml;
        }
        schedulePersistDownloads();
        return;
    }
    // ==================================================================
    // ★★★★★ 闪烁/进度弹一下不动 根因修复 (最终版) — newHtml 必须 100% 稳定, 任何动态字节都不能写!
    //
    //   旧版问题: diagHtml 里写了动态 engLabel(由 dl.engine 决定, 轮询 event 会改 engine! → _lastHtml 必不等)
    //            → 每 200ms 重建整卡 → 按钮 hover 鼠标刚好离开/重新进入前一瞬间 DOM 销毁 → "闪一下才正常".
    //            → 进度条 width 才设好就被 innerHTML 重置为 0% → 用户视觉 "弹一下就不动了".
    //
    //   新版修复原则:
    //     ① newHtml 里 绝对禁止 引用 dl.engine / dl.url / dl.name / dl.type / dl.link_type —— 这些都可变!
    //     ② isBT 标识不再依赖运行时动态字段 (magnet/.torrent 只是 URL, 轮询中不变但我要更保守)
    //       → 只用 String(id).startsWith('bt-') 这种 taskId 级静态判断
    //     ③ 诊断壳: 放三个 span 占位符 (.dl-engine-tag 空壳 + .bt-tag 空壳), patch 里补 textContent/显示
    //     ④ data-canon-state + 按钮 = 纯由 canon 8 态决定 (已经是 normalize 后的稳定态)
    //     最终 newHtml 的唯一性只依赖: 任务数 × (task-id + canon + unknown-size-flag)
    //       只要不新建/不删除/不切换 canon/不 unknown-size 翻转 → _lastHtml 永远相等 → 永不重建!
    // ==================================================================
    const newHtml = Array.from(state.downloads.entries()).map(([id, dl]) => {
        const canon = normalizeDlState(dl.state);
        const unknownSize = canon === 'running' && (!dl.total || dl.total === 0);

        // ===== BT 标识: 只用稳定 task-id (bt- 前缀由后端 BT 引擎启动时返回, 永不改变) =====
        //       任何 URL/dl.type/dl.engine 等运行时可变字段 一律不用!
        const idStr = String(id);
        const tid = JSON.stringify(idStr);
        const btHint = idStr.startsWith('bt-');

        // ===== 诊断壳 (纯占位: 不写任何动态字! patch 里补 textContent/样式) =====
        const diagShell =
              '<span class="bt-tag" style="display:none" title="BitTorrent 自研下载器"></span>'
            + '<div class="dl-engine-tag" style="display:none"></div>';
        // 注: 即便不是 BT 任务, 也先写入空壳占位 (display:none, patch 时再按需显示), 保证字节一致

        // stateText: 结构占位 (span 壳). 文本全部由 patchDownloadItemIncrementally 填
        let stateText = '';
        switch (canon) {
            case 'starting':
            case 'running':
            case 'extracting':
            default:
                stateText = '<span class="dl-speed"></span> <span class="dl-pct"></span>';
                break;
            case 'paused':
                stateText = '已暂停';
                break;
            case 'completed':
                stateText = '已完成';
                break;
            case 'failed':
                // error 文也用 span 壳, escape 交给 patch 用 textContent 写 (避免 error 变了 rebuild!)
                stateText = '失败: <span class="dl-error-text"></span>';
                break;
            case 'canceled':
                stateText = '已取消';
                break;
        }

        // 下载进度条: 结构属性
        const dlBarClass = unknownSize ? 'download-progress-fill progress-indeterminate' : 'download-progress-fill';
        const dlBarAttr = unknownSize ? 'data-dl-bar="indeterminate"' : 'data-dl-bar="normal"';

        // ★ Bug 修复 (2026-09-12): extract-info 移出 extract-progress-bar 的 overflow:hidden 容器,
        //   否则 height:8px + overflow:hidden 会裁剪掉解压百分比/文件名文字!
        //   改用 extract-progress-wrap 包裹 (条 + 文字), 显示/隐藏控制 wrap 整体
        const extractBarHtml = '<div class="extract-progress-wrap" style="display:none"><div class="extract-progress-bar"><div class="extract-progress-fill" style="width:0%"></div></div><span class="extract-info"></span></div>';
        const btBarHtml = '<div class="bt-progress-bar" style="display:none" data-bt-bar><div class="bt-progress-fill" style="width:0%"></div><span class="bt-info"></span></div>';

        // ================================================================
        // ★★★ 按钮 hover 闪 终极根治: 不再按 canon 选择生成 按钮子集
        //   旧版: running → pause+cancel, paused→resume+cancel ... → canon 变化时 actionsHtml 字节变 → fingerprint 变 → 整卡重建 → 按钮 DOM 销毁 → hover 闪!
        //   新版: 永远一次性渲染 全部 5 个按钮 (pause/resume/cancel/open/retry)
        //         壳用统一稳定 data-role, 每个按钮 display 由 data-visible-role 控制
        //         → canon 变化 → 只改 style.display (3 行 JS) → DOM 永不重建! → hover 永不闪!
        // ================================================================
        // 预期可见角色 (由 canon 决定)
        // running/starting → [pause, cancel]
        // paused → [resume, cancel]
        // completed/extracting → [open]
        // failed/canceled → [retry]
        // 其余全部 display:none
        const visibleMap = { // canon → Set(role)
            starting: new Set(['pause','cancel']),
            running:  new Set(['pause','cancel']),
            paused:   new Set(['resume','cancel']),
            completed:new Set(['open']),
            extracting:new Set(['open']),
            failed:   new Set(['retry','delete']),
            canceled: new Set(['retry','delete']),
        };
        const vs = visibleMap[canon] || visibleMap.running;
        const mk = (role, sym, title) => {
            const show = vs.has(role) ? '' : 'display:none;';
            return '<button class="dl-action-btn ' + role + '" data-role="' + role + '"'
                 + ' data-action="' + role + '" data-task-id=' + tid
                 + ' style="' + show + '" title="' + title + '">' + sym + '</button>';
        };
        const actionsHtml =
              mk('pause',  '⏸',      '暂停')
            + mk('resume', '▶',      '继续')
            + mk('cancel', '✕',      '取消')
            + mk('open',   '📂 打开', '打开文件')
            + mk('retry',  '↻ 重试',  '重试')
            + mk('delete', '🗑 删除', '删除已下载的文件 (含失败的压缩包)');

        // ★★★ 100% 稳定壳: name/url/engine-title 全部空字符串壳 (不影响 _lastHtml 比较!)
        //   patchDownloadItemIncrementally 会在首次/结构变化时用 textContent 填真实内容
        return '<div class="download-item" data-task-id="' + idStr + '" data-canon-state="' + canon + '"' + (btHint ? ' data-engine-hint="bt"' : '') + '>'
             +   '<div class="download-item-header">'
             +     '<span class="download-item-name"></span>'
             +     '<span class="download-item-status">' + stateText + '</span>'
             +     '<span class="dl-stall-badge" style="display:none;margin-left:6px;padding:1px 6px;border-radius:3px;font-size:10px;font-weight:bold;background:#c0392b;color:#fff;line-height:1.2;"></span>'
             +   '</div>'
             +   '<div class="download-item-url"></div>' // title 也空壳, patch 里补
             +   diagShell
             +   '<div class="download-progress-bar" ' + dlBarAttr + '>'
             +     '<div class="' + dlBarClass + '" style="width:0%"></div>'
             +   '</div>'
             +   btBarHtml
             +   extractBarHtml
             +   '<div class="download-item-info">'
             +     '<span class="dl-size-text"></span>'
             +     '<div class="dl-actions">' + actionsHtml + '</div>'
             +   '</div>'
             + '</div>';
    }).join('');
    // ==================================================================
    // ★ 双保险: 结构指纹 (字符串相等 之外 再加 Set 级别比较 → 防止 whitespace / 稳定壳之外的细微字节差误判)
    //   指纹 = `${state.downloads.size}|${canon1}:${id1}|${canon2}:${id2}…`
    //   如果字符串级误判 _lastHtml === newHtml 没命中, 但实际指纹没变 → 不重建!
    // ==================================================================
    let structFingerprint = String(state.downloads.size);
    for (const [id, dl] of state.downloads.entries()) {
        const c = normalizeDlState(dl.state);
        const u = (c === 'running' && (!dl.total || dl.total === 0)) ? 'U' : 'K';
        structFingerprint += '|' + String(id) + ':' + c + ':' + u;
    }
    const fingerprintMatch = (list._lastStructFingerprint === structFingerprint);
    const htmlMatch = (list._lastHtml === newHtml);
    let rebuild = !(htmlMatch && fingerprintMatch);

    // ==================================================================
    // ★★★ 进度条又不动的最终保险: DOM/Map 一致性同步
    //
    // 场景:  (极高频导致"看似进度永远卡住"的元凶: 前一次render只完成了一半 / 指纹相等误判 DOM 没建全)
    //        state.downloads 有 N 条, 但 DOM 里 <div class="download-item"> 数量 ≠ N
    //        OR 任一条 taskId 在 state 里存在 但 DOM 没有对应节点
    //   → 即便指纹 + HTML 全部相等, 也强制重建, 绝不再出现"Map里有进度但DOM永远不更新"
    // ==================================================================
    if (!rebuild) {
        const domItems = list.querySelectorAll(':scope > .download-item');
        let domIds = new Set();
        for (let i = 0; i < domItems.length; i++) { const it = domItems[i]; if (it && it.dataset && it.dataset.taskId) domIds.add(String(it.dataset.taskId)); }
        // 1. 数量不一致 → 强制
        if (domItems.length !== state.downloads.size) { rebuild = true; }
        else {
            // 2. 任一条 state 里没有 DOM → 强制
            for (const [sid,] of state.downloads.entries()) {
                if (!domIds.has(String(sid))) { rebuild = true; break; }
            }
        }
        // 3. 任一个下载进度条没拿到 style.width (首壳 width:0%) 但 dl.progress>0.5 → 说明 patch 没跑成功 (致命!)
        //    旧版: 10% 采样触发 → 强制重建 (这本身就导致 hover 闪 + 进度条 0% 重建 = 弹一下 不动! → 已彻底删除!)
        //    新版: 检测到真的 DOM 没进度 → 只调用 patchAllDownloadsIncrementally() 直写 DOM, 绝不 rebuild 整卡
    }
    if (rebuild) {
        list.innerHTML = newHtml;
        list._lastHtml = newHtml;
        list._lastStructFingerprint = structFingerprint;
        window.__forceRebuildCnt = (window.__forceRebuildCnt || 0) + 1;
    }
    // ================================================================
    // ★ 旧版 10% 采样 "检测不到进度 → 强制重建整卡" 已删除 (会导致 hover 闪 + 进度条归零重建 → 弹一下不动!)
    //   新版: 改成检测到 任何 任务的 DOM 进度条宽度未同步 → 仅 patchAll 直写 (不重建任何节点, 按钮保留不闪!)
    // ================================================================
    if (!rebuild) {
        let needPatch = false;
        try {
            for (const [sid, sdl2] of state.downloads.entries()) {
                if (!sdl2) continue;
                const canon2 = normalizeDlState(sdl2.state || 'running');
                const unknown2 = (canon2 === 'running' && (!sdl2.total || sdl2.total === 0));
                const pctNow = Number(sdl2.progress) || 0;
                if (unknown2 || pctNow <= 0.2) continue;
                const csid = CSS.escape ? CSS.escape(String(sid)) : String(sid);
                const one = document.querySelector('.download-item[data-task-id="' + csid + '"]');
                if (!one) { needPatch = true; break; }
                const fill = one.querySelector('.download-progress-fill');
                if (!fill) { needPatch = true; break; }
                const w = String(fill.style.width || '');
                const curPct = parseFloat(w);
                if (!w || w === '0%' || isNaN(curPct) || Math.abs(curPct - pctNow) > 0.1) {
                    needPatch = true; break;
                }
            }
        } catch(_) { needPatch = true; }
        if (needPatch) { try { patchAllDownloadsIncrementally(); } catch(_){} }
    }
    // 首次 render 壳是空的, 立刻 patch 一遍动态内容 (否则用户看到全 0% 空壳闪一下)
    try { patchAllDownloadsIncrementally(); } catch (e) {}
    schedulePersistDownloads();
}

// state → canonical 8 态 (保证 SpeedEngine 乱返回 downloading/connecting 都不会导致每 200ms rebuild)
function normalizeDlState(s) {
    switch (s) {
        case 'downloading':
        case 'connecting':
            return 'running';
        case 'started':
            return 'running';
        case 'probe':
            return 'starting';
        default:
            return ['starting','running','paused','completed','extracting','failed','canceled'].includes(s) ? s : 'running';
    }
}

// ======================================================================
// ★★★ 增量 DOM 更新 (彻底解决按钮 hover 闪烁的核心)
//   只 patch "每 200ms 会变的非结构性字段": 进度条宽度/速度/百分比/下载量文本
//   绝对不动 <button class="dl-action-btn"> 节点 → 鼠标 hover 永远不丢失 → 不闪!
//   结构性变化 (新建任务/删除任务/state切换→按钮类型变) 才走 renderDownloadList()
// ======================================================================

function patchDownloadItemIncrementally(dl, id) {
    const qid = CSS.escape ? CSS.escape(String(id)) : String(id);
    let item = document.querySelector('.download-item[data-task-id="' + qid + '"]');
    // ================================================================
    // ★ 兜底 (进度弹一下不动的根治)
    //   找不到卡片 → 500ms 节流下强制重建, 保证不会死循环
    // ================================================================
    if (!item) {
        if (!window._dlRebuildThrottle || Date.now() - window._dlRebuildThrottle > 500) {
            window._dlRebuildThrottle = Date.now();
            try { renderDownloadList(); } catch (e) {}
        }
        return;
    }
    const canon = normalizeDlState(dl.state);
    const idStr = String(id);
    const tidJson = JSON.stringify(idStr);

    // ================================================================
    // ★★★ B2b 结构性同步补丁 (终极 hover 闪 根治): canon变化 只改5按钮 display
    //
    //   旧版: expected.length vs existingBtns 长度不同 → 清空 actionsWrap 重建 → 按钮 DOM 销毁 (致命!)
    //   新版: renderDownloadList 永远写 5 个按钮壳 (pause/resume/cancel/open/retry) + data-role
    //         canon 变化 → 只改 role 对应按钮的 style.display = '' or 'none'
    //         → 0 DOM 重建, 0 replaceChild, 0 重排, hover 中鼠标永远不丢节点!
    // ================================================================
    if (item.dataset.canonState !== canon) {
        item.dataset.canonState = canon;
        // 1) name + url (首次/更新统一同步)
        const nameSpan = item.querySelector('.download-item-name');
        if (nameSpan && nameSpan.textContent !== (dl.name || '下载任务')) nameSpan.textContent = dl.name || '下载任务';
        const urlDiv = item.querySelector('.download-item-url');
        if (urlDiv) {
            const wantUrl = dl.url || '无链接信息';
            if (urlDiv.textContent !== wantUrl) urlDiv.textContent = wantUrl;
            if (dl.url && urlDiv.getAttribute('title') !== dl.url) urlDiv.setAttribute('title', dl.url);
        }
        // 2) 按钮: 按 canon 只改 display, 永远不 append/remove/replace 任何按钮节点!
        const actionsWrap = item.querySelector('.dl-actions');
        if (actionsWrap) {
            const visibleMap = {
                starting: { pause: true, cancel: true },
                running:  { pause: true, cancel: true },
                paused:   { resume: true, cancel: true },
                completed:{ open: true },
                extracting:{ open: true },
                failed:   { retry: true },
                canceled: { retry: true },
            };
            const roles = ['pause','resume','cancel','open','retry'];
            const showMap = visibleMap[canon] || visibleMap.running;
            for (let i = 0; i < roles.length; i++) {
                const r = roles[i];
                const b = actionsWrap.querySelector('.dl-action-btn[data-role="' + r + '"]');
                if (!b) continue;
                const want = showMap[r] ? '' : 'none';
                if (b.style.display !== want) b.style.display = want;
                // 同步 data-task-id (如果还没写: 兼容 老 render 壳残留)
                if ((b.dataset.taskId || '') !== idStr) b.dataset.taskId = idStr;
            }
        }
        // 3) failed: 错误文本壳填充 (有变化才写, 避免 layout)
        if (canon === 'failed') {
            const errEl = item.querySelector('.dl-error-text');
            const wantErr = dl.error || '未知错误';
            if (errEl && errEl.textContent !== wantErr) errEl.textContent = wantErr;
        } else {
            const errEl = item.querySelector('.dl-error-text');
            if (errEl && errEl.textContent) errEl.textContent = '';
        }
    } else {
        // canon 没变: 只在 空壳首次 补 name + url + 按钮 data-task-id 补齐 (有变化才写)
        const nameSpan = item.querySelector('.download-item-name');
        if (nameSpan && !nameSpan.textContent) nameSpan.textContent = dl.name || '下载任务';
        const urlDiv = item.querySelector('.download-item-url');
        if (urlDiv && !urlDiv.textContent) {
            urlDiv.textContent = dl.url || '无链接信息';
            if (dl.url) urlDiv.setAttribute('title', dl.url);
        }
        // 老壳首次加载到新系统: 如果按钮缺 data-role → 只补 data-task-id (display 由 render 时 style 已设置)
        const actionsWrap = item.querySelector('.dl-actions');
        if (actionsWrap) {
            const legacyBtn = actionsWrap.querySelector('.dl-action-btn');
            if (legacyBtn && !legacyBtn.dataset.role) {
                // 老壳 (3.x 早期 只有 2~3 个按钮且无 role) → 强制重建一次 (1 次之后就有 5 按钮壳了)
                try {
                    const rdl = document.getElementById('download-list');
                    if (rdl) { rdl._lastHtml = ''; rdl._lastStructFingerprint = ''; renderDownloadList(); }
                } catch(_){}
                return;
            }
            // 新壳: 若 data-task-id 还没补 (快速切换时)
            const roles = ['pause','resume','cancel','open','retry'];
            for (let i = 0; i < roles.length; i++) {
                const r = roles[i];
                const b = actionsWrap.querySelector('.dl-action-btn[data-role="' + r + '"]');
                if (b && (b.dataset.taskId || '') !== idStr) b.dataset.taskId = idStr;
            }
        }
    }

    // ------------------------------------------------------------------
    // ★ B2b+: 同步 Engine/BT 诊断壳 (render 里只放空壳, 这里补 textContent/display)
    // ------------------------------------------------------------------
    try {
        // Engine 标签 (SwiftFetch/IDM/BitTorrent)
        const engEl = item.querySelector('.dl-engine-tag');
        const urlLow = String(dl.url || '').toLowerCase();
        const dlType = String(dl.type || dl.link_type || '').toLowerCase();
        const isReallyBT = (dl.engine === 'bt')
            || dlType === 'torrent' || dlType === 'bt'
            || urlLow.startsWith('magnet:') || urlLow.endsWith('.torrent')
            || idStr.startsWith('bt-');
        if (engEl) {
            const engRaw = dl.engine;
            if (engRaw && (canon === 'starting' || canon === 'running' || canon === 'extracting' || canon === 'paused' || canon === 'failed')) {
                const labelMap = { 'idm':'IDM','speed':'SwiftFetch','aria2':'SwiftFetch','legacy':'内置','bt':'BitTorrent' };
                engEl.textContent = '引擎: ' + (labelMap[engRaw] ? labelMap[engRaw] : String(engRaw));
                engEl.style.display = 'inline-block';
            } else if (!engRaw && isReallyBT) {
                engEl.style.display = 'none';
            } else {
                engEl.style.display = 'none';
                engEl.textContent = '';
            }
        }
        const btTagEl = item.querySelector('.bt-tag');
        if (btTagEl) {
            if (isReallyBT) {
                btTagEl.style.display = 'inline-block';
                if (!btTagEl.textContent) btTagEl.textContent = 'BT';
            } else {
                btTagEl.style.display = 'none';
            }
        }
    } catch (_) {}


    // ------------------------------------------------------------------
    // R4 徽标: 前端独立停滞 ≥ 10s → 右上角红色徽标 "⚠ 停滞XX秒" (让用户区分"真卡住"vs"慢速下载")
    //   ≥30s 且已自动尝试恢复: 升级为橙黄徽标, 文案"重试中 Xs"
    // ------------------------------------------------------------------
    const stallBadge = item.querySelector('.dl-stall-badge');
    const fStall = Number(dl._front_stall_secs) || 0;
    // 兼容后端发过来的 phase_stall_secs (后端支持也直接合并 MAX(前端检测, 后端报告))
    const bStall = Number(dl.phase_stall_secs) || 0;
    const stallSecs = Math.max(fStall, bStall);
    if (stallBadge) {
        if (stallSecs >= 30 && (canon === 'running' || canon === 'starting')) {
            stallBadge.style.display = 'inline-block';
            stallBadge.textContent = `⚠ 停滞 ${stallSecs.toFixed(0)} 秒`;
            stallBadge.style.background = dl._autoRestartHinted ? '#e67e22' : '#c0392b';
        } else if (stallSecs >= 10 && (canon === 'running' || canon === 'starting')) {
            stallBadge.style.display = 'inline-block';
            stallBadge.textContent = `⚠ 停 ${stallSecs.toFixed(0)}s`;
            stallBadge.style.background = '#d35400';
        } else {
            stallBadge.style.display = 'none';
            stallBadge.textContent = '';
        }
    }

    // ------------------------------------------------------------------
    // 公共: 下载进度条永远不覆盖! 下载进度=downloaded/total 保留, 哪怕在解压中也展示 100% (下载完了)
    // ------------------------------------------------------------------
    const unknownSize = canon === 'running' && (!dl.total || dl.total === 0);
    let dlPct = (dl.progress || 0);
    if (canon === 'completed' || canon === 'extracting' || canon === 'failed' || canon === 'canceled') {
        dlPct = 100; // 这些状态下载部分都算走完
    }
    const dlPctText = unknownSize ? formatBytes(dl.downloaded) : (dlPct.toFixed(1) + '%');

    const dlBarFill = item.querySelector('.download-progress-fill');
    if (dlBarFill) {
        if (unknownSize) {
            // ★ 2026-10-06 修: 原来这里 width 写 100% —— 想表达"大小未知, 用流动条纹示意",
            //   但视觉上就是"进度条一上来就顶满", 用户以为进度错了。
            //   现在交给 CSS 的 .progress-indeterminate 画一条**窄带左右滑动**,
            //   一眼能看出"还在探测/连接", 不会被误读成 100%。
            dlBarFill.classList.add('progress-indeterminate');
            dlBarFill.style.width = '';
        } else {
            dlBarFill.classList.remove('progress-indeterminate');
            dlBarFill.style.width = Math.max(0, Math.min(100, dlPct)) + '%';
        }
    }

    // ------------------------------------------------------------------
    // public size-text (info 行左侧 downloaded/total 或解压字节)
    // ------------------------------------------------------------------
    const sizeSpan = item.querySelector('.dl-size-text');

    // ------------------------------------------------------------------
    // [Case A] 解压条显示逻辑 (三种状态):
    //   1) canon === 'extracting' → 红色解压中条 (动态进度)
    //   2) canon === 'completed' 且 _extract_success === true → 绿色解压条 100% (成功保留)
    //   3) canon === 'completed' 且 _extract_success === false → 红色解压条 100% (失败保留)
    //   4) 其他 → 隐藏解压条
    // ------------------------------------------------------------------
    const hasExtractHistory = (dl._extract_success === true || dl._extract_success === false);
    const showExtractBar = (canon === 'extracting') || (canon === 'completed' && hasExtractHistory);
    if (showExtractBar) {
        const extractPct = Number(dl._extract_percent) || 0;
        const extractCurrent = typeof dl._extract_current_file === 'string' ? dl._extract_current_file : '准备解压...';
        const extractBytesDone = Number(dl._extract_bytes_extracted) || 0;
        const extractBytesTotal = Number(dl._extract_bytes_total) || 0;

        // 独立解压条显示出来 (控制 wrap 整体显示)
        const extBar = item.querySelector('.extract-progress-wrap');
        if (extBar && extBar.style.display === 'none') {
            extBar.style.display = 'block';
        }
        const extFill = item.querySelector('.extract-progress-fill');
        if (extFill) {
            // ★ 颜色 (2026-09-13 修复): extracting=默认橙色(不加类); 成功=绿色; 失败=深红
            //   旧 bug: extracting 加了 extract-failed → 显示深红色, 和失败混淆
            extFill.classList.remove('extract-success', 'extract-failed');
            if (canon === 'extracting') {
                // 解压中: 不加任何类, 使用 CSS 默认橙红色
            } else if (dl._extract_success === true) {
                extFill.classList.add('extract-success'); // 绿色
            } else {
                extFill.classList.add('extract-failed'); // 深红色
            }
            extFill.style.width = `${Math.max(0, Math.min(100, extractPct))}%`;
        }
        const extInfo = item.querySelector('.extract-info');
        if (extInfo) {
            const shortFile = extractCurrent.length > 55 ? ('...' + extractCurrent.slice(-53)) : extractCurrent;
            const bytesPart = extractBytesTotal > 0
                ? `${formatBytes(extractBytesDone)} / ${formatBytes(extractBytesTotal)}`
                : '';
            extInfo.textContent = `解压 ${extractPct.toFixed(1)}%${bytesPart ? ' · '+bytesPart : ''} · ${shortFile}`;
            // ★ 文字颜色同步 (2026-09-13 修复): extracting=默认橙色; 成功=绿; 失败=深红
            extInfo.classList.remove('extract-info-success', 'extract-info-failed');
            if (canon === 'extracting') {
                // 解压中: 不加类, 使用 CSS 默认橙色
            } else if (dl._extract_success === true) {
                extInfo.classList.add('extract-info-success');
            } else {
                extInfo.classList.add('extract-info-failed');
            }
        }

        // 解压中才覆盖右上角状态和 size 文本 (解压完成后保持"已完成 100%"显示)
        if (canon === 'extracting') {
            // 右上角状态: speed=文件名摘要, pct=解压百分比
            const speedSpan = item.querySelector('.dl-speed');
            if (speedSpan) {
                const sf = extractCurrent.length > 40 ? ('...' + extractCurrent.slice(-38)) : extractCurrent;
                speedSpan.textContent = sf;
            }
            const pctSpan = item.querySelector('.dl-pct');
            if (pctSpan) pctSpan.textContent = `解压 ${extractPct.toFixed(1)}%`;

            // info 行左: 解压字节
            if (sizeSpan) {
                if (extractBytesTotal > 0) {
                    sizeSpan.textContent = `解压中: ${formatBytes(extractBytesDone)} / ${formatBytes(extractBytesTotal)}`;
                } else {
                    sizeSpan.textContent = '解压中...';
                }
            }
        }
        if (canon === 'extracting') return;
    } else {
        // 非解压相关状态: 隐藏解压条
        const extBar = item.querySelector('.extract-progress-wrap');
        if (extBar && extBar.style.display !== 'none') {
            extBar.style.display = 'none';
        }
    }

    // ------------------------------------------------------------------
    // [Case BT] BitTorrent 任务: 独立蓝色条纹进度条 + peers/seeders 信息
    // ------------------------------------------------------------------
    const dlType = String(dl.type || dl.link_type || '').toLowerCase();
    const isBT = (dl.engine === 'bt')
              || dlType === 'torrent'
              || dlType === 'bt'
              || String(id).startsWith('bt-')
              || String(dl.url || '').toLowerCase().startsWith('magnet:')
              || String(dl.url || '').toLowerCase().endsWith('.torrent')
              || (String(dl.name || '').toLowerCase().endsWith('.torrent') && canon !== 'completed');

    if (isBT && canon !== 'extracting') {
        const btPct = Math.max(0, Math.min(100, Number(dl.progress_percent ?? dl.progress ?? 0)));
        const peers = Number(dl.bt_peers ?? 0);
        const seeders = Number(dl.bt_seeders ?? 0);
        const infoText = dl.bt_info_text || '';
        const btTotal = Number(dl.total ?? 0);
        const btDownloaded = Number(dl.downloaded ?? 0);

        // BT 独立进度条: 显示出来
        const btBar = item.querySelector('.bt-progress-bar');
        if (btBar && btBar.style.display === 'none') {
            btBar.style.display = 'block';
        }
        const btFill = item.querySelector('.bt-progress-fill');
        if (btFill) {
            // BT 完成前条纹动画, 完成后静态
            if (canon === 'completed' || btPct >= 100) {
                btFill.style.animationPlayState = 'paused';
            }
            btFill.style.width = `${btPct}%`;
        }
        const btInfoEl = item.querySelector('.bt-info');
        if (btInfoEl) {
            const speedBPS = Number(dl.speed_bps ?? 0);
            const speedStr = speedBPS > 0 ? formatSpeedBps(speedBPS) : '';
            const ps = `Peers:${peers} / Seeders:${seeders}`;
            const sizePart = btTotal > 0
                ? `${formatBytes(btDownloaded)} / ${formatBytes(btTotal)} (${btPct.toFixed(1)}%)`
                : (btDownloaded > 0 ? `${formatBytes(btDownloaded)}` : '');
            const parts = [];
            if (ps) parts.push(ps);
            if (infoText) parts.push(infoText);
            if (sizePart) parts.push(sizePart);
            if (speedStr) parts.push(speedStr);
            btInfoEl.textContent = parts.join('  ·  ');
        }
    } else {
        // 非 BT 任务: 隐藏 BT 进度条
        const btBar = item.querySelector('.bt-progress-bar');
        if (btBar && btBar.style.display !== 'none') {
            btBar.style.display = 'none';
        }
    }

    // ------------------------------------------------------------------
    // [Case B] 下载中/启动中/暂停/完成/失败/取消 → 普通下载 patch
    // ------------------------------------------------------------------
    // 右上角 dl-pct
    const pctSpan = item.querySelector('.dl-pct');
    if (pctSpan) pctSpan.textContent = dlPctText;

    // 右上角 dl-speed (速度+ETA 或诊断或状态文本)
    const rawSpeed = dl.speed || (dl.speed_bps ? formatSpeedBps(dl.speed_bps) : '');
    const speedText = (rawSpeed && !/^0(\.0+)?\s*(B|KB|MB|GB)\/s$/.test(rawSpeed)) ? rawSpeed : '';
    const etaText = getStableEta(dl) > 0 ? formatEta(getStableEta(dl)) : '';
    const subLabels = {
        'probe_head': 'GET 探测(大小)', 'probe_head_done': '探测完成', 'probe_get_range': 'GET 范围探测',
        'probe_failed_fallback_stream': '探测失败(转流式)', 'probe_timeout_fallback_stream': '探测超时(转流式)',
        'connect_start': '建立连接', 'stream_start': '进入流式下载', 'stream_wait_headers': '等待响应头',
        'stream_body': '流式读取', 'stalled_no_data': '数据停滞',
        'chunk_dynamic_start': '动态分片启动', 'chunk_static_start': '分片启动', 'chunk_downloading': '分片下载中',
        'init': '初始化',
    };
    const subLabel = subLabels[dl.subphase] || dl.subphase || '';
    const phaseStall = dl.phase_stall_secs ? ` · ${dl.phase_stall_secs}s` : '';
    const httpHint = dl.last_http_status ? ` · HTTP ${dl.last_http_status}` : '';
    const noBytesYet = !dl.downloaded || dl.downloaded === 0;
    // ★ 诊断提示 (不再只在 0 字节时生效! 之前 downloaded>0 永远显示"下载中..."导致卡顿时没任何提示)
    const diagHint = (subLabel || httpHint || phaseStall)
        ? `${subLabel || '下载中'}${httpHint}${phaseStall}`
        : '';
    const connectHint = noBytesYet ? (subLabel ? `${subLabel}${httpHint}${phaseStall}` : '连接中...') : '';
    // ★ 用户报「GX 资源一直连接中」：一个字节都没收到时，"连接中..."会一直挂着，
    //   看着像死机。这里按等待时长**升级提示**，最后给出可操作的原因，
    //   而不是永远只写三个字。（GX 链接 1 小时过期，多半就是这个原因）
    let stalledHint = '';
    if (noBytesYet && (canon === 'starting' || canon === 'running')) {
        const since = Number(dl._zeroSince) || 0;
        const waited = since ? Math.floor((Date.now() - since) / 1000) : 0;
        if (waited >= 45) {
            stalledHint = (dl.gxResourceId ? '连不上：下载链接可能已过期 —— 点「↻ 重试」会重新获取链接'
                                           : '连不上：服务器没响应，点「↻ 重试」再试一次')
                + `（已等 ${waited}s）`;
        } else if (waited >= 15) {
            stalledHint = `连接中…（已等 ${waited}s，正在建立连接）`;
        }
    }

    let speedLine = '';
    if (canon === 'starting' || canon === 'running') {
        if (speedText) {
            // 有速度 → 速度 + ETA (若有诊断附加其后, 提供上下文)
            const base = etaText ? `${speedText} · 预计 ${etaText}` : speedText;
            speedLine = diagHint ? `${base} (${diagHint})` : base;
        } else if (isBT && noBytesYet) {
            // ★ BT 专属提示: 区分"获取 Peer"与"已连 Peer 握手中", 不再显示通用"连接中"
            const peers = Number(dl.bt_peers ?? 0);
            const seeders = Number(dl.bt_seeders ?? 0);
            if (peers > 0) {
                speedLine = `BT 已连 ${peers} Peer (做种 ${seeders}), 等待数据...`;
            } else {
                speedLine = 'BT 正在从 Tracker 获取 Peer...';
            }
        } else if (diagHint) {
            // ★ 速度为0 但有诊断 (stalled / chunking / HTTP 码) → 直接显示诊断, 绝不吞成"下载中..."
            speedLine = diagHint;
        } else if (dl.downloaded > 0) {
            speedLine = '下载中...';
        } else {
            // ★ 卡在"连接中"时按等待时长升级提示（见上面 stalledHint）
            speedLine = stalledHint || connectHint || '连接中...';
        }
    } else if (canon === 'paused') {
        speedLine = '已暂停';
    } else if (canon === 'completed') {
        speedLine = '已完成';
    } else if (canon === 'failed') {
        speedLine = '下载失败';
    } else if (canon === 'canceled') {
        speedLine = '已取消';
    }
    const speedSpan = item.querySelector('.dl-speed');
    if (speedSpan) speedSpan.textContent = speedLine;

    // info 行左: downloaded / total
    if (sizeSpan) {
        sizeSpan.textContent = unknownSize
            ? `${formatBytes(dl.downloaded)}`
            : `${formatBytes(dl.downloaded)} / ${formatBytes(dl.total)}`;
    }

    // URL title 顺带
    const urlDiv = item.querySelector('.download-item-url');
    if (urlDiv && dl.url) {
        if (urlDiv.getAttribute('title') !== dl.url) urlDiv.setAttribute('title', dl.url);
    }
}

// ================================================================
// ★★★ 下载进度 DOM 批量写入调度器 (RequestAnimationFrame Batch)
// 根治 layout thrash: 同一帧的所有 DOM 写合并到下一次 vsync,
// N 个下载任务总共只触发 1 次 reflow + paint (原来是 N 次)
// ================================================================
// ★ 自测诊断统计变量 (全局)
window.__dlPatchedTotal = 0;
window.__dlPatchedTotalErrors = 0;
window.__dlLastProgressLog = { byId: {} };

/** 对所有下载项 【同步直写 DOM】(彻底根除 RAF 四保险多层死锁导致的"弹一下不动")
 *  说明: 过去多轮 patch 的 RAF-batch 方案, 只要任何一层(rAF/setTimeout/watchdog/flush return/finally)
 *        被 WebView2 的奇奇怪怪的事件循环/合成器挂起 跳过一次 → 下一次死锁
 *  改成最朴素最可靠的: 遍历 state.downloads → 同步 for 循环 → 逐个 patchDownloadItemIncrementally()
 *       不节流、不队列、不调度 → 保证只要调用了 patchAll → 下次 tick 进度条一定有值
 *  性能: 下载任务数 < 1000 (用户日常 < 50) → 同步 DOM 写最多 1~2ms, 0 layout thrash 风险;
 *       且只有 progress 才会 style.width = 整数%, 浏览器合并 paint
 */
function patchAllDownloadsIncrementally() {
    if (!state || !state.downloads || state.downloads.size === 0) return;
    // 同步循环 (不进 RAF, 不进 setTimeout, 不进 queue) → 最可靠
    for (const [id, dl] of state.downloads.entries()) {
        if (!dl) continue;
        try {
            patchDownloadItemIncrementally(dl, id);
            window.__dlPatchedTotal++;
        } catch (err) {
            window.__dlPatchedTotalErrors++;
            console.error('[patchAll] 单任务失败 id=' + String(id || '?'), err);
        }
    }
    // 自测断言: 每次 patchAll 结尾, 保证"进度>0.1"的任务 DOM 进度条 width !== '0%' / ''
    //           不通过 → 强制 renderDownloadList() 重建 (一次兜底) + Toast (仅 1 次 per id)
    try {
        const list = document.getElementById('download-list');
        if (!list) return;
        for (const [sid, sdl] of state.downloads.entries()) {
            if (!sdl) continue;
            const canon = normalizeDlState(sdl.state || 'running');
            const unknown = (canon === 'running' && (!sdl.total || sdl.total === 0));
            const pct = Number(sdl.progress) || 0;
            if (!unknown && pct > 0.1) {
                const csid = CSS.escape ? CSS.escape(String(sid)) : String(sid);
                const one = document.querySelector('.download-item[data-task-id="' + csid + '"]');
                if (!one) {
                    // DOM 没条目 → 立即重建 (防死)
                    window.__dlMissingDomCnt = (window.__dlMissingDomCnt || 0) + 1;
                    try { renderDownloadList(); } catch(_){}
                    return;
                }
                const fill = one.querySelector('.download-progress-fill');
                if (fill) {
                    const w = String(fill.style.width || '');
                    if (!w || w === '0%' || w === '0') {
                        // ★ 真的 patch 没写上去 → 直接写!
                        fill.classList.remove('progress-indeterminate');
                        fill.style.width = Math.max(0, Math.min(100, pct)) + '%';
                    }
                } else {
                    try { renderDownloadList(); } catch(_){}
                    return;
                }
            }
        }
    } catch (_) {}
}

// (向后兼容) 旧调用点仍会调 _scheduleDlPatchBatch / _flushDlPatchQueue → 现在直接走同步
function _scheduleDlPatchBatch(items) { try { patchAllDownloadsIncrementally(); } catch(_){} }
function _flushDlPatchQueue() { try { patchAllDownloadsIncrementally(); } catch(_){} }

window.cancelDownload = async function(taskId) {
    try {
        await invoke('cancel_download', { taskId: taskId });
    } catch (e) {
        console.error('取消失败:', e);
    }
};

window.pauseDownload = async function(taskId) {
    try {
        await invoke('pause_download', { taskId: taskId });
        const dl = state.downloads.get(taskId);
        if (dl) { dl.state = 'paused'; renderDownloadList(); }
    } catch (e) {
        console.error('暂停失败:', e);
    }
};

window.resumeDownload = async function(taskId) {
    const dl = state.downloads.get(taskId);
    try {
        await invoke('resume_download', { taskId: taskId });
        if (dl) { dl.state = 'running'; renderDownloadList(); }
    } catch (e) {
        // ★ 重启后恢复出来的"已暂停"任务，后端内存里根本没有它（任务表是纯内存的），
        //   resume_download 必然报"任务不存在"，而 retry_download 对不存在的任务
        //   是**静默空转**（只 Ok(())，什么都不做）—— 所以这里必须重新 start_download。
        //   引擎会读磁盘上的 `.swiftfetch-resume`，从断点接着下（不是从头来）。
        const msg = String(e && e.message ? e.message : e);
        frontLog('RESUME_FAIL', 'taskId=' + taskId + ' err=' + msg.slice(0, 120));
        if (dl && dl.url) {
            try {
                // ★ GX 的预签名链接 1 小时就过期 —— 恢复一个很久以前的暂停任务时
                //   旧链接必然 403（表现就是"一直连接中"）。这里重新取一条再续传。
                dl.url = await refreshGxUrlIfExpired(dl);
                const realId = await invoke('start_download', {
                    taskId: taskId,
                    url: dl.url,
                    filePath: dl.filePath || '',
                    headers: null,
                });
                if (realId && realId !== taskId) {
                    state.downloads.delete(taskId);
                    dl.taskId = realId;
                    state.downloads.set(realId, dl);
                }
                dl.state = 'running';
                dl._finished = false;
                renderDownloadList();
                showToast('已从断点继续下载');
                return;
            } catch (e2) {
                frontLog('RESUME_RESTART_FAIL', String(e2 && e2.message ? e2.message : e2).slice(0, 160));
                showToast('继续下载失败：' + String(e2 && e2.message ? e2.message : e2), 'error', 6000);
                return;
            }
        }
        showToast('继续下载失败：' + msg, 'error', 6000);
    }
};

// 打开已下载的文件 (直接用系统默认程序打开)
// ★ 优先级 (2026-09-13 重写):
//   1) dl.extractedPath (解压后的游戏目录) —— 最符合用户预期: 点"打开"看到游戏文件
//   2) dl.filePath 去掉压缩包扩展名后的目录 (可能已被解压过但 extractedPath 没落盘)
//   3) dl.filePath (原压缩包, BT 任务本身是目录)
//   后端 open_file 已具备目录/文件/不存在 3 种路径的智能处理, 这里选最合理的路径传过去
window.openDownloadedFile = async function(taskId) {
    const dl = state.downloads.get(taskId);
    if (!dl) {
        frontLog('OPEN_FAIL', 'taskId=' + taskId + ' reason=任务不存在');
        return;
    }

    const fp = String(dl.filePath || '').replace(/\//g, '\\');
    const ep = dl.extractedPath ? String(dl.extractedPath).replace(/\//g, '\\') : '';

    // 候选路径列表 (按优先级排序), 逐个检查存在性, 第一个存在的就是目标
    const candidates = [];

    // 1) 解压输出目录 (最优先)
    if (ep) candidates.push({ path: ep, reason: 'extractedPath' });

    // 2) filePath 去掉压缩包扩展名 (byrut BT 或已手动解压的场景)
    if (fp) {
        const ARCHIVE_EXTS = ['.zip', '.rar', '.7z', '.tar', '.gz', '.bz2', '.torrent'];
        const fpLow = fp.toLowerCase();
        for (const ext of ARCHIVE_EXTS) {
            if (fpLow.endsWith(ext)) {
                const stripped = fp.slice(0, -ext.length);
                candidates.push({ path: stripped, reason: 'stripExt=' + ext });
                break;
            }
        }
    }

    // 3) 原 filePath
    if (fp) candidates.push({ path: fp, reason: 'filePath' });

    // 4) filePath 的父目录 (兜底)
    if (fp) {
        const lastSlash = fp.lastIndexOf('\\');
        const parent = lastSlash > 0 ? fp.substring(0, lastSlash) : '';
        if (parent) candidates.push({ path: parent, reason: 'parentDir' });
    }

    // 5) extractDir (兜底)
    if (dl.extractDir) candidates.push({ path: String(dl.extractDir).replace(/\//g, '\\'), reason: 'extractDir' });

    // 逐个用后端 check_bt_disk_progress (轻量 exists 检查) 探测, 找到第一个存在的路径
    let targetPath = '';
    let targetReason = '';
    for (const c of candidates) {
        if (!c.path || c.path.length < 3) continue; // 避免相对路径/空串
        try {
            const check = await invoke('check_bt_disk_progress', { dir: c.path });
            if (check && (check.exists || (check.file_count !== undefined && check.file_count > 0) || (check.disk_bytes !== undefined && check.disk_bytes > 0))) {
                targetPath = c.path;
                targetReason = c.reason;
                break;
            }
        } catch (e) { /* 静默, 试下一个 */ }
    }

    // 如果全部检查失败, 退回到第一个非空候选 (让后端 open_file 自己兜底)
    if (!targetPath) {
        const fallback = candidates.find(c => c.path && c.path.length >= 3);
        if (fallback) {
            targetPath = fallback.path;
            targetReason = fallback.reason + '(fallback)';
        }
    }

    frontLog('OPEN_REQ', 'taskId=' + taskId + ' ep=' + (ep||'(无)') + ' fp=' + fp + ' -> targetPath=' + targetPath + ' reason=' + targetReason);
    if (!targetPath) {
        try { showToast('无法打开: 该任务没有可定位的文件路径', 'error', 3000); } catch(_) {}
        return;
    }
    try {
        await invoke('open_file', { path: targetPath });
        frontLog('OPEN_OK', 'path=' + targetPath);
    } catch (e) {
        frontLog('OPEN_FAIL_ERR', 'path=' + targetPath + ' err=' + e);
        // open_file 失败, 回退到 open_folder 打开父目录
        try {
            const fpNorm = String(targetPath).replace(/\//g, '\\');
            const lastSlash = fpNorm.lastIndexOf('\\');
            const dir = lastSlash > 0 ? fpNorm.substring(0, lastSlash) : fpNorm;
            await invoke('open_folder', { path: dir });
            frontLog('OPEN_FALLBACK_DIR', 'dir=' + dir);
        } catch (e2) {
            try { showToast('打开失败: ' + (e2 && e2.message ? e2.message : String(e2)), 'error', 3000); } catch(_) {}
        }
    }
};

// ★ 删除已下载的文件 (2026-10-03)
//   解压失败时源压缩包不会自动删除 (delete_archive 只在成功时生效), 软件里原本
//   也没有删除入口 —— 用户只能去资源管理器删。这里补上, 后端 delete_path 带重试
//   (引擎/解压子进程刚退出时句柄可能还没释放)。
window.deleteDownloadFile = async function(taskId) {
    const dl = state.downloads.get(taskId);
    if (!dl) return;
    const p = dl.filePath || '';
    if (!p) { alert('找不到该任务的文件路径'); return; }
    if (!confirm('确定删除这个文件吗？\n\n' + p + '\n\n删除后无法恢复。')) return;
    try {
        await invoke('delete_path', { path: p });
        frontLog('DL_DELETE', 'task=' + taskId + ' file=' + p.slice(0, 120));
        // 一并清掉断点续传记录, 否则下次下载会拿它把新文件标成"已完成"
        try { await invoke('delete_path', { path: p + '.swiftfetch-resume' }); } catch (_) {}
        state.downloads.delete(taskId);
        if (typeof renderDownloadList === 'function') renderDownloadList(true);
    } catch (e) {
        alert('删除失败: ' + (e && e.message ? e.message : String(e)));
    }
};

// 重试下载 (重新发起同一 URL 的下载)
window.retryDownload = async function(taskId) {
    const dl = state.downloads.get(taskId);
    if (!dl) return;
    // ★★ GX 资源（2026-10-08 修）：以前这里**直接放弃**——复制链接弹窗让用户
    //   "去网站手动下载"。而 GX 的预签名链接 1 小时过期，于是"重试"永远重试不了，
    //   用户看到的就是「GX 资源一直连接中」还退不出来。
    //   现在：能拿到资源 id 就**重新取一条新链接**，照常走内置下载器重试。
    if (dl.gxResourceId) {
        try {
            const fresh = await refreshGxUrlIfExpired({ ...dl, gxResourceId: dl.gxResourceId });
            if (fresh) dl.url = fresh;
        } catch (e) {}
    } else if (dl.url && dl.url.includes('galgamex.com')) {
        // 没有资源 id（老任务/手工粘贴的链接）→ 保留原来的"复制链接去网站"兜底
        try {
            await invoke('copy_to_clipboard', { text: dl.url });
            alert(`这条 GX 任务的资源 id 没记录，无法自动换链\n\n下载链接已复制到剪贴板\n文件: ${dl.name}\n\n请打开 galgamex.net 手动下载`);
        } catch (e) {
            alert(`下载链接:\n${dl.url}`);
        }
        return;
    }
    // 重新生成 task_id
    const newTaskId = `dl_${Date.now()}_${Math.random().toString(36).slice(2, 10)}`;
    // BY/KO 走内置下载器
    let headers = [];
    if (dl.url.includes('byrutgame.org')) {
        headers = [['Referer', 'https://byrutgame.org/']];
    } else if (dl.url.includes('playzip.com')) {
        headers = [['Referer', 'https://playzip.com/']];
    } else if (dl.url.includes('galgamex.com')) {
        headers = [['Referer', 'https://galgamex.net/']];
    }
    try {
        const realTaskId = await invoke('start_download', {
            taskId: newTaskId,
            url: dl.url,
            filePath: dl.filePath,
            headers: headers,
        });
        // ★ BT 任务真实 key 是 bt-xxx: 用 realTaskId 作为 Map key 才能收到进度/操作按钮
        const mapKey = realTaskId || newTaskId;
        state.downloads.delete(taskId);
        state.downloads.set(mapKey, {
            name: dl.name,
            url: dl.url,
            filePath: dl.filePath,
            progress: 0,
            speed: '',
            state: 'running',
            total: 0,
            downloaded: 0,
            engine: String(mapKey).startsWith('bt-') ? 'bt' : (dl.engine || undefined),
            autoExtract: dl.autoExtract,
            extractDir: dl.extractDir,
            extractCode: dl.extractCode,
            deleteArchive: dl.deleteArchive,
            createFolder: dl.createFolder,
            createShortcut: dl.createShortcut,
            // ★ 重试后仍要保留来源信息，否则下次重试又换不了链、快捷方式也不弹窗了
            source: dl.source || '',
            gxResourceId: dl.gxResourceId || null,
            gxIndex: (dl.gxIndex == null ? null : dl.gxIndex),
            gameName: dl.gameName || '',
            downloadRoot: dl.downloadRoot || '',
            cover: dl.cover || '',
            taskId: mapKey,
        });
        renderDownloadList();
    } catch (e) {
        alert(`重试失败: ${e}`);
    }
};

function formatBytes(bytes) {
    if (!bytes || bytes === 0) return '0 B';
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(2)} MB`;
    return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function formatEta(seconds) {
    // ================================================================
    // ★ 稳定 ETA: <1s → "即将完成"; <1min 进一位到秒; ≥1min / ≥1h 进一位到分钟/小时 (防止 1秒2秒来回跳)
    // ================================================================
    if (!seconds || seconds <= 0) return '';
    if (seconds < 1) return '即将完成';
    if (seconds < 60) return `${Math.ceil(seconds)}秒`;
    if (seconds < 3600) {
        const m = Math.floor(seconds / 60);
        const s = Math.ceil(seconds % 60);
        return `${m}分${s > 0 ? s + '秒' : ''}`;
    }
    const h = Math.floor(seconds / 3600);
    const m = Math.ceil((seconds % 3600) / 60);
    return `${h}时${m > 0 ? m + '分' : ''}`;
}

// 取下载项的稳定 ETA (优先用 慢速 EMA: dl._eta_ema, 没初始化才退回 dl.eta_secs 原始值)
function getStableEta(dl) {
    if (!dl) return 0;
    const ema = Number(dl._eta_ema) || 0;
    const raw = Number(dl.eta_secs) || 0;
    if (ema > 0) return ema;
    return raw;
}

function formatSpeedBps(bps) {
    if (!bps || bps <= 0) return '';
    if (bps < 1024) return `${bps} B/s`;
    if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
    if (bps < 1024 * 1024 * 1024) return `${(bps / 1024 / 1024).toFixed(2)} MB/s`;
    return `${(bps / 1024 / 1024 / 1024).toFixed(2)} GB/s`;
}

// ============================================================
// 下载进度事件监听 (旧 updateDownloadListDOM 机制 已废弃!)
// ============================================================
// ★ 旧版 scheduleProgressRender → updateDownloadListDOM 是 2025 年早期遗留机制, 直接写 DOM 但:
//   ① 与 2026 版 patchDownloadItemIncrementally 双写冲突 → 产生 progress/width 跳变 (进度弹一下就不动)
//   ② 没有 Engine/BT 诊断/stall 徽标 同步 → 信息不完整
// ★ 新版: 统一通过 patchAllDownloadsIncrementally (RAF batch + 完整同步)
// ★★★ 终极根治 "弹一下就不动": scheduleProgressRender 彻底禁止 RAF / pending 去重锁!
//   根因: WebView2 合成器线程偶发挂起(首次加载/切页/GPU调度冲突) → RAF 回调永不执行
//        → progressRenderPending 永久停在 true → 下一行 if (pending) return 短路
//        → 所有 download-progress 事件 永远不调 patchAll → 用户视觉: "刚启动动一下就卡死"
//   修复 (最朴素最可靠): 同步直调 patchAllDownloadsIncrementally, 零节流零调度零队列.
//        下载任务数<1000 → patchAll 总耗时 <2ms, 无layout thrash, 合并paint没问题.
//   updateDownloadListDOM 兼容: 同样同步直调.
function scheduleProgressRender() {
    try { patchAllDownloadsIncrementally(); } catch (e) {
        console.error('[scheduleProgressRender] patchAll fail:', e);
        // 一级兜底: 30ms 后再试 1 次 (不是 RAF, 用 setTimeout 即使合成器挂起也能跑)
        setTimeout(() => { try { patchAllDownloadsIncrementally(); } catch(_){} }, 30);
    }
}

// 兼容旧调用: 任何第三方/事件仍调 updateDownloadListDOM → 代理新版 patchAll (避免双写跳变)
function updateDownloadListDOM() {
    try { patchAllDownloadsIncrementally(); } catch (e) {}
}

// 下载完成处理 (可被事件监听和轮询共同调用)
// 把事件里的任务登记进下载列表。
// ★ 模块内下载 (修改器 / MC 模组) 也走主下载引擎, 前端这边并不是
//   "自己发起的", 所以 Map 里没有条目 —— 必须由事件兜底创建, 否则整条下载
//   在「下载」页上根本不出现。download-started / download-progress 都调它。
function ensureDownloadEntry(p) {
    let dl = state.downloads.get(p.task_id);
    if (dl) return dl;
    dl = {
        name: p.name || p.filename || '下载中',
        url: p.url || '',
        filePath: p.file_path || '',
        progress: p.progress_percent || 0,
        speed: '',
        state: p.state || 'starting',
        total: p.total || 0,
        downloaded: p.downloaded || 0,
        autoExtract: false,   // 模块内下载不自动解压
        extractDir: '',
        extractCode: '',
        createFolder: false,
        deleteArchive: false,
        _finished: false,
        engine: p.engine || 'speed',
    };
    state.downloads.set(p.task_id, dl);
    return dl;
}

async function onDownloadFinished(event) {
    const p = event.payload;
    const dl = state.downloads.get(p.task_id);
    if (!dl) return;
    if (dl._finished) return;
    dl._finished = true;

    // ★ 修复: 后端 download-finished 事件携带真实 file_path (byrut BT 已去扩展名),
    //   前端必须用它更新 dl.filePath, 否则"打开"按钮用的还是带 .zip 扩展名的旧路径 → 路径不存在 → 打开父目录
    if (p.file_path && p.file_path !== dl.filePath) {
        frontLog('DL_FIN_PATH_FIX', 'task=' + p.task_id + ' old filePath=' + (dl.filePath||'') + ' -> new=' + p.file_path);
        dl.filePath = p.file_path;
    }
    // 极快的下载可能一次进度事件都没赶上 → 名字还是占位的"下载中", 这里补上
    if (p.name && (!dl.name || dl.name === '下载中')) dl.name = p.name;

    if (p.state === 'completed') {
        dl.progress = 100;
        dl.state = 'completed';
        frontLog('DL_FIN_OK', `task=${p.task_id} name=${(dl.name||'').slice(0,40)} filePath=${dl.filePath||''}`);
        renderDownloadList();

        // ★ BT 任务不依赖那个"下载后自动解压"勾选框 —— 种子下下来的是不是压缩包,
        //   程序扫一下就知道, 不该让用户先去勾一个框。原来整段包在 dl.autoExtract 里,
        //   而 BT 任务的勾选框是关的 → 解压和快捷方式一次都没跑过。
        const _btId = String(p.task_id || dl.taskId || '');
        const _btUrl = String(dl.url || '').toLowerCase();
        const _looksBT = (dl.engine === 'bt') || _btId.startsWith('bt-')
                      || _btUrl.startsWith('magnet:') || _btUrl.endsWith('.torrent');
        if (dl.autoExtract || _looksBT) {
            // BT 任务: autoExtract = "自动下载BT" 语义, 下载 = 内容下载本身, 无需解压
            const urlLow = String(dl.url || '').toLowerCase();
            const nameLow = String(dl.name || '').toLowerCase();
            const taskId = String(p.task_id || dl.taskId || '');
            const dlType = String(dl.type || dl.link_type || '').toLowerCase();
            const isBT = (dl.engine === 'bt')
                      || dlType === 'torrent'
                      || dlType === 'bt'
                      || taskId.startsWith('bt-')
                      || urlLow.startsWith('magnet:')
                      || urlLow.endsWith('.torrent')
                      || nameLow.endsWith('.torrent');
            if (!isBT) {
                try { await handleAutoExtract(dl); } catch (e) { console.warn('handleAutoExtract 抛错:', e); }
                return; // handleAutoExtract 内部最后会 renderDownloadList()
            }
            // ★ 新增 (2026-10-06): BT 下载完如果落地的是一个**压缩包**, 先自动解压。
            //   以前这里直接跳过解压 (假设"BT 内容本身就是游戏目录"), 结果种子里的
            //   zip/rar/7z 就永远躺在目录里没人管, 更不会有快捷方式。
            //   解压成功后的桌面快捷方式由 handleAutoExtract 自己创建 (那边已有逻辑)。
            let btArchive = '';
            try {
                const r = await invoke('list_archives', { dir: dl.filePath });
                const arr = (r && r.archives) || [];
                // 只有一个压缩包才解 (多分卷/多个包的情况交给人处理, 免得解错)
                if (arr.length === 1) btArchive = arr[0];
                frontLog('BT_ARCHIVE_SCAN', 'task=' + p.task_id + ' found=' + arr.length + ' pick=' + (btArchive || '(无)'));
            } catch (e) {
                frontLog('BT_ARCHIVE_SCAN_FAIL', 'task=' + p.task_id + ' err=' + String(e).slice(0, 80));
            }
            if (btArchive) {
                dl.filePath = btArchive;   // 让 handleAutoExtract 按压缩包走
                dl.extractedPath = '';
                // 未显式关掉就建快捷方式 (BT 任务的这个开关常常压根没被设过)
                if (dl.createShortcut !== false) dl.createShortcut = true;
                try { await handleAutoExtract(dl); } catch (e) { console.warn('handleAutoExtract 抛错:', e); }
                return;
            }

            // 没找到压缩包 → 直接给 BT 输出目录/内容建快捷方式
            //   (BT 任务通常没设过这个开关, 未显式关掉就当要)
            if (dl.createShortcut !== false && dl.filePath) {
                const gameName = vxShortcutName(dl, dl.filePath);
                try {
                    await invoke('create_desktop_shortcut', {
                        targetPath: dl.filePath,
                        shortcutName: gameName,
                    });
                    frontLog('SC_BT_OK', 'task=' + p.task_id + ' name=' + gameName + ' -> BT 桌面快捷方式已创建');
                } catch (e) {
                    frontLog('SC_BT_FAIL', 'task=' + p.task_id + ' name=' + gameName + ' err=' + e);
                }
            }
        }
        renderDownloadList();
    } else {
        // ★★★ 关键修复: 后端可能把"超量写入"(downloaded > total) 误报为 failed
        //   实际文件已完整下载到磁盘 (验证: 文件大小 == total)
        //   如果 downloaded >= total - 64KB → 文件已完整, 强制走完成流程 (触发自动解压)
        const dlTotal = Number(dl.total) || 0;
        const dlDownloaded = Number(dl.downloaded) || 0;
        if (dlTotal > 0 && dlDownloaded >= dlTotal - 65536) {
            // 文件已完整下载 (超量或接近 total) → 当完成处理
            dl.progress = 100;
            dl.state = 'completed';
            dl._overcounted_as_complete = true; // 标记原因
            frontLog('DL_FIN_FAIL_RESCUED', 'task=' + p.task_id + ' error=' + (dl.error||'') + ' downloaded=' + dlDownloaded + ' total=' + dlTotal + ' -> rescued as completed (overcounted)');
            renderDownloadList();
            // 走完成流程 (触发自动解压)
            if (dl.autoExtract) {
                const urlLow = String(dl.url || '').toLowerCase();
                const nameLow = String(dl.name || '').toLowerCase();
                const taskId = String(p.task_id || dl.taskId || '');
                const dlType = String(dl.type || dl.link_type || '').toLowerCase();
                const isBT = (dl.engine === 'bt')
                          || dlType === 'torrent' || dlType === 'bt'
                          || taskId.startsWith('bt-')
                          || urlLow.startsWith('magnet:')
                          || urlLow.endsWith('.torrent')
                          || nameLow.endsWith('.torrent');
                if (!isBT) {
                    try { await handleAutoExtract(dl); } catch (e) { console.warn('handleAutoExtract (rescued) 抛错:', e); }
                    return;
                }
            }
            renderDownloadList();
            return;
        }
        dl.state = 'failed';
        dl.error = p.error || '下载失败';
        // ★ 记录失败原因到日志 (不再用阻塞式 alert, 避免卡死整个前端事件循环)
        frontLog('DL_FIN_FAIL', 'task=' + p.task_id + ' error=' + (dl.error||'') + ' url=' + (dl.url||'').slice(0,80) + ' downloaded=' + dlDownloaded + '/' + dlTotal);
        console.error('[onDownloadFinished] 下载失败:', dl.error, 'task=', p.task_id, 'downloaded=', dlDownloaded, 'total=', dlTotal);
        renderDownloadList();
    }
}

// 下载事件监听 (必须在 Tauri 就绪后注册, 否则回调不生效)
async function setupDownloadListeners() {
    frontLog('SETUP_LISTEN', 'setupDownloadListeners() entered');
    // 下载进度事件 (实时更新, 主要靠轮询兜底)
    const unlistenPromise = listen('download-progress', (event) => {
        const p = event.payload;
        window._dbgEvtCnt = (window._dbgEvtCnt || 0) + 1;
        // ★ 前10次 + 每20次写一次日志, 避免刷屏 (CDP连不上也能100%确认有没有收到事件)
        if (window._dbgEvtCnt <= 10 || window._dbgEvtCnt % 20 === 0) {
            frontLog('EVT_DLPROG', '#evt=' + window._dbgEvtCnt +
                ' id=' + (p.task_id || '') +
                ' state=' + (p.state || '') +
                ' dl=' + (p.downloaded || 0) +
                ' speed=' + (p.speed_formatted || '') +
                ' sub=' + (p.subphase || ''));
        }
        // 调试: 每 20 次事件 (~4秒) 打印一次收到的 payload 与更新后的 dl, 便于定位前后端数据通道
        if (window._dbgEvtCnt % 20 === 0) {
            console.log('[evt] download-progress 原始payload:', JSON.parse(JSON.stringify(p)));
        }
        const had = state.downloads.has(p.task_id);
        let dl = ensureDownloadEntry(p);
        // 兜底: 如果 Map 里还没有这个任务 (事件比 start_download 同步 set 先触发),
        //       立即注册并强制重新渲染, 否则整段回调会直接退出, 永远不更新DOM
        if (!had) {
            console.warn('[evt] 任务不在 Map 中, 兜底创建并立即重渲染:', p.task_id, dl);
            renderDownloadList();
            // 继续往下跑增量逻辑
        }
        if (dl) {
            let changed = false;
            // download-started 的事件只有 task_id/state, 没有 name/url/file_path ——
            // 那条路建出来的条目名是占位"下载中"、链接为空, 等进度事件到了补上。
            // (模块内下载最明显: 列表里会一直挂着一个叫"下载中"的条目)
            if (p.name && (!dl.name || dl.name === '下载中')) { dl.name = p.name; changed = true; }
            if (p.url && !dl.url) { dl.url = p.url; changed = true; }
            if (p.file_path && !dl.filePath) { dl.filePath = p.file_path; changed = true; }
            if (p.total > 0 && p.total !== dl.total) {
                dl.total = p.total;
                changed = true;
            }
            if (p.downloaded !== undefined && p.downloaded !== dl.downloaded) {
                dl.downloaded = p.downloaded;
                // ★ 记录"从什么时候开始一个字节都没收到"——用于把"连接中..."升级成
                //   "链接可能已过期，点重试"（用户报 GX 一直连接中）
                if (!p.downloaded) {
                    if (!dl._zeroSince) dl._zeroSince = Date.now();
                } else {
                    dl._zeroSince = 0;
                }
                changed = true;
            }
            if (p.active_connections !== undefined) {
                dl.active_connections = p.active_connections;
            }
            if (p.state && p.state !== dl.state) {
                // 状态迁移: "downloading"/"connecting" -> "running"; "starting" 有数据时 -> "running"
                if (p.state === 'downloading' || p.state === 'connecting') {
                    dl.state = 'running';
                } else if (p.state === 'starting' && dl.downloaded > 0) {
                    dl.state = 'running';
                } else {
                    dl.state = p.state;
                }
                changed = true;
            }
            if (p.progress_percent !== undefined && typeof p.progress_percent === 'number' && isFinite(p.progress_percent)) {
                // ★★★ 统一浮点精度 (0.1%), 去 Math.floor → 避免 dl.progress 在整数/浮点间跳变
                // 进度百分比必须单调递增: 防止倒退导致进度条一跳一跳 (35%->33%)
                // 容差 0.001% 仅用于倒退保护, 不设前进门槛 (否则小增量永远卡住不更新)
                const pctIn = Math.max(0, Math.min(100, p.progress_percent));
                // ★ 与 poll 统一浮点容差: 0.0001% (原0.001%), 任何微小增量都能显示 (否则事件里永远卡整数边界)
                if (pctIn >= dl.progress - 0.0001) {
                    dl.progress = Math.max(dl.progress, Math.min(100, pctIn));
                    changed = true;
                } else if (pctIn >= 100 && dl.progress < 100) {
                    dl.progress = 100;
                    changed = true;
                }
            }
            // 速度: 双 EMA 平滑 (后端 α=0.75 + 前端 α=0.8), 仅差值≥1KB/s 才重绘字符串
            // 仅当 speed_bps 是有效数字时走 EMA 平滑; 否则保留文字型速度 (如"正在建立连接...")
            if (p.speed_bps !== undefined && typeof p.speed_bps === 'number' && isFinite(p.speed_bps) && p.speed_bps >= 0) {
                applySpeedEma(dl, p.speed_bps);
                changed = true;
            } else if (p.speed_formatted) {
                // 非数字速度 ("连接中"/"探测文件大小") 直接保留, 不做 EMA
                if (p.speed_formatted !== dl.speed) {
                    dl.speed = p.speed_formatted;
                    changed = true;
                }
            } else if (p.speed_bps === 0) {
                // 显式 0 字节/秒, EMA 平滑下来
                applySpeedEma(dl, 0);
                changed = true;
            }
            if (p.eta_secs !== undefined) {
                dl.eta_secs = p.eta_secs;
                changed = true;
            }
            // 诊断字段 (卡"连接中"时用于定位问题)
            // subphase: 只有后端传的非空值才覆盖, 防止轮询或空值把事件中的诊断信息冲掉
            if (p.subphase !== undefined && p.subphase !== null && String(p.subphase).length > 0) {
                if (p.subphase !== dl.subphase) { dl.subphase = p.subphase; changed = true; }
            }
            if (p.last_http_status !== undefined && p.last_http_status !== null) {
                if (p.last_http_status !== dl.last_http_status) { dl.last_http_status = p.last_http_status; changed = true; }
            }
            // 兼容后端字段: 既支持前端约定的 phase_stall_secs, 也支持后端实际发送的 phase_entered_at
            const incoming_stall = p.phase_stall_secs !== undefined && p.phase_stall_secs !== null
                ? p.phase_stall_secs
                : (p.phase_entered_at !== undefined && p.phase_entered_at !== null ? p.phase_entered_at : undefined);
            if (incoming_stall !== undefined) {
                const as_num = Number(incoming_stall) || 0;
                if (as_num !== dl.phase_stall_secs) { dl.phase_stall_secs = as_num; changed = true; }
            }
            // ===== 引擎 & BT 专属字段 =====
            if (p.engine !== undefined && p.engine !== dl.engine) {
                dl.engine = p.engine;
                changed = true;
            }
            if (p.bt_peers !== undefined) {
                const v = Number(p.bt_peers) || 0;
                if (v !== dl.bt_peers) { dl.bt_peers = v; changed = true; }
            }
            if (p.bt_seeders !== undefined) {
                const v = Number(p.bt_seeders) || 0;
                if (v !== dl.bt_seeders) { dl.bt_seeders = v; changed = true; }
            }
            if (p.bt_info_text !== undefined && p.bt_info_text !== dl.bt_info_text) {
                dl.bt_info_text = p.bt_info_text;
                changed = true;
            }
            // BT 精确百分比 (浮点, 之前 dl.progress 是 floor整数, 进度条视觉要精确)
            if (p.progress_percent !== undefined && typeof p.progress_percent === 'number' && isFinite(p.progress_percent)) {
                dl.progress_percent = p.progress_percent;
            }
            // speed_bps 保留原值 (BT 格式化 bt-info 时要用)
            if (p.speed_bps !== undefined && typeof p.speed_bps === 'number' && isFinite(p.speed_bps)) {
                dl.speed_bps = p.speed_bps;
            }
            // ★ 进度事件: 彻底禁止直接写 DOM! (旧版 updateDownloadListDOM 残留逻辑 已与 patchDownloadItemIncrementally 双写冲突 → 进度条"弹一下就不动")
            //   正确做法: 仅更新 state.downloads Map 内存, 调 scheduleProgressRender → RAF batch → patchAllDownloadsIncrementally 统一增量渲染
            //   如果元素还不存在 (changed=true), scheduleProgressRender 会走 render 兜底. 不再 querySelector 写 width/textContent!
            if (changed) {
                scheduleProgressRender();
            } else if (document.getElementById('download-list')) {
                scheduleProgressRender();
            }
        }
    });

    // 下载已启动 (SwiftFetch/SpeedEngine 触发)
    listen('download-started', (event) => {
        const p = event.payload;
        // ★ 没有条目也要建 —— 模块内下载 (修改器 / MC 模组) 走的是同一个
        //   引擎, 但前端不是发起方, 只更新已有条目的话它们永远不会出现在列表里。
        const dl = ensureDownloadEntry(p);
        dl.state = p.state === 'started' ? 'running' : (p.state || 'running');
        renderDownloadList();
    });

    // 下载启动失败
    listen('download-failed', (event) => {
        const p = event.payload;
        const dl = state.downloads.get(p.task_id);
        if (dl) {
            dl._finished = true;
            dl.state = 'failed';
            dl.error = p.error || '下载启动失败';
            // ★ 不再弹阻塞式 alert, 记录到日志即可 (用户通过下载卡红色状态看到失败)
            frontLog('DL_START_FAIL', `task=${p.task_id} error=${dl.error} url=${(dl.url||'').slice(0,80)}`);
            console.error('[download-failed] 下载启动失败:', dl.error, 'task=', p.task_id);
            renderDownloadList();
        }
    });

    // 下载完成事件: 自动解压 (由轮询在检测到完成时触发)
    listen('download-finished', onDownloadFinished);

    // (内置浏览器已移除 → 不再注册 browser-download-* 实时事件与诊断定时器)
}

// ============================================================
// 下载历史持久化 (localStorage) + SwiftFetch 双重恢复
// ============================================================
const DL_STORAGE_KEY = 'vortex_downloads_v2';

// ================================================================
// (内置浏览器已移除) 保留函数声明 + 统一调用 openExternal → 系统默认浏览器
//   - focusMainDownloadWindow: 只切 DOM 下载 tab
//   - hideBrowserWindowAfterAdopt: 空函数 (不存在浏览器窗口)
//   - adoptBrowserDownloadIntoMain: 直接 openBrowserWindow → 系统默认浏览器
//   - _pollBrowserHistorySync / startBrowserDownloadSync: 空函数
// ================================================================

window.__browserAdopted = new Set(); // 保留定义, 避免历史 clearBtn 访问时报 ReferenceError
window.__browserSyncTimer = null;

function _browserDlFingerprint(item) {
    const url = String(item.url || '').trim();
    const fn = String(item.filename || item.name || '').trim();
    return (fn ? fn + '::' : '') + url;
}

// 取默认下载目录 (与主下载模态保持一致)
async function _defaultDownloadDir() {
    try {
        const cfg = await invoke('get_config');
        if (cfg && cfg.download_dir) return cfg.download_dir;
    } catch (_) {}
    return 'D:\\game';
}

// 切到主下载页 tab (DOM 层, 主窗口本来就在前台, 无需 API 探测)
async function focusMainDownloadWindow() {
    _switchToDownloadsTab();
    return true;
}
// 无浏览器窗口可隐藏 → 空函数
async function hideBrowserWindowAfterAdopt() { return false; }

// 收编函数 (浏览器已移除 → 直接打开系统默认浏览器)
async function adoptBrowserDownloadIntoMain({ url, filename, total, status, source }) {
    if (!url) return null;
    try { openBrowserWindow(url); }
    catch (e) { alert('打开系统浏览器失败: ' + (e && e.message ? e.message : String(e))); }
    return null;
}

// 轮询浏览器历史 → 空函数
async function _pollBrowserHistorySync() { /* no-op: 内置浏览器已移除 */ }

// 启动同步 → 空函数 (幂等)
function startBrowserDownloadSync() { /* no-op: 内置浏览器已移除 */ }


// 保存下载列表到 localStorage。
//
// ★★ 以前**只存已完成/失败**，注释说"活动任务靠 SwiftFetch 恢复"——但 SwiftFetch 的
//    任务表是纯内存的，进程一退就没了。结果：下到一半关软件，那条任务**直接消失**
//    （既不在列表里，也没标失败，.swiftfetch-resume 还留在磁盘上没人认领）。
//    用户明确要求「上次没下载完的关掉软件应该只是暂停了而不是直接消失」。
//    → 活动任务也存，**状态落盘成 paused**（下次启动它确实不在跑），并保留
//      url/filePath/downloaded 这些续传需要的信息；续传数据由引擎的
//      `.swiftfetch-resume` 负责，点「继续」时会自动接着下。
function persistDownloadsToStorage() {
    try {
        const history = [];
        for (const [id, dl] of state.downloads) {
            const st = dl.state;
            const done = (st === 'completed' || st === 'failed' || st === 'canceled' || st === 'extracting');
            const active = (st === 'running' || st === 'starting' || st === 'paused' || st === 'downloading' || st === 'pending');
            if (!done && !active) continue;
            history.push({
                    id,
                    name: dl.name,
                    url: dl.url || '',
                    filePath: dl.filePath || '',
                    progress: dl.progress || 0,
                    // 活动任务 → paused：重启后它不在跑，界面要显示"已暂停 + 继续"
                    state: active ? 'paused' : st,
                    total: dl.total || 0,
                    downloaded: dl.downloaded || 0,
                    autoExtract: dl.autoExtract || false,
                    extractDir: dl.extractDir || '',
                    extractCode: dl.extractCode || '',
                    createFolder: dl.createFolder || false,
                    deleteArchive: dl.deleteArchive || false,
                    // ★ Bug 修复 (2026-09-12): 持久化 createShortcut, 否则重启后恢复任务丢失该字段
                    createShortcut: dl.createShortcut !== false,
                    // ★ 游戏名要一起存，否则恢复后重建快捷方式/文件夹会退回压缩包名
                    gameName: dl.gameName || '',
                    downloadRoot: dl.downloadRoot || '',
                    // ★ 解压出来的目录 + 封面：重启后「🔗 快捷方式」按钮还要用
                    extractedPath: dl.extractedPath || '',
                    cover: dl.cover || '',
                    engine: dl.engine || '',
                    error: dl.error || '',
                    // ★ 来源要一起存：重启后 GX 任务仍要弹"选程序 + 确认名字"的窗
                    source: dl.source || '',
                    savedAt: Date.now(),
                    // ★ HR1: 浏览器来源标记持久化 → 下次重启 fallback 虚拟历史时仍能识别
                    _fromBrowser: !!dl._fromBrowser,
                    _browserSource: dl._browserSource || '',
                });
        }
        // 限制历史数量 (最多 50 条)；**未完成的优先保留**，别被一堆完成记录挤掉
        let trimmed = history;
        if (history.length > 50) {
            const active = history.filter(function (h) { return h.state === 'paused'; });
            const rest = history.filter(function (h) { return h.state !== 'paused'; });
            trimmed = rest.slice(-(Math.max(0, 50 - active.length))).concat(active);
        }
        localStorage.setItem(DL_STORAGE_KEY, JSON.stringify(trimmed));
    } catch (e) {
        console.warn('保存下载历史失败:', e);
    }
}

// 从 localStorage 恢复历史下载
function restoreDownloadsFromStorage() {
    try {
        const raw = localStorage.getItem(DL_STORAGE_KEY);
        if (!raw) return;
        const history = JSON.parse(raw);
        if (!Array.isArray(history)) return;

        let restored = 0;
        for (const item of history) {
            // 如果 SwiftFetch 已恢复此任务, 跳过
            if (state.downloads.has(item.id)) continue;

            state.downloads.set(item.id, {
                name: item.name || '下载',
                url: item.url || '',
                filePath: item.filePath || '',
                progress: item.progress || 0,
                speed: '',
                state: item.state || 'completed',
                total: item.total || 0,
                downloaded: item.downloaded || 0,
                autoExtract: item.autoExtract || false,
                extractDir: item.extractDir || '',
                extractCode: item.extractCode || '',
                createFolder: item.createFolder || false,
                deleteArchive: item.deleteArchive || false,
                // ★ Bug 修复 (2026-09-12): 恢复 createShortcut, 与持久化对称
                createShortcut: item.createShortcut !== false,
                // ★ 恢复时把 gameName / downloadRoot 一起带回来（快捷方式命名要用）
                gameName: item.gameName || '',
                downloadRoot: item.downloadRoot || '',
                extractedPath: item.extractedPath || '',
                cover: item.cover || '',
                // ★ 来源也一起恢复（决定要不要弹"选程序 + 确认名字"的窗）
                source: item.source || '',
                // ★ 落盘的 paused 任务：重启后它确实没在跑，标记成"未完成但可继续"
                _resumable: item.state === 'paused' && !!item.url,
                _finished: item.state !== 'running' && item.state !== 'started' && item.state !== 'paused',
                error: item.error || '',
                // ★ HR1: 从持久化读回浏览器来源标记
                _fromBrowser: !!item._fromBrowser,
                _browserSource: item._browserSource || '',
                engine: item.engine || '',
                _savedAt: item.savedAt || 0,
            });
            restored++;
        }
        if (restored > 0) {
            console.log(`从 localStorage 恢复 ${restored} 条下载历史`);
        }
    } catch (e) {
        console.warn('恢复下载历史失败:', e);
    }
}

// 保存下载列表 (节流包装)
let _persistTimer = null;
function schedulePersistDownloads() {
    if (_persistTimer) return;
    _persistTimer = setTimeout(() => {
        _persistTimer = null;
        persistDownloadsToStorage();
    }, 300);
}

// 从 SwiftFetch (SpeedEngine) 恢复下载状态 (页面刷新后调用)
async function restoreDownloadsFromSwiftFetch() {
    try {
        const tasks = await invoke('get_speed_engine_status');
        if (!tasks || !Array.isArray(tasks)) return;

        for (const dl of tasks) {
            const taskId = dl.task_id;
            // 如果已存在则跳过
            if (state.downloads.has(taskId)) continue;

            const total = dl.total || 0;
            const downloaded = dl.downloaded || 0;
            const progress = dl.progress_percent || (total > 0 ? (downloaded / total) * 100 : 0);

            const stateStr = normalizeDlState(dl.state || 'running');
            const isFinished = stateStr === 'completed' || stateStr === 'failed';

            state.downloads.set(taskId, {
                name: dl.file_name || '下载中',
                url: dl.url || '',
                filePath: dl.file_path || '',
                progress: progress,
                speed: dl.speed_formatted || '',
                speed_bps: dl.speed_bps || 0,
                eta_secs: dl.eta_secs || null,
                state: stateStr,
                total: total,
                downloaded: downloaded,
                autoExtract: false,
                extractDir: '',
                extractCode: '',
                createFolder: false,
                deleteArchive: false,
                _finished: isFinished,
                engine: dl.engine || 'speed',
            });
        }

        if (state.downloads.size > 0) {
            renderDownloadList();
        }
    } catch (e) {
        // 静默失败, 可能 SwiftFetch 未启动
        console.log('恢复 SwiftFetch 下载状态失败:', e);
    }
}

// ============================================================
// 下载进度轮询 (主进度来源, 不依赖事件)
// ============================================================
let pollTimer = null;
// ★★★ lastPollData 正式启用 (之前定义了完全没用来检测停滞 → 这就是"看似在下载但不动"的最后一根稻草!)
//   每 200ms: 记住 downloaded + timestamp; 如果 downloaded>=16KB 前进 → reset累计
//   10s=停滞警告(标红徽标) / 30s=疑似卡死(尝试 SwiftFetch 内部 restart, 最多 1 次防无限重启)
let lastPollData = Object.create(null);   // task_id -> {downloaded, ts}
let _frontStallRestarted = Object.create(null); // task_id -> true (已经自动重启过,防循环)

function startDownloadPolling() {
    frontLog('POLL_START', 'startDownloadPolling() entered, dynamic interval (active=1s / idle=3s)');
    if (pollTimer) { clearTimeout(pollTimer); clearInterval(pollTimer); pollTimer = null; }
    // ★ 性能: 动态轮询间隔 —— 有活动下载任务时 1000ms (事件已提供 200ms 实时进度, 轮询仅作兜底), 空闲时 3s
    //   (旧值 200ms + 200ms 事件 = 双写 IPC 阻塞, 导致 UI 卡死但实际下载正常)
    const scheduleNext = () => {
        const hasActive = [...state.downloads.values()].some(d =>
            d.state === 'running' || d.state === 'starting' || d.state === 'paused' || d.state === 'extracting');
        pollTimer = setTimeout(async () => {
            try { await pollDownloadsStatus(); } catch (_) {}
            scheduleNext();
        }, hasActive ? 1000 : 3000);
    };
    scheduleNext();
}

async function pollDownloadsStatus() {
    try {
        // 记住轮询开始前的任务数量 → 结束时不同说明有删除/有新增, 需要结构性渲染
        const prevTaskCount = state.downloads.size;
        let needRender = false;
        const now = Date.now();
        window._dbgPollCnt = (window._dbgPollCnt || 0) + 1;
        // ★ 前5次 + 每60次(约12秒) 打一次心跳日志, 证明轮询没卡死没停
        if (window._dbgPollCnt <= 5 || window._dbgPollCnt % 60 === 0) {
            frontLog('POLL_HB', 'pollCnt=' + window._dbgPollCnt);
        }

        // 辅助: 带超时的 invoke, 避免后端死锁导致整个轮询被卡住(串行await时一个不返回就全没了)
        const invokeTimeout = (name, args, timeoutMs = 3000) => Promise.race([
            invoke(name, args || {}),
            new Promise((_, reject) => setTimeout(() => reject(new Error(`${name} 超时 ${timeoutMs}ms`)), timeoutMs)),
        ]);

        // 查询 SwiftFetch (SpeedEngine) 活动下载状态 (唯一下载引擎)
        window._pollErrCount = window._pollErrCount || 0;
        const speedTasks = await invokeTimeout('get_speed_engine_status', {}, 3000)
            .then(r => Array.isArray(r) ? r : [])
            .catch((e) => {
                console.error('[poll] get_speed_engine_status 调用失败/超时:', e);
                window._pollErrCount++;
                if (window._pollErrCount % 20 === 0) {
                    try { showToast('⚠ 下载引擎轮询已失败'+window._pollErrCount+'次: ' + (e && e.message ? e.message : String(e)), 'error', 3500); } catch(_) {}
                }
                return [];
            });
        // 成功查询 重置错误计数 (避免成功/失败间隔中误报)
        if (!window._lastPollSpeedTasksEmpty || speedTasks.length > 0) {
            window._pollErrCount = Math.max(0, window._pollErrCount - 1);
        }
        window._lastPollSpeedTasksEmpty = (speedTasks.length === 0);

        // 调试: 每 20 次轮询 (~4秒1次) 打印一次原始返回
        if (window._dbgPollCnt % 20 === 0) {
            console.log('[poll] #'+window._dbgPollCnt+' speedTasks 返回:', JSON.parse(JSON.stringify(speedTasks)));
        }

        // 从 SwiftFetch (SpeedEngine) 查询活动下载状态
        const activeSpeedIds = new Set();
        
        if (speedTasks && Array.isArray(speedTasks)) {
            for (const t of speedTasks) { try {
                activeSpeedIds.add(t.task_id);
                let dl = state.downloads.get(t.task_id);
                if (!dl) {
                    // 初始化 state (只在首次创建时跑一次)
                    const initialState = normalizeDlState((t.state === 'starting') ? 'starting' : 'running');
                    dl = {
                        name: t.file_name || '下载中',
                        url: t.url || '',
                        filePath: t.file_path || '',
                        progress: t.progress_percent || 0,  // 0.1% 浮点精度
                        progress_percent: t.progress_percent || 0,
                        speed: t.speed_formatted || '',
                        speed_bps: t.speed_bps || 0,
                        eta_secs: t.eta_secs || null,
                        state: initialState,
                        total: t.total || 0,
                        downloaded: t.downloaded || 0,
                        autoExtract: false,
                        extractDir: '',
                        extractCode: '',
                        createFolder: false,
                        deleteArchive: false,
                        _finished: false,
                        _last_state: initialState,
                        engine: t.engine || 'speed',
                        bt_peers: Number(t.bt_peers) || 0,
                        bt_seeders: Number(t.bt_seeders) || 0,
                        bt_info_text: t.bt_info_text || '',
                    };
                    state.downloads.set(t.task_id, dl);
                    needRender = true; // ★ 结构性变化 (新增任务) → 保持
                }
                
                // 更新 SpeedEngine 任务进度
                if (dl.state !== 'completed' && dl.state !== 'failed' && dl.state !== 'canceled' && dl.state !== 'extracting') {
                    const newDownloaded = t.downloaded || 0;
                    const newTotal = t.total || 0;
                    const newProgress = t.progress_percent || 0;
                    const newSpeed = t.speed_formatted || '';
                    const newSpeedBps = t.speed_bps || 0;
                    const newEta = t.eta_secs || null;
                    // ★ 修复: 同步后端真实 file_path (byrut BT 去扩展名后的路径), 确保"打开"按钮指向正确目录
                    if (t.file_path && t.file_path !== dl.filePath) {
                        dl.filePath = t.file_path;
                    }
                    // 兼容后端: t.subphase 只有在非空时才更新, 防止轮询返回空值覆盖事件中的诊断信息
                    const newSubphase = (t.subphase && t.subphase.length > 0) ? t.subphase : dl.subphase;
                    const newHttp = t.last_http_status || (dl.last_http_status || null);
                    // 兼容后端字段名: 既支持 phase_stall_secs (前端约定) 也支持 phase_entered_at (后端实际发送)
                    const _rawStall = t.phase_stall_secs !== undefined && t.phase_stall_secs !== null
                        ? t.phase_stall_secs
                        : (t.phase_entered_at !== undefined && t.phase_entered_at !== null ? t.phase_entered_at : 0);
                    const newStall = Number(_rawStall) || 0;
                    // 后端 "downloading"/"connecting" 映射为前端 "running"; "starting" 在有数据时也转 running
                    const rawState = t.state || dl.state;
                    let rawEff = rawState;
                    if (rawEff === 'downloading' || rawEff === 'connecting') rawEff = 'running';
                    if (rawEff === 'starting' && newDownloaded > 0) rawEff = 'running';
                    // ★★★ 最终统一 normalize → dl.state 里也只存 canonical 态
                    //     保证 SpeedEngine 返回 downloading / running 抖动时 effectiveState == dl.state
                    //     → needRender 永远不设 → 不重建 DOM → 按钮不闪
                    let effectiveState = normalizeDlState(rawEff);
                    
                    // 更新进度 (★ downloaded + progress 必须单调递增, 防止一跳一跳倒退)
                    if (newTotal > 0 && newTotal !== dl.total) {
                        dl.total = newTotal;
                    }
                    // downloaded 只能前进不能后退
                    if (newDownloaded > dl.downloaded) {
                        dl.downloaded = newDownloaded;
                    }
                    // progress 只能前进 (★★★ 容差从 0.001% → 0.0001%, 任何浮点微小增量都能推进, 彻底防"明明在下载就是不动")
                    //   倒退门槛 → 任何倒退幅度超过 0.0001% 才拒绝, 并立即 Math.max 做最终单调.
                    const pctFromBackend = Math.max(0, Math.min(100, Number(newProgress) || 0));
                    // ★★★ 根治 C2: 后端 SwiftFetch 很多实现只发整数 progress_percent (X% 整数, 没有小数点),
                    //   前端 downloaded/total 本地算一遍做"小数补充", 取 MAX(后端%, Calc%) 作为最终推进值,
                    //   任何场景都启用 (不是 newProgress===0 才启用! 旧条件门槛太高 → 整数%永远不补充→小数点后永远0)
                    let calcPct = -1;
                    if (newTotal > 0 && newDownloaded >= 0) {
                        calcPct = Math.min(100, (newDownloaded / newTotal) * 100);
                    }
                    const bestPct = calcPct >= 0 ? Math.max(pctFromBackend, calcPct) : pctFromBackend;
                    if (bestPct >= dl.progress - 0.0001) {
                        dl.progress = Math.max(dl.progress, Math.min(100, bestPct));
                    }
                    // ★★★ 根治 C1: 99.9% 永远卡死闸门!
                    //   触发条件(任一满足即推进到100%):
                    //     (a) 后端 pct >= 99.95 且 downloaded + 容差 >= total (最后几MB rounding误差)
                    //     (b) 后端直接发 state === 'completed' / 'finished' / 'done' / 'success'
                    //     (c) downloaded >= total 且 total > 0 (哪怕后端 pct 仍发 99.9, 也按真实字节为准)
                    const _rawStateDown = String(t.state || '').toLowerCase();
                    const forceCompleteByState = (_rawStateDown === 'completed') || (_rawStateDown === 'finished')
                        || (_rawStateDown === 'done') || (_rawStateDown === 'success');
                    const downloadedReachedTotal = (newTotal > 0) && (newDownloaded >= newTotal - 65536); // 64KB 容差 (SwiftFetch 尾包缓存未刷)
                    const pctAlmostFull = (pctFromBackend >= 99.95) || (calcPct >= 99.95);
                    if ((dl.progress < 100) && (forceCompleteByState || downloadedReachedTotal || pctAlmostFull)) {
                        dl.progress = 100;
                        // ★★★ 修复: 无论 reason 是 state/downloaded/pct, 只要强制到 100% 就把 dl.state 设为 completed
                        //   之前只有 forceCompleteByState 才设 completed, 导致 reason=pct 时 progress=100 但 state=running,
                        //   后端停止发事件后永远不会触发 onDownloadFinished → 自动解压永远不执行!
                        //   BT 任务也会有 pctAlmostFull, 但 onDownloadFinished 里的 isBT 检测会跳过解压, 所以安全
                        dl.state = 'completed';
                        effectiveState = 'completed'; // ★ 防止下方 effectiveState !== dl.state 把它覆盖回 running
                        frontLog('DL_FORCE_100', `file=${(dl.name||'').slice(0,40)} backend%=${pctFromBackend} calc%=${calcPct>=0?calcPct.toFixed(2):'N/A'} downloaded=${newDownloaded} total=${newTotal} reason=${forceCompleteByState?'state':(downloadedReachedTotal?'downloaded':'pct')}`);
                    }
                    if (newProgress >= 100 && dl.progress < 100) {
                        dl.progress = 100;
                    }
                    // ★ 速度: 双 EMA 平滑 (后端已经 EMA, 前端再 EMA)
                    // 注意: 不再 needRender=true → 交给 patchIncrementally 显示速度文本, 不动按钮 DOM
                    if (typeof newSpeedBps === 'number' && isFinite(newSpeedBps) && newSpeedBps >= 0) {
                        applySpeedEma(dl, newSpeedBps);
                    } else if (typeof newSpeed === 'string' && newSpeed.length > 0) {
                        // 文字型诊断速度直接保留 (只更新内存值, 不 needRender)
                        dl.speed = newSpeed;
                    }
                    dl.eta_secs = newEta;
                    // 诊断 (★ 内存更新即可, 不 needRender; patchIncrementally 会顺带拼到文本里)
                    if (newSubphase !== dl.subphase) { dl.subphase = newSubphase; }
                    if (newHttp !== dl.last_http_status) { dl.last_http_status = newHttp; }
                    if (newStall !== dl.phase_stall_secs) { dl.phase_stall_secs = newStall; }
                    // ===== 引擎 + BT 专属字段 (轮询合并) =====
                    if (t.engine !== undefined && t.engine !== dl.engine) { dl.engine = t.engine; needRender = true; /* 引擎tag是结构性的 */ }
                    const peers = Number(t.bt_peers) || 0;
                    if (peers !== dl.bt_peers) dl.bt_peers = peers;
                    const seeders = Number(t.bt_seeders) || 0;
                    if (seeders !== dl.bt_seeders) dl.bt_seeders = seeders;
                    if (t.bt_info_text !== undefined && t.bt_info_text !== dl.bt_info_text) {
                        dl.bt_info_text = t.bt_info_text;
                    }
                    if (t.progress_percent !== undefined && typeof t.progress_percent === 'number' && isFinite(t.progress_percent)) {
                        dl.progress_percent = t.progress_percent;
                    }
                    if (typeof t.speed_bps === 'number' && isFinite(t.speed_bps)) dl.speed_bps = t.speed_bps;

                    // ★ BT 磁盘进度兜底: 如果 BT 任务 downloaded=0 但 filePath 目录有文件,
                    //   说明 BT 引擎已下载数据但 ctx.downloaded 未正确更新 → 用磁盘实际大小补充
                    //
                    // ★ 修复 (2026-10-03): 这里原本直接用 `isBT` 和 `id`, 但**两者在本函数里
                    //   都没有定义** (本函数的循环变量是 `t`)。JS 求值未声明的标识符会抛
                    //   ReferenceError, 被下面 per-task 的 catch 静默吞掉 —— 后果是每个活跃
                    //   下载的**每次轮询**都在这一行中断, 后面全部逻辑失效:
                    //     · 状态跃迁与按钮切换 (completed/failed)
                    //     · 轮询侧的完成检测 → onDownloadFinished
                    //     · 轮询侧的失败检测
                    //     · 整个前端停滞检测 / 自动重启 (所以停滞徽标从未出现过)
                    //   现在按本文件其它处的统一写法在本作用域内推导 isBT, 并用 t.task_id。
                    const _dlType = String(dl.type || dl.link_type || '').toLowerCase();
                    const isBT = (dl.engine === 'bt')
                              || _dlType === 'torrent'
                              || _dlType === 'bt'
                              || String(t.task_id).startsWith('bt-')
                              || String(dl.url || '').toLowerCase().startsWith('magnet:')
                              || String(dl.url || '').toLowerCase().endsWith('.torrent');
                    if (isBT && newDownloaded === 0 && dl.filePath && !dl._bt_disk_checked) {
                        dl._bt_disk_checked = true;
                        setTimeout(async () => {
                            try {
                                const r = await invoke('check_bt_disk_progress', { dir: dl.filePath });
                                if (r && r.disk_bytes > 0 && r.file_count > 0) {
                                    // 磁盘有数据但后端报0 → 用磁盘数据
                                    dl.downloaded = r.disk_bytes;
                                    if (dl.total > 0) {
                                        dl.progress = Math.max(dl.progress, Math.min(99, (r.disk_bytes / dl.total) * 100));
                                    }
                                    frontLog('BT_DISK_FIX', 'task=' + t.task_id + ' disk_bytes=' + r.disk_bytes + ' files=' + r.file_count + ' → 补充 downloaded');
                                    try { patchDownloadItemIncrementally(dl, t.task_id); } catch (e) {}
                                }
                            } catch (e) { /* 静默 */ }
                            // 30s 后允许再次检查
                            setTimeout(() => { if (dl) dl._bt_disk_checked = false; }, 30000);
                        }, 5000);
                    }
                    
                    // ★ 仅状态切换 (按钮类型变) 才结构性重建
                    if (effectiveState && effectiveState !== dl.state) {
                        dl.state = effectiveState;
                        dl._last_state = effectiveState;
                        needRender = true;
                    }
                    
                    // 检测完成/失败
                    // ★ Bug 修复 (2026-09-15): 删除 dl._finished = true 提前设置!
                    //   之前在这里设 _finished=true, 紧接着调用 onDownloadFinished,
                    //   但 onDownloadFinished 入口有 `if (dl._finished) return;` 直接退出,
                    //   导致 DL_FIN_OK 日志不写、handleAutoExtract 自动解压永不触发 → "卡 100% 无解压"
                    //   修复: 由 onDownloadFinished 内部 line 4281 自己设置 _finished, 防重复触发
                    if (effectiveState === 'completed' && !dl._finished) {
                        dl.state = 'completed';
                        dl.progress = 100;
                        dl._last_state = 'completed';
                        onDownloadFinished({
                            payload: {
                                task_id: t.task_id,
                                state: 'completed',
                                file_path: t.file_path,
                            }
                        });
                        needRender = true; // ★ 完成 → 按钮变成"打开", 必须重建
                    } else if (effectiveState === 'failed' && !dl._finished) {
                        dl._finished = true;
                        dl.state = 'failed';
                        dl._last_state = 'failed';
                        needRender = true; // ★ 失败 → 按钮变成"重试", 必须重建
                    }
                    // ★ 移除原来的: "if (newDownloaded>0 ...) needRender=true"
                    //   这是每200ms必触发的闪烁元凶!

                    // ================================================================
                    // ★★★ R5: 前端独立停滞检测 (不依赖后端 phase_stall_secs, 真正解决"下载下一点卡住不动")
                    //   之前 lastPollData 定义了但从未使用 → 任何停滞都无法被前端独立发现!
                    //   逻辑:
                    //     * downloaded 增加 ≥ 16KB → reset 累计 (ts = now, stall 秒数清零)
                    //     * 否则 stall_secs = (now - lastTs)/1000 累计
                    //     * ≥ 10s → dl._front_stall_secs 暴露给 patch 显示红色徽标
                    //     * ≥ 30s 且没重启过 → 调 SwiftFetch retry_task_id 自动尝试恢复一次
                    // ================================================================
                    const tidStr = String(t.task_id);
                    const prevSnap = lastPollData[tidStr];
                    // 启动中(0字节)不计入停滞(可能是探测阶段) → downloaded<16KB的0字节不告警
                    const progressGate = newDownloaded >= 16384 || (dl.downloaded && dl.downloaded >= 16384);
                    if (prevSnap && progressGate) {
                        const movedForward = newDownloaded > (prevSnap.downloaded + 16384);  // ≥16KB 前进才算
                        if (movedForward) {
                            lastPollData[tidStr] = { downloaded: newDownloaded, ts: now };
                            dl._front_stall_secs = 0;
                            dl._front_stall_since = 0;
                        } else {
                            const secs = (now - prevSnap.ts) / 1000;
                            dl._front_stall_secs = Math.max(dl._front_stall_secs || 0, secs);
                            dl._front_stall_since = dl._front_stall_since || prevSnap.ts;
                            // ≥30s & 非 paused & 还没自动重启过 → 调 SwiftFetch 内部重试恢复 1 次
                            if (secs >= 30 && (effectiveState === 'running' || effectiveState === 'starting') && !_frontStallRestarted[tidStr]) {
                                _frontStallRestarted[tidStr] = true;
                                frontLog('STALL_AUTO_RESTART', 'task=' + tidStr.slice(0,14) + ' stall=' + secs.toFixed(0) + 's name=' + (dl.name||'').slice(0,40) + ' retry');
                                try {
                                    if (typeof invoke === 'function') invoke('retry_download', { taskId: tidStr }).catch(()=>{});
                                } catch (_) {}
                                dl.speed = dl.speed || `⚠ 停滞 ${secs.toFixed(0)}秒 → 自动重试中`;
                                dl._autoRestartHinted = true;
                            }
                        }
                    } else {
                        // 首次记录 or 刚起任务 downloaded<16KB 还没前进 → 写入 baseline
                        lastPollData[tidStr] = { downloaded: newDownloaded, ts: now };
                    }
                }
                } catch(e) { console.error('[poll] 单任务处理异常 (task_id='+(t? t.task_id:'?')+'):', e); }
            }
        }
        // 清理 lastPollData 中已经不存在的 taskId (避免内存泄漏)
        try {
            for (const _k of Object.keys(lastPollData)) {
                if (!state.downloads.has(_k)) {
                    delete lastPollData[_k];
                    delete _frontStallRestarted[_k];
                }
            }
        } catch (_) {}

        // ================================================================
        // ★★★ B2c-1: state.downloads 全量 独立迭代 (per-task try/catch 彻底防级联卡死)
        //   之前: 只在 SwiftFetch 返回的 speedTasks for-循环里 写了 try/catch
        //   但如果 state.downloads 里有 "没在 SwiftFetch 中 (非活动的 paused/completed/failed/extracting
        //   或 新建 但 SwiftFetch 还没注册 的条目)", 这些条目如果 patch/字段出错, 之前没有任何 catch
        //   → 整个 poll 循环里 Step1 patchAll 抛 ReferenceError (dl 为null / 某个 el.classList === undefined 等)
        //   → 后续 Step2 renderDownloadList 也跑不了 → 用户看到"进度弹一下不会动"
        //   修复: 遍历 state.downloads 里 所有条目, 每条独立 try/catch, 独立做:
        //         ① 状态 canon 规范化 (保证 canonical)
        //         ② ETA 稳定化 (后端 eta 秒 有时 跳变)
        //         ③ 99.9% 闸门 (二次兜底: 即便 speedTasks 中因 backend 返回奇怪值没触发, 这里再推进)
        //   任何一条出错 → console.error + frontLog + 继续下一条 (不会中断 patchAll)
        // ================================================================
        try {
            const entries = Array.from(state.downloads.entries());
            for (let i = 0; i < entries.length; i++) { try {
                const [sid, sdl] = entries[i];
                if (!sdl) continue;
                const sCanon = normalizeDlState(sdl.state || 'running');
                // --- 99.9% 闸门 二次兜底 (2nd pass) ---
                if (sCanon === 'running' || sCanon === 'starting') {
                    const st = Number(sdl.total) || 0;
                    const sd = Number(sdl.downloaded) || 0;
                    const sp = Number(sdl.progress) || 0;
                    const calc = st > 0 ? Math.min(100, (sd / st) * 100) : -1;
                    // ★ downloaded >= total-64KB 或 downloaded > total (超量写入也算完成)
                    const downloadedReached = st > 0 && sd >= st - 65536;
                    const pctFull = sp >= 99.95 || (calc >= 0 && calc >= 99.95);
                    // ★★★ 新增: 任务从 SpeedEngine 消失 (不在 activeSpeedIds 中) + downloaded > 0
                    //   说明 SwiftFetch 已完成或异常移除了任务, 但后端没发 download-finished 事件
                    //   → 前端必须独立检测并触发完成处理
                    //   ★ 排除自测任务 (ID 以 __selftest 开头, 不在 SpeedEngine 中是正常的)
                    const isSelfTest = String(sid).startsWith('__selftest');
                    const lostFromEngine = !isSelfTest && !activeSpeedIds.has(sid) && sd > 0;

                    if (sp < 100 && (downloadedReached || pctFull)) {
                        sdl.progress = 100;
                        sdl._force_100_pass2 = true;
                        needRender = true;
                    }

                    // ★★★ 关键修复: 如果任务从 SpeedEngine 消失 且 下载量已达 99%+ (或超量)
                    //   → 强制标记完成并触发 onDownloadFinished (否则自动解压永远不触发!)
                    if (lostFromEngine && (downloadedReached || pctFull || sp >= 99) && !sdl._finished) {
                        sdl.progress = 100;
                        sdl.state = 'completed';
                        sdl._force_100_pass2 = true;
                        sdl._lost_from_engine = true; // 标记原因
                        needRender = true;
                        frontLog('DL_FORCE_COMPLETE_LOST', 'task=' + String(sid).slice(0,14) + ' name=' + (sdl.name||'').slice(0,40) + ' downloaded=' + sd + ' total=' + st + ' completed (lost from engine)');
                        // 异步触发完成处理 (不在 poll 循环中直接 await, 避免阻塞轮询)
                        setTimeout(() => {
                            try {
                                onDownloadFinished({ payload: { task_id: sid, state: 'completed' } });
                            } catch(e) { console.error('[poll] onDownloadFinished (lost) 异常:', e); }
                        }, 0);
                    }

                    // ★★★ 新增: 任务从 SpeedEngine 消失 但下载量远未完成 (<90%)
                    //   → 标记失败 (SwiftFetch 异常退出, 不可能恢复)
                    if (lostFromEngine && !downloadedReached && !pctFull && sp < 90 && !sdl._finished) {
                        sdl.state = 'failed';
                        sdl.error = '下载引擎异常中断 (任务已从引擎中消失)';
                        sdl._lost_from_engine = true;
                        needRender = true;
                        frontLog('DL_FORCE_FAIL_LOST', 'task=' + String(sid).slice(0,14) + ' name=' + (sdl.name||'').slice(0,40) + ' downloaded=' + sd + ' total=' + st + ' pct=' + sp.toFixed(2) + 'pct failed (lost from engine, below 90pct)');
                        sdl._finished = true; // 防止重复触发
                    }
                }
                // --- ETA 稳定: 后端 eta_secs 可能瞬时跳大跳小 (算法重算), 前端独立慢速 EMA(α=0.05) ---
                const rawEta = Number(sdl.eta_secs) || 0;
                if (rawEta > 0) {
                    const prevEta = Number(sdl._eta_ema) || 0;
                    const alpha = 0.05;  // ≈ 20 轮 (≈4秒) 窗口, 稳定不抖
                    sdl._eta_ema = (prevEta === 0) ? rawEta : (prevEta * (1-alpha) + rawEta * alpha);
                } else if (rawEta === 0 && (sCanon === 'completed' || sCanon === 'failed' || sCanon === 'canceled')) {
                    sdl._eta_ema = 0;
                }
            } catch (pe) {
                console.error('[poll] state.downloads 单任务兜底处理失败:', pe);
                frontLog('POLL_STATE_EACH_FAIL', String(pe && pe.message ? pe.message : pe));
            } }
        } catch (outer_e) {
            console.error('[poll] state.downloads 外层遍历失败:', outer_e);
        }

        // ================================================================
        // ★★★ B2c-2: 后端 进度容差从 0.001% 降到 0.0001% (10x 更细, 任何浮点微小增量都能显示)
        //             → 位置: 上面已经处理 bestPct 时调用 >= dl.progress - 0.001
        //               重新覆写这一行, 改为 -0.0001 同时保证 Math.max 最终单调
        // (此段逻辑 在上半段 speedTasks for 循环内, 单独 Edit)
        // ================================================================

        // ★★★ 分两步更新, 彻底告别闪烁:
        //   Step 1: patchAllDownloadsIncrementally → 直接改 DOM 节点的 width/textContent, 不动按钮 (无闪烁)
        //   Step 2: 仅当 "结构性变化" (新建任务/删除任务/状态切换→按钮变了) 才调用 renderDownloadList 重建整卡
        try { patchAllDownloadsIncrementally(); } catch (e) {
            console.error('[poll] patchAllDownloadsIncrementally 失败:', e);
            frontLog('POLL_PATCHALL_FAIL', String(e && e.message ? e.message : e));
        }

        const structuralChange = needRender || (state.downloads.size !== prevTaskCount);
        if (structuralChange) {
            try { renderDownloadList(); } catch (e) {
                console.error('[poll] renderDownloadList structuralChange 失败:', e);
            }
        }
    } catch (e) {
        console.log('轮询下载状态失败:', e);
        frontLog('POLL_FAIL_OUTER', String(e && e.message ? e.message : e));
    }
}

/// 桌面快捷方式该叫什么名字。
///
/// ★ 之前直接用压缩包名, 于是从 galgamex 下来的 `#A9667.zip` 会在桌面留下一个
///   `#a9667.lnk` —— 用户看到的"快捷方式错了"也包括这一半。
///   按可信度排序: 详情页/GX 传来的真游戏名 > 解压出来的目录名 > 压缩包名。
function vxShortcutName(dl, extractedPath) {
    const strip = (s) => String(s || '')
        .replace(/\.(zip|rar|7z|tar|gz|bz2)$/i, '')
        .replace(/\.part\d+$/i, '')
        .trim();
    const generic = /^(game|games|download|downloads|游戏|下载|新建文件夹|new folder)$/i;
    const base = (p) => {
        const t = String(p || '').replace(/[\\/]+$/, '');
        const i = Math.max(t.lastIndexOf('\\'), t.lastIndexOf('/'));
        return i >= 0 ? t.slice(i + 1) : t;
    };
    const norm = (p) => String(p || '').replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase();

    // 1) 详情页 / GX 带过来的真游戏名最可信
    const explicit = strip(dl && (dl.gameName || dl.game_name));
    if (explicit && !generic.test(explicit)) return explicit;

    // 2) 解压出来的目录名。★ 平铺解压时它就是下载根目录 (D:\game), 那不是游戏名, 要排掉。
    const ep = base(extractedPath);
    const root = dl && dl.downloadRoot;
    if (ep && extractedPath && !(root && norm(extractedPath) === norm(root)) && !generic.test(ep)) {
        return strip(ep) || ep;
    }

    // 3) 退回压缩包名
    return strip(dl && (dl.name || dl.filename)) || '游戏';
}

// ============================================================
// 创建桌面快捷方式
// ------------------------------------------------------------
// ★ 用户要求（2026-10-08 第二轮）：
//   · **只有 GX 资源**才弹窗；KO / BY 全部自动（自动挑程序、自动命名，不打扰）。
//   · GX 在"下载完成 + 解压完成"时弹窗，并**用资源管理器直接打开解压位置**，
//     让玩家自己选要启动的程序；
//   · **选完程序之后**再确认快捷方式名字；
//   · 也可以取消（不创建）或直接用默认。
// 所以弹窗是两步：① 选 exe（带「打开解压位置」）→ ② 确认名字 → 创建。
// ============================================================
let _scPending = null;   // { targetPath, defaultName, exes, chosen, step, auto:boolean }

// 「以后自动创建，不再询问」的开关（用户在弹窗里勾）
const SC_AUTO_KEY = 'vortex_shortcut_auto';

function scSanitize(name) {
    return String(name || '').replace(/[<>:"/\\|?*]/g, '_').replace(/\s+$/, '');
}

function scShowStep(n) {
    const s1 = document.getElementById('sc-step1');
    const s2 = document.getElementById('sc-step2');
    const next = document.getElementById('sc-next');
    const back = document.getElementById('sc-back');
    const confirm = document.getElementById('sc-confirm');
    // 「打开解压位置」只在第 1 步（挑程序）有意义；第 2 步留着会把底部挤成两行
    const openFolder = document.getElementById('sc-open-folder');
    if (s1) s1.style.display = (n === 1 ? 'flex' : 'none');
    if (s2) s2.style.display = (n === 2 ? 'flex' : 'none');
    if (next) next.style.display = (n === 1 ? '' : 'none');
    if (back) back.style.display = (n === 2 ? '' : 'none');
    if (confirm) confirm.style.display = (n === 2 ? '' : 'none');
    if (openFolder) openFolder.style.display = (n === 1 ? '' : 'none');
    if (_scPending) _scPending.step = n;
    const title = document.getElementById('sc-title');
    if (title) title.textContent = (n === 1 ? '选择要启动的程序' : '确认快捷方式名字');
}

/// 打开弹窗。
///   opts: { targetPath, defaultName, gameName, cover, exes?, auto?: true }
///   auto=true → 直接进第 2 步（程序自动挑，只确认名字）；否则停在第 1 步让用户挑程序。
///   ★ 两种模式都**要**扫候选列表：auto 模式扫完要把自动挑中的程序名显示在第 2 步上
///     （用户点「上一步」也能看到完整列表）。
function openShortcutDialog(opts) {
    const modal = document.getElementById('sc-modal');
    if (!modal) return;
    const targetPath = String((opts && opts.targetPath) || '');
    const defaultName = scSanitize((opts && opts.defaultName) || '') || '游戏';
    const autoMode = !!(opts && opts.auto);
    _scPending = {
        targetPath: targetPath,
        defaultName: defaultName,
        exes: (opts && opts.exes) || [],
        chosen: null,
        step: autoMode ? 2 : 1,
    };
    const myPending = _scPending;

    const nameEl = document.getElementById('sc-game-name');
    if (nameEl) nameEl.textContent = (opts && opts.gameName) || defaultName;
    const pathEl = document.getElementById('sc-game-path');
    if (pathEl) { pathEl.textContent = targetPath; pathEl.title = targetPath; }

    const coverEl = document.getElementById('sc-cover');
    if (coverEl) {
        const cover = (opts && opts.cover) || '';
        coverEl.innerHTML = cover
            ? '<img src="' + escapeHtml(cover) + '" onerror="this.replaceWith(document.createTextNode(\'🎮\'))">'
            : '🎮';
    }

    const input = document.getElementById('sc-name-input');
    if (input) input.value = defaultName;
    updateScPreview();

    modal.style.display = 'flex';
    // ★ 把主窗口提到前台：下载+解压常要一两分钟，那时窗口多半被别的程序盖住了，
    //   不叫一下用户根本看不到这个确认框。
    try { invoke('raise_main_window'); } catch (e) {}

    const pickedEl = document.getElementById('sc-picked-name');
    if (autoMode && pickedEl) pickedEl.textContent = '正在扫描解压目录…';
    scShowStep(_scPending.step);

    // 扫候选（两种模式都要）：扫完 auto 模式补上"自动选中的程序"，并让「上一步」可用
    scLoadExeList(targetPath, defaultName).then(function () {
        // 期间用户可能已经关窗 / 又开了新的弹窗，别乱动
        if (_scPending !== myPending) return;
        if (myPending.step === 2) scGoStep2(true);
    });

    setTimeout(function () {
        try { if (input && _scPending === myPending && _scPending.step === 2) { input.focus(); input.select(); } } catch (e) {}
    }, 80);
}

/// 拉候选程序列表（后端用与"自动挑"完全相同的打分，所以推荐项就是自动会选的那个）
async function scLoadExeList(dir, hint) {
    const box = document.getElementById('sc-exe-list');
    if (!box) return;
    box.innerHTML = '<div class="loading">正在扫描解压目录…</div>';
    try {
        const list = await invoke('list_exe_candidates', { dir: dir, hintName: hint || '' }) || [];
        if (!list.length) {
            box.innerHTML = '<div class="sc-exe-none">这个目录里没找到 exe —— 可以直接关掉这个窗，或去解压位置手动看看。</div>';
            if (_scPending) _scPending.chosen = '';
            return;
        }
        _scPending.exes = list;
        // ★ 后端只在"有把握"时才给 auto（名字对得上 / 单游戏目录 / 只有一个候选）。
        //   共享目录里名字对不上时它**不给**推荐 —— 那种情况下不预设选中项，
        //   让用户自己点一个，免得一路下一步建出指错游戏的快捷方式。
        const auto = list.find(function (x) { return x.auto; });
        _scPending.chosen = auto ? auto.path : '';
        box.innerHTML = list.map(function (x, i) {
            const sel = (x.path === _scPending.chosen) ? ' sel' : '';
            return '<div class="sc-exe-item' + sel + '" data-sc-exe="' + i + '">'
                + '<span class="sc-exe-radio"></span>'
                + '<div class="sc-exe-main">'
                +   '<div class="sc-exe-name">' + escapeHtml(x.name) + '</div>'
                +   '<div class="sc-exe-sub">' + escapeHtml(x.rel || '') + (x.size ? ' · ' + formatBytes(x.size) : '') + '</div>'
                + '</div>'
                + (x.auto ? '<span class="sc-exe-badge">推荐</span>' : '')
                + '</div>';
        }).join('');
        // 提示文案跟着候选情况变（避免"推荐"缺失时用户不知道该怎么办）
        const tip = document.getElementById('sc-exe-tip');
        if (tip) {
            if (!auto) {
                tip.textContent = '这个目录里有多个游戏，没找到名字对得上的程序 —— 请自己选一个（旁边「打开解压位置」能帮你确认）。';
            } else if (list.length === 1) {
                tip.textContent = '只找到一个 exe，直接下一步就行。';
            } else {
                tip.textContent = '标「推荐」的是自动挑选的结果；选错了可以自己换一个。';
            }
        }
    } catch (e) {
        box.innerHTML = '<div class="sc-exe-none">扫描失败：' + escapeHtml(String(e && e.message ? e.message : e)) + '</div>';
    }
}

function closeShortcutDialog() {
    const modal = document.getElementById('sc-modal');
    if (modal) modal.style.display = 'none';
    _scPending = null;
}

function updateScPreview() {
    const input = document.getElementById('sc-name-input');
    const prev = document.getElementById('sc-preview-name');
    if (!prev) return;
    const raw = input ? input.value : '';
    const shown = scSanitize(raw) || ((_scPending && _scPending.defaultName) || '游戏');
    prev.textContent = shown;
}

/// 第 1 步 → 第 2 步
///   silent=true 时（候选列表刚扫完、自动模式补显示）不弹"请先选一个"的提示，
///   因为那是用户没做过任何操作时的后台回调，弹提示会莫名其妙。
function scGoStep2(silent) {
    if (!_scPending) return;
    const exes = _scPending.exes || [];
    // ★ 没预设选中项（共享目录里名字对不上）时必须让用户先点一个，不能替他猜
    if (!_scPending.chosen) {
        if (!exes.length) { scShowStep(2); return; }
        if (silent) return;
        showToast('请先选一个要启动的程序', 'error', 2600);
        return;
    }
    const picked = exes.find(function (x) { return x.path === _scPending.chosen; }) || exes[0];
    const el = document.getElementById('sc-picked-name');
    if (el) {
        if (!picked) {
            el.textContent = '（未选择，将自动挑一个）';
        } else {
            // rel 就是文件名时不必重复显示
            const rel = picked.rel && picked.rel !== picked.name ? '（' + picked.rel + '）' : '';
            el.textContent = picked.name + rel;
        }
    }
    scShowStep(2);
    if (silent) return;   // 后台回调：别抢用户焦点（他可能正在输入名字）
    setTimeout(function () {
        const input = document.getElementById('sc-name-input');
        try { if (input) { input.focus(); input.select(); } } catch (e) {}
    }, 60);
}

async function confirmShortcutDialog() {
    const input = document.getElementById('sc-name-input');
    const raw = input ? input.value : '';
    const name = scSanitize(raw) || ((_scPending && _scPending.defaultName) || '游戏');
    if (!_scPending) { closeShortcutDialog(); return; }
    // 用户选了具体程序就传 exe 路径（后端文件分支直接用），否则传目录让后端自动挑
    const target = (_scPending.chosen && /\.exe$/i.test(_scPending.chosen)) ? _scPending.chosen : _scPending.targetPath;
    if (!target) { closeShortcutDialog(); return; }
    const btn = document.getElementById('sc-confirm');
    const old = btn ? btn.textContent : '';
    if (btn) { btn.disabled = true; btn.textContent = '创建中…'; }
    try {
        await invoke('create_desktop_shortcut', { targetPath: target, shortcutName: name });
        // 勾了"以后自动创建"就记住，下次解压完直接建、不再弹窗
        const forever = document.getElementById('sc-auto-forever');
        if (forever && forever.checked) {
            try { localStorage.setItem(SC_AUTO_KEY, '1'); } catch (e) {}
            frontLog('SC_AUTO', '用户勾选「以后自动创建」');
        }
        frontLog('SC_OK', 'name=' + name + ' target=' + target.slice(-60) + ' -> 桌面快捷方式已创建');
        showToast('✅ 桌面快捷方式已创建：' + name, 'success', 2600);
        closeShortcutDialog();
    } catch (e) {
        const msg = String(e && e.message ? e.message : e);
        frontLog('SC_FAIL', 'name=' + name + ' err=' + msg.slice(0, 200));
        // 失败时**不要关窗**：用户可以改个名字/换个程序再试
        showToast('创建失败：' + msg, 'error', 7000);
    } finally {
        if (btn) { btn.disabled = false; btn.textContent = old || '创建快捷方式'; }
    }
}

/// KO / BY：不打扰，直接用自动推断的名字和程序建（失败才提示）
async function autoCreateShortcut(targetPath, defaultName, taskId) {
    try {
        await invoke('create_desktop_shortcut', { targetPath: targetPath, shortcutName: defaultName });
        frontLog('SC_OK', 'task=' + (taskId || '') + ' name=' + defaultName + ' -> 自动创建（非 GX 资源）');
        showToast('已自动创建桌面快捷方式：' + defaultName, 'success', 2400);
    } catch (e) {
        const msg = String(e && e.message ? e.message : e);
        frontLog('SC_FAIL', 'task=' + (taskId || '') + ' name=' + defaultName + ' err=' + msg.slice(0, 200));
        showToast('桌面快捷方式没创建成功：' + msg, 'error', 6000);
    }
}

/// 解压成功后决定"怎么建桌面快捷方式"。
///
/// ★ 用户要求（2026-10-08 第四轮）：**创建之前名字一定要能改**。
///   之前只有 GX 弹窗，KO/BY 直接自动建 —— 用户下载完发现"改不了名字"。
///   现在**所有**下载都弹这个窗：
///     · GX   → 从**第 1 步**开始（先选要启动的程序，同时把解压位置用资源管理器打开）
///     · 其它 → 直接到**第 2 步**（程序已自动挑好并显示出来，只确认名字），
///              想换程序点「上一步」就能进列表
///   这样既满足"KO/BY 的程序自动选"，又保证名字在创建前随时能改。
/// 判定用 `!== false`：模块内下载（GX / 站外抓取）的任务条目 createShortcut
/// 常常是 undefined，严格等于 true 会**静默不创建**。
async function maybeHandleShortcut(dl, taskId) {
    if (!dl || dl.createShortcut === false || !dl.extractedPath) return;
    const gameName = vxShortcutName(dl, dl.extractedPath);
    const isGx = (dl.source === 'galgamex' || dl.source === 'galgamex-direct');
    // 用户在弹窗里勾过"以后自动创建" → 回到全自动，不再打扰
    let forever = false;
    try { forever = localStorage.getItem(SC_AUTO_KEY) === '1'; } catch (e) {}
    if (forever) {
        await autoCreateShortcut(dl.extractedPath, gameName, taskId);
        return;
    }
    if (isGx) {
        // 先把解压位置打开（用户明确要求），再弹窗让玩家选程序
        try { invoke('open_folder', { path: dl.extractedPath }); } catch (e) {}
    }
    openShortcutDialog({
        targetPath: dl.extractedPath,
        defaultName: gameName,
        gameName: (dl.gameName || dl.name || gameName),
        cover: dl.cover || dl.header_image || '',
        // GX 要自己挑程序 → 停在第 1 步；其它直接把自动挑好的程序摆出来
        auto: !isGx,
    });
    frontLog('SC_ASK', 'task=' + taskId + ' name=' + gameName
        + (isGx ? ' -> GX 弹窗（选程序 + 确认名字）' : ' -> 弹窗确认名字（程序自动选）'));
}

// 绑定（文档级委托：弹窗在 body 顶层，任何页面都能用）
document.addEventListener('click', function (e) {
    if (!e.target || !e.target.closest) return;
    if (e.target.closest('[data-sc-close]')) { closeShortcutDialog(); return; }
    if (e.target.closest('#sc-confirm')) { confirmShortcutDialog(); return; }
    if (e.target.closest('#sc-next')) { scGoStep2(); return; }
    if (e.target.closest('#sc-back')) { scShowStep(1); return; }
    if (e.target.closest('#sc-open-folder')) {
        const dir = _scPending && _scPending.targetPath;
        if (dir) { try { invoke('open_folder', { path: dir }); } catch (err) {} }
        return;
    }
    if (e.target.closest('#sc-name-reset')) {
        const input = document.getElementById('sc-name-input');
        if (input && _scPending) { input.value = _scPending.defaultName; updateScPreview(); }
        return;
    }
    // 选程序
    const item = e.target.closest('[data-sc-exe]');
    if (item && _scPending) {
        const idx = Number(item.dataset.scExe);
        const x = (_scPending.exes || [])[idx];
        if (x) {
            _scPending.chosen = x.path;
            const list = document.getElementById('sc-exe-list');
            if (list) {
                list.querySelectorAll('.sc-exe-item').forEach(function (el, i) {
                    el.classList.toggle('sel', i === idx);
                });
            }
        }
        return;
    }
});
document.addEventListener('input', function (e) {
    if (e.target && e.target.id === 'sc-name-input') updateScPreview();
});
document.addEventListener('keydown', function (e) {
    const modal = document.getElementById('sc-modal');
    if (!modal || modal.style.display === 'none' || modal.style.display === '') return;
    if (e.key === 'Enter') {
        e.preventDefault();
        if (_scPending && _scPending.step === 1) scGoStep2(); else confirmShortcutDialog();
    } else if (e.key === 'Escape') { e.preventDefault(); closeShortcutDialog(); }
});

async function handleAutoExtract(dl) {
    // ★ 用 filePath 判断扩展名 (dl.name 在 byrut/BT 等场景下可能为空, 但 filePath 一定由 modal 设置)
    //   先剥掉 URL query (?verify=...) 再取 ext, 避免 ? 之后的字符污染扩展名判断
    //   兼容: filePath 缺失时回退到 name, 都没有就跳过解压
    let fileBase = String(dl.filePath || dl.name || '');
    // 剥掉 query string
    const qIdx = fileBase.indexOf('?');
    if (qIdx > 0) fileBase = fileBase.substring(0, qIdx);
    // 取最后一个 '.' 后的部分作为扩展名 (注意 Windows 路径分隔符是 '\\')
    const pathForExt = fileBase.replace(/\//g, '\\');
    const lastBackslash = pathForExt.lastIndexOf('\\');
    const fileNamePart = lastBackslash >= 0 ? pathForExt.substring(lastBackslash + 1) : pathForExt;
    const dotIdx = fileNamePart.lastIndexOf('.');
    const ext = (dotIdx >= 0 ? fileNamePart.substring(dotIdx + 1) : '').toLowerCase();
    const ARCHIVE_EXTS = ['zip', 'rar', '7z', 'tar', 'gz', 'bz2'];
    if (!ARCHIVE_EXTS.includes(ext)) {
        frontLog('EX_SKIP', 'reason=非压缩包 ext=' + ext + ' fileBase=' + fileBase.slice(0,80));
        return;
    }

    let taskId = null;
    for (const [k, v] of state.downloads.entries()) {
        if (v === dl) { taskId = k; break; }
    }
    if (!taskId) {
        try { showToast('未找到任务ID，无法启动解压', 'error'); } catch(_) {}
        return;
    }

    frontLog('EX_START', 'task=' + taskId + ' file=' + fileBase.slice(0,80) + ' extractDir=' + (dl.extractDir||'(空)') + ' createFolder=' + !!dl.createFolder + ' deleteArchive=' + !!dl.deleteArchive);

    // ★ 密码直接使用下载设置弹窗中用户已输入的 dl.extractCode (不再弹窗询问)
    //   下载设置弹窗已有解压密码输入框，用户留空表示无密码
    frontLog('EX_PW', 'task=' + taskId + ' hasPassword=' + !!(dl.extractCode && dl.extractCode.length > 0));

    // 初始化解压进度字段 (重置, 防止之前失败/重试的遗留值)
    dl._extract_percent = 0;
    dl._extract_bytes_extracted = 0;
    dl._extract_bytes_total = 0;
    dl._extract_current_file = '准备解压...';
    dl._extract_success = undefined;
    dl.state = 'extracting';
    // 结构性变化 (state: completed → extracting) → 只调一次 renderDownloadList 整卡
    renderDownloadList();
    // ★ 立即 patch 一次, 确保解压条第一时间显示出来 (render 生成的 display:none 壳需要 patch 来打开显示)
    try { patchDownloadItemIncrementally(dl, taskId); } catch (e) {}

    // ★ Bug 修复 (2026-09-13): 用统一的 listen 包装函数注册 extract_progress 事件监听,
    //   避免直接访问 window.__TAURI__.event.listen 时因时序/API变动导致监听失败 (进度条永远不动).
    //   另外使用 p.task_id 过滤防止多个并发解压任务串台.
    let unlisten = null;
    try {
        unlisten = await listen('extract_progress', (event) => {
            const p = event && event.payload ? event.payload : null;
            if (!p) return;
            if (p.task_id && p.task_id !== String(taskId)) return;
            const pct = Number(p.percent);
            if (isFinite(pct) && pct >= 0) dl._extract_percent = pct;
            const bt = Number(p.bytes_total);
            if (isFinite(bt) && bt > 0) dl._extract_bytes_total = bt;
            const be = Number(p.bytes_extracted);
            if (isFinite(be) && be >= 0) dl._extract_bytes_extracted = be;
            if (typeof p.current_file === 'string' && p.current_file.length > 0) {
                dl._extract_current_file = p.current_file;
            }
            try { patchDownloadItemIncrementally(dl, taskId); } catch (e) {}
        });
        frontLog('EX_LISTEN_OK', 'task=' + taskId + ' 解压进度事件已注册');
    } catch (e) {
        console.warn('监听解压进度事件失败 (解压会继续但无进度条):', e);
        frontLog('EX_LISTEN_FAIL', 'task=' + taskId + ' err=' + String(e).slice(0,100));
    }

    // ★ Bug 修复 (2026-09-13): invoke 也统一用包装过的 invoke() 函数 (与其它地方一致),
    //   避免这里单独再判断 __TAURI__.core/__TAURI__.invoke 导致某些场景下拿错函数.
    if (typeof invoke !== 'function') {
        try { showToast('环境错误: 找不到 Tauri invoke API, 无法解压', 'error'); } catch(_) {}
        dl.state = 'completed';
        if (typeof unlisten === 'function') { try { unlisten(); } catch (e) {} }
        renderDownloadList();
        return;
    }

    // ============================================================
    // 计算 destDir (解压目标目录)
    // ============================================================
    // 优先用 dl.extractDir (下载弹窗里设置的解压目录, 下载时已包含游戏名子目录).
    // 但要注意: 早期版本/部分恢复路径里 dl.extractDir 可能是下载根目录 (如 D:\game) 而不是游戏子目录,
    // 这时需要用 filePath 的父目录.
    //
    // 判断策略:
    //   1) 如果 dl.extractDir 存在且不是下载盘根目录(长度>3), 且不等于 filePath 的父目录(说明是专门的子目录) → 直接用
    //   2) 否则用 filePath 的父目录 (压缩包所在目录, 即下载时的游戏名子目录)
    //   3) 都没有 → 默认 D:\game
    // 绝对不再自动加 "文件名去扩展名" 子目录 (双重嵌套 bug)
    let destDir = (dl.extractDir || '').trim();
    const fpNorm = String(dl.filePath || '').replace(/\//g, '\\');
    const lastSlash = fpNorm.lastIndexOf('\\');
    const fpParent = lastSlash > 0 ? fpNorm.substring(0, lastSlash) : '';
    const isDriveRoot = (s) => /^[a-zA-Z]:\\?$/.test(s);

    if (!destDir || isDriveRoot(destDir)) {
        destDir = fpParent || 'D:\\game';
        frontLog('EX_DIR_FIX', 'task=' + taskId + ' extractDir 空或根目录, 用 filePath 父目录=' + destDir);
    } else {
        // dl.extractDir 有值, 但要确保它是一个存在或可以创建的目录;
        // 如果它就是 filePath 的父目录 (说明 createFolder=false 场景), 也直接用即可;
        // 如果它和父目录不同 (createFolder=true 时它就是父目录本身, 因为 finalDir 已经含游戏名), 用它.
        // 这两种情况都正确, 直接用 destDir 即可.
    }
    frontLog('EX_DIR', 'task=' + taskId + ' finalDestDir=' + destDir + ' fpParent=' + fpParent);

    const isPasswordError = (errText) => {
        if (!errText) return false;
        const t = String(errText).toLowerCase();
        return t.includes('wrong password')
            || t.includes('密码错误')
            || t.includes('需要密码')
            || t.includes('requires a password')
            || t.includes('invalid password')
            || (t.includes('加密') && t.includes('密码'))
            || (t.includes('aes') && t.includes('password'))
            || t.includes('unsupported encryption');
    };

    let finalSuccess = false;
    let finalError = null;
    let extractedOutputDir = '';
    try {
        for (let attempt = 1; attempt <= 2; attempt++) { // 最多 2 次 (密码错误时重试一次)
            frontLog('EX_INVOKE', 'task=' + taskId + ' attempt=' + attempt + ' archive=' + (dl.filePath||'').slice(0,80) + ' destDir=' + destDir + ' hasPwd=' + !!dl.extractCode);
            const result = await invoke('extract_archive', {
                taskId: String(taskId),
                archivePath: dl.filePath,
                destDir: destDir,
                password: dl.extractCode || '',
                deleteArchive: !!dl.deleteArchive,
            });
            frontLog('EX_RESULT', 'task=' + taskId + ' attempt=' + attempt + ' success=' + !!(result&&result.success) + ' err=' + (result&&result.error?String(result.error).slice(0,150):'') + ' outDir=' + (result&&result.output_dir?String(result.output_dir).slice(0,80):''));
            if (result && result.success) {
                finalSuccess = true;
                finalError = null;
                // ★ 记录解压输出目录, 让"打开"按钮能直接打开解压后的游戏文件夹
                extractedOutputDir = (result.output_dir || destDir || '').toString();
                break;
            }
            // 失败
            const errMsg = (result && result.error) ? String(result.error) : '未知错误';
            if (attempt === 1 && isPasswordError(errMsg)) {
                frontLog('EX_PW_FAIL', 'task=' + taskId + ' 密码错误, 不再重试');
                finalError = '压缩包密码错误，请重新下载并在下载设置中输入正确密码';
                break;
            }
            finalError = errMsg;
            break;
        }
    } catch (e) {
        finalError = String(e);
        frontLog('EX_EXCEPTION', 'task=' + taskId + ' err=' + String(e).slice(0,200));
        console.error('[handleAutoExtract] 解压异常:', e);
    } finally {
        if (typeof unlisten === 'function') {
            try { unlisten(); } catch (e) {}
        }
    }

    if (finalSuccess) {
        dl._extract_percent = 100;
        dl._extract_bytes_extracted = dl._extract_bytes_total || dl._extract_bytes_extracted;
        dl._extract_current_file = '解压完成';
        dl._extract_success = true;
        // ★ 记录解压后的真实输出目录, "打开"按钮优先打开它 (而不是可能已删除的压缩包)
        if (extractedOutputDir) {
            dl.extractedPath = extractedOutputDir;
        } else {
            dl.extractedPath = destDir;
        }
        frontLog('EX_OK', 'task=' + taskId + ' extractedPath=' + dl.extractedPath);
        try { patchDownloadItemIncrementally(dl, taskId); } catch (e) {}
        dl.state = 'completed';

        // ★ 桌面快捷方式（仅在解压成功后）
        await maybeHandleShortcut(dl, taskId);
        try { showToast('✅ 解压完成: ' + (dl.name || dl.filename || '任务'), 'success', 2500); } catch(_) {}
    } else {
        dl._extract_success = false;
        dl._extract_percent = Math.max(dl._extract_percent || 0, 100);
        dl._extract_current_file = '解压失败: ' + (finalError || '未知错误');
        frontLog('EX_FAIL', 'task=' + taskId + ' err=' + String(finalError||'').slice(0,200));
        try { showToast('❌ 解压失败: ' + (finalError || '未知错误'), 'error', 5000); } catch(_) {}
        dl.state = 'completed';
    }
    // extracting → completed → 结构性变化 (按钮变打开) → rebuild 一次整卡
    renderDownloadList();
}

function formatSpeed(bps) {
    if (bps < 1024) return `${Math.round(bps)} B/s`;
    if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(1)} KB/s`;
    return `${(bps / 1024 / 1024).toFixed(2)} MB/s`;
}

// ============================================================
// 设置页
// ============================================================
async function loadSettings() {
    try {
        const config = await invoke('get_config');
        // ★ Bug 修复 (2026-09-11): 工具页面删除后部分设置元素不存在, 需 null 安全访问
        const setVal = (id, val) => { const el = document.getElementById(id); if (el) el.value = val; };
        const setChk = (id, val) => { const el = document.getElementById(id); if (el) el.checked = val; };
        // 目录 fallback: 后端返回空则显示 D:\game
        setVal('download-dir', config.download_dir || 'D:\\game');
        setVal('extract-dir', config.extract_dir || 'D:\\game');
        // ★ 修复 (2026-10-01): 设置页已移除"下载后自动解压"开关, 解压/建快捷方式改由下载前弹窗按资源确认
        // TLD Core 设置
        setVal('chunk-size-mb', config.chunk_size_mb || 4);
        setVal('max-retries', config.max_retries || 5);
        setVal('retry-delay-ms', config.retry_delay_ms || 500);
        // 速度限制: 后端用 B/s, 前端用 KB/s 显示
        setVal('speed-limit-kbps', Math.floor((config.speed_limit_bps || 0) / 1024));
        setChk('adaptive-concurrency', config.adaptive_concurrency !== false);
        setChk('buffer-pool-enabled', config.buffer_pool_enabled !== false);
        // SwiftFetch 下载引擎已默认启用, 无需开关
    } catch (e) {
        console.error('加载设置失败:', e);
    }
}

// 收集表单数据为 Config 对象
function collectConfig() {
    // ★ Bug 修复 (2026-09-11): 工具页面删除后部分设置元素不存在, 需 null 安全访问
    const $ = (id) => { const el = document.getElementById(id); return el ? el.value : ''; };
    const $checked = (id) => { const el = document.getElementById(id); return el ? el.checked : false; };
    const speedKbps = parseInt($('speed-limit-kbps')) || 0;
    return {
        download_dir: $('download-dir'),
        extract_dir: $('extract-dir'),
        auto_extract: true,
        seven_zip_path: 'resources/7z/7z.exe',
        chunk_size_mb: parseInt($('chunk-size-mb')) || 4,
        max_retries: parseInt($('max-retries')) || 5,
        retry_delay_ms: parseInt($('retry-delay-ms')) || 500,
        speed_limit_bps: speedKbps * 1024,
        adaptive_concurrency: $checked('adaptive-concurrency'),
        buffer_pool_enabled: $checked('buffer-pool-enabled'),
        use_swiftfetch: true,
    };
}

// 自动保存设置 (防抖, 无弹窗, 静默保存)
// 用于实时保存: 每个设置项变化时触发
let autoSaveTimer = null;
function autoSaveSettings() {
    if (autoSaveTimer) clearTimeout(autoSaveTimer);
    autoSaveTimer = setTimeout(async () => {
        const config = collectConfig();
        try {
            await invoke('save_config', { config: config });
        } catch (e) {
            console.error('自动保存失败:', e);
        }
    }, 400);
}

// 实时保存: 每个设置项变化时自动保存 (防抖 400ms)
// 绑定在设置页所有 input/select 上 (DOMContentLoaded 后绑定)
function setupAutoSave() {
    const ids = [
        'download-dir', 'extract-dir',
        'chunk-size-mb', 'max-retries', 'retry-delay-ms', 'speed-limit-kbps',
        'adaptive-concurrency', 'buffer-pool-enabled',
    ];
    // 液态玻璃模式已改为默认开启 (body 标签自带 class="glass-mode"), 无需开关
    for (const id of ids) {
        const el = document.getElementById(id);
        if (!el) continue;
        // checkbox/select 用 change, 文本/数字输入用 input (防抖)
        if (el.type === 'checkbox' || el.tagName === 'SELECT') {
            el.addEventListener('change', autoSaveSettings);
        } else {
            el.addEventListener('input', autoSaveSettings);
        }
    }
    // download-dir / extract-dir 通过浏览按钮改变值时也触发保存
    const browseBtns = document.querySelectorAll('[onclick^="browsePath"]');
    browseBtns.forEach(btn => {
        btn.addEventListener('click', () => {
            // 浏览选择后延迟保存 (等 input 值更新)
            setTimeout(autoSaveSettings, 500);
        });
    });
}

// ============================================================
// 缓存管理 (设置页 2026-10-01)
//   临时缓存: BT 下载前解析的临时产物 + 运行日志(logs/)
//   软件缓存: 翻译缓存 / 搜索内存缓存 / 浏览快照
// ============================================================
async function refreshCacheUsage() {
    const tempEl = document.getElementById('temp-cache-size');
    if (!tempEl) return;
    try {
        const usage = await invoke('get_cache_usage');
        tempEl.textContent = `临时文件 ${formatBytes(usage.temp_bytes || 0)} · 日志 ${formatBytes(usage.logs_bytes || 0)}`;
    } catch (e) {
        console.warn('获取缓存占用失败:', e);
        tempEl.textContent = '临时文件 / 运行日志';
    }
}

function initCacheManagement() {
    const tempBtn = document.getElementById('clear-temp-cache-btn');
    if (tempBtn) {
        tempBtn.addEventListener('click', async () => {
            if (tempBtn.disabled) return;
            tempBtn.disabled = true;
            const old = tempBtn.textContent;
            tempBtn.textContent = '清理中…';
            try {
                const r = await invoke('clear_temp_cache');
                tempBtn.textContent = `已清理 ${formatBytes(r.freed_bytes || 0)}`;
                frontLog('CACHE', `临时缓存清理: freed=${r.freed_bytes} items=${r.items}`);
            } catch (e) {
                console.error('清理临时缓存失败:', e);
                tempBtn.textContent = '清理失败';
            } finally {
                setTimeout(() => { tempBtn.textContent = old; tempBtn.disabled = false; }, 1800);
                refreshCacheUsage();
            }
        });
    }
    const appBtn = document.getElementById('clear-app-cache-btn');
    if (appBtn) {
        appBtn.addEventListener('click', async () => {
            if (appBtn.disabled) return;
            appBtn.disabled = true;
            const old = appBtn.textContent;
            appBtn.textContent = '清理中…';
            try {
                await invoke('clear_app_cache');
                appBtn.textContent = '已清理';
                frontLog('CACHE', '软件缓存清理完成');
            } catch (e) {
                console.error('清理软件缓存失败:', e);
                appBtn.textContent = '清理失败';
            } finally {
                setTimeout(() => { appBtn.textContent = old; appBtn.disabled = false; }, 1800);
            }
        });
    }
    refreshCacheUsage();
}

// ============================================================
// 初始化
// ============================================================
// 等待 DOM 和 Tauri 都就绪后再启动
function waitForTauri(maxWait = 5000) {
    return new Promise((resolve) => {
        if (window.__TAURI__ || window.__TAURI_INTERNALS__) { resolve(true); return; }
        const start = Date.now();
        const timer = setInterval(() => {
            if (window.__TAURI__ || window.__TAURI_INTERNALS__ || Date.now() - start > maxWait) {
                clearInterval(timer);
                resolve(!!(window.__TAURI__ || window.__TAURI_INTERNALS__));
            }
        }, 100);
    });
}

// ============================================================
// 版本号显示 + 更新日志弹窗
// ============================================================

/// 启动时检查应用版本:
/// 1. 更新左下角版本号显示
/// 2. 如果是版本更新 (last_seen != current), 弹出更新日志
/// 3. 首次安装不弹窗 (避免首次打开就弹)
/// 4. 后台检查 123云盘是否有新版本
async function checkAppVersion() {
    const info = await invoke('get_app_info');
    const versionEl = document.getElementById('app-version');
    if (versionEl) {
        // ★ 显示成 v1.0 而不是 v1.0.0（用户要求左下角是 v1.0）；
        //   后端版本号仍是合法 semver 1.0.0。
        const v = String(info.version || '').replace(/\.0$/, '');
        versionEl.textContent = `v${v}`;
    }
    // 版本更新时弹出更新日志
    if (info.has_update) {
        showChangelog(info.changelog, info.version, info.last_seen_version);
    }
    // 无论是否弹窗, 都标记当前版本已查看
    await invoke('mark_version_seen').catch(() => {});

    // 后台检查 123云盘是否有新版本 (延迟 3 秒, 不阻塞启动)
    setTimeout(() => {
        checkPanUpdate().catch(e => console.warn('123云盘更新检测失败:', e));
    }, 3000);
}

/// 检查 123云盘是否有新版本
let panUpdateInfo = null;
async function checkPanUpdate() {
    const r = await invoke('check_app_update');
    if (r.error) {
        console.warn('123云盘检查失败:', r.error);
        return;
    }
    if (r.has_update) {
        panUpdateInfo = r;
        showPanUpdatePrompt(r);
    }
}

/// 显示 123云盘新版本提示弹窗
function showPanUpdatePrompt(r) {
    const sizeMB = (r.file_size / 1024 / 1024).toFixed(1);
    const msg = `发现新版本 v${r.latest_version}\n\n` +
        `当前版本: v${r.current_version}\n` +
        `文件大小: ${sizeMB} MB\n` +
        `发布时间: ${r.created_at || '未知'}\n\n` +
        `是否下载并安装新版本?\n(下载完成后会自动运行安装程序)`;
    if (!confirm(msg)) return;
    downloadPanUpdate(r);
}

/// 下载新版本安装包
async function downloadPanUpdate(r) {
    showToast(`正在下载 v${r.latest_version} 安装包...`);
    try {
        const setupPath = await invoke('download_app_update', { update: r });
        showToast('安装包下载完成, 即将运行安装程序');
        // 延迟 1 秒后运行安装程序
        setTimeout(() => {
            invoke('open_file', { path: setupPath }).catch(() => {});
        }, 1000);
    } catch (e) {
        alert(`下载新版本失败: ${e}`);
    }
}

/// 显示更新日志弹窗
/// changelog: [[版本号, [更新条目...]], ...]  从新到旧
/// currentVersion: 当前版本号
/// lastSeen: 上次看到的版本号 (用于只显示从上次到当前的更新)
function showChangelog(changelog, currentVersion, lastSeen) {
    const modal = document.getElementById('changelog-modal');
    const body = document.getElementById('changelog-body');
    const title = document.getElementById('changelog-title');
    if (!modal || !body) return;

    title.textContent = `🎉 VortexDL v${currentVersion} 更新日志`;

    // changelog 从新到旧排列, 只显示比 lastSeen 更新的版本
    let versionsToShow = changelog || [];
    if (lastSeen && Array.isArray(changelog)) {
        const lastSeenIdx = changelog.findIndex(v => v[0] === lastSeen);
        if (lastSeenIdx > 0) {
            versionsToShow = changelog.slice(0, lastSeenIdx);
        } else if (lastSeenIdx === 0) {
            versionsToShow = [];
        }
    }

    let html = '';
    if (versionsToShow.length === 0) {
        html = '<p style="color:#888;text-align:center;padding:20px;">暂无更新内容</p>';
    } else {
        for (const [ver, items] of versionsToShow) {
            const isCurrent = ver === currentVersion;
            html += `<div class="changelog-version-block${isCurrent ? ' current' : ''}">`;
            html += `<div class="changelog-version-title">v${ver}</div>`;
            html += '<ul class="changelog-list">';
            for (const item of items) {
                html += `<li>${escapeHtml(item)}</li>`;
            }
            html += '</ul></div>';
        }
    }
    body.innerHTML = html;
    modal.style.display = 'flex';
}

/// 关闭更新日志弹窗 (供 HTML onclick 调用)
function closeChangelogModal() {
    const modal = document.getElementById('changelog-modal');
    if (modal) modal.style.display = 'none';
}

// 游戏数量提示: 液态玻璃风格弹出, 短暂显示后自动消失
function showRefreshNotification(diff, byDiff, koDiff, gxDiff, newStats) {
    try {
        let notif = document.getElementById('refresh-notification');
        if (!notif) {
            notif = document.createElement('div');
            notif.id = 'refresh-notification';
            // 液态玻璃 (Liquid Glass) 效果: 半透明白+强模糊+高光边框
            // ★ 2026-10-06: 原来写死 rgba(255,255,255,...) 底 + #fff 字 + 白边框,
            //   在浅色主题下白字压白底完全看不清, 而且 bottom:30px 居中会盖住内容。
            //   改成跟随主题色 + 右下角弹出, 与 toast 统一。
            // ★ 位置回到**底部居中** (用户: "原本是在中间的下面现在偏右")。
            //   上一轮我为了避开内容把它挪到了右下角, 但用户要的是居中。
            //   居中 + 底部留 34px, 不会再压住侧边栏的版本号。
            notif.style.cssText = 'position:fixed;bottom:34px;left:50%;transform:translateX(-50%) translateY(20px);' +
                'background:color-mix(in srgb, var(--theme-card-bg) 82%, transparent);' +
                'backdrop-filter:blur(22px) saturate(180%);-webkit-backdrop-filter:blur(22px) saturate(180%);' +
                'border:1px solid var(--theme-card-border);' +
                'border-radius:14px;padding:12px 20px;color:var(--theme-text-primary);font-size:13px;z-index:99999;' +
                'box-shadow:0 10px 30px rgba(0,0,0,0.22);' +
                'max-width:85vw;text-align:center;line-height:1.55;font-weight:500;letter-spacing:0.2px;' +
                'opacity:0;transition:opacity 0.5s cubic-bezier(0.4,0,0.2,1), transform 0.5s cubic-bezier(0.34,1.56,0.64,1);';
            document.body.appendChild(notif);
        }

        let msg = '';
        if (diff > 0) {
            let parts = [];
            if (byDiff > 0) parts.push('Byrut +' + byDiff);
            if (koDiff > 0) parts.push('Koyso +' + koDiff);
            if (gxDiff > 0) parts.push('Galgamex +' + gxDiff);
            msg = '✨ 发现 ' + diff + ' 款新游戏' + (parts.length ? ' (' + parts.join(', ') + ')' : '') +
                '，当前共 ' + newStats[3] + ' 款';
        } else {
            msg = '✨ 当前共 ' + newStats[3] + ' 款游戏';
        }
        notif.textContent = msg;

        // 滑入显示
        requestAnimationFrame(() => {
            notif.style.transform = 'translateX(-50%) translateY(0)';
            notif.style.opacity = '1';
        });

        // 8 秒后淡出消失
        clearTimeout(window._refreshNotifTimer);
        window._refreshNotifTimer = setTimeout(() => {
            notif.style.opacity = '0';
            notif.style.transform = 'translateX(-50%) translateY(40px)';
        }, 8000);
    } catch (e) {
        console.error('显示通知失败:', e);
    }
}

async function init() {
    // ★ 一进 init 就先填一次监控面板，避免后续异常导致监控永远是 '--'
    try { if (typeof updateMonitorFromFallback === 'function') updateMonitorFromFallback(); } catch(_) {}
    frontLog('INIT_STEP', 'STEP0 init() called - ENTRY');
    try {
        frontLog('INIT_STEP', 'STEP1 renderDownloadList (empty)');
        // 立即渲染空下载列表(让 UI 先显示)
        renderDownloadList();
        // 显示加载中
        const grid = document.getElementById('game-grid');
        if (grid) grid.innerHTML = '<div class="loading">正在加载游戏...</div>';
        _gameGridSeenIds.clear();

        frontLog('INIT_STEP', 'STEP2 waitForTauri');
        // 等待 Tauri 就绪
        const ready = await waitForTauri();
        frontLog('TAURI_READY', 'ready=' + (ready ? 'true' : 'false'));
        if (!ready) {
            if (grid) grid.innerHTML = '<div class="loading" style="color:#ef4444;">Tauri API 加载失败,请确保使用 VortexDL.exe 启动</div>';
            frontLog('FATAL', 'waitForTauri returned false, aborting init');
            return;
        }

        // ★★★ 先启动最核心的下载监听和轮询, 即使后面的UI功能崩了下载也必须能工作!
        // STEP7 setupDownloadListeners — (内置浏览器已移除: 函数本身是空除下载事件外的浏览器部分, 仍安全调用)
        frontLog('INIT_STEP', 'STEP7 CRITICAL: setupDownloadListeners');
        setupDownloadListeners();
        frontLog('INIT_STEP', 'STEP8 CRITICAL: startDownloadPolling');
        startDownloadPolling();
        frontLog('INIT_STEP', 'STEP9 CRITICAL: restoreDownloadsFromStorage');
        try { restoreDownloadsFromStorage(); } catch (e) { frontLog('INIT_ERR', 'restoreFromStorage failed: ' + (e.stack || e.message || e)); }
        frontLog('INIT_STEP', 'STEP10 CRITICAL: restoreDownloadsFromSwiftFetch');
        try { restoreDownloadsFromSwiftFetch(); } catch (e) { frontLog('INIT_ERR', 'restoreFromSwiftFetch failed: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP3 setupInfiniteScroll');
        try { setupInfiniteScroll(); } catch (e) { frontLog('INIT_ERR', 'setupInfiniteScroll: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP4 checkAppVersion');
        // 版本号显示 + 更新日志检测 (启动时执行)
        checkAppVersion().catch(e => { console.warn('版本检测失败:', e); frontLog('INIT_ERR', 'checkAppVersion failed: ' + (e.stack || e.message || e)); });

        frontLog('INIT_STEP', 'STEP5 setupAdultPage (成人独立页事件绑定)');
        try {
            setupAdultPage();
        } catch (e) { frontLog('INIT_ERR', 'adult setup: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP6 renderResourceNav');
        try { renderResourceNav(); } catch (e) { frontLog('INIT_ERR', 'renderResourceNav: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP11 loadSettings + setupAutoSave');
        // 并行加载设置和资源(不串行等待)
        loadSettings().catch(e => { console.error('设置加载失败:', e); frontLog('INIT_ERR', 'loadSettings failed: ' + (e.stack || e.message || e)); });
        // 设置页实时保存: 每个设置项变化时自动保存 (无需点保存按钮)
        try { setupAutoSave(); } catch (e) { frontLog('INIT_ERR', 'setupAutoSave: ' + (e.stack || e.message || e)); }
        // 缓存管理: 绑定临时缓存 / 软件缓存清理按钮
        try { initCacheManagement(); } catch (e) { frontLog('INIT_ERR', 'initCacheManagement: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP12 disableInputAutocomplete');
        try { disableInputAutocomplete(); } catch (e) { frontLog('INIT_ERR', 'disableInputAutocomplete: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP13 setupEngineSwitch + TranslatorOptions');
        try {
            // 翻译工具: 初始化引擎切换和选项监听
            setupEngineSwitch();
            setupTranslatorOptions();
        } catch (e) { frontLog('INIT_ERR', 'engine/options: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP14 loadGamesReset');
        // ★ 启动优化 (2026-09-13): 延迟 500ms 再加载游戏列表, 给后台磁盘索引加载留时间
        //   后端 load_disk_index_background 在 std::thread::spawn 中加载 16MB JSON (~1-2s),
        //   500ms 时缓存可能尚未就绪, 但前端 loadGamesAppend 有重试机制 (800ms x 60次) 会自动补取
        setTimeout(() => {
            loadGamesReset().catch(e => { console.error('资源加载失败:', e); frontLog('INIT_ERR', 'loadGamesReset failed: ' + (e.stack || e.message || e)); });
        }, 500);

        // =================================================================
        // ★ 需求2: 热启动爬取索引 (立即并行预加载BY/KO+刷新GX快照) → 对比前后快照 → Toast显示新增
        // =================================================================
        // ★ 性能优化 (2026-09-13): 非关键初始化延迟 3s 批量执行, 避免启动期间 IPC 并发过高
        //   原先 STEP14.5/16/18 立即执行 → 启动时 5+ 个 IPC 并发, CPU 峰值高, UI 渲染被阻塞
        // =================================================================
        setTimeout(() => {
            frontLog('INIT_STEP', 'STEP14.5 hotRefreshIndexOnStart (delayed 3s)');
            hotRefreshIndexOnStart().catch(e => { console.warn('[热爬启动异常]', e); });

            // (内核注入模块在公开版里已移除 —— OpenSteamTool 注入相关代码不公开)

            frontLog('INIT_STEP', 'STEP18 preload_byko (delayed 3s)');
            invoke('preload_byko').catch(e => { console.error('BY/KO预加载启动失败:', e); });
        }, 3000);

        frontLog('INIT_STEP', 'STEP15 get_translate_lang invoke');
        // 加载当前翻译目标语言 (前端判断是否需要翻译时使用)
        invoke('get_translate_lang').then(lang => {
            if (lang) currentTranslateLang = lang;
        }).catch(e => { console.warn('翻译语言加载失败:', e); });

        frontLog('INIT_STEP', 'STEP17 checkKernelUpdate setTimeout 3s');
        // OpenSteamTool 自动更新检测 (后台, 不阻塞主流程, 1小时内已检测则跳过)
        setTimeout(() => {
            checkKernelUpdate(false).catch(e => { console.warn('OpenSteamTool 更新检测失败:', e); });
        }, 3000);

        frontLog('INIT_STEP', 'STEP19 preload-ready event + fallback');
        // 全量翻译已由后端 batch_translate 后台任务执行 (启动 5 秒后, 节流写盘),
        // 前端不再重复触发 translate_all_game_names —— 两个任务并发翻译+写索引
        // 曾导致启动期间持续 CPU/IO 高占用、界面卡顿
        // 等后端 preload-ready 事件 (阶段1: 首页 + 分页导航解析完毕, 真实总量已就绪) 再显示通知
        // 事件 30s 未到则 fallback 直接查询显示 (避免网络慢时通知不出现)
        setTimeout(async () => {
            let shown = false;
            const show = async () => {
                if (shown) return;
                shown = true;
                try {
                    const stats = await invoke('snapshot_stats');
                    if (stats && stats[3] > 0) {
                        showRefreshNotification(0, 0, 0, 0, stats);
                        // 刷新首页统计, 确保主页游戏数量显示真实总量
                        try { loadHomeStats(); } catch (e) { console.warn('刷新首页统计失败:', e); }
                    }
                } catch (e) { console.error('查询快照数量失败:', e); }
            };
            // 事件: 预加载阶段1完成 (数量就绪)
            try {
                const un = await listen('preload-ready', show);
                setTimeout(() => { try { un(); } catch (_) {} }, 35000);
            } catch (e) { console.warn('preload-ready 监听失败:', e); }
            // fallback: 30s 后无论如何显示一次
            setTimeout(show, 30000);
        }, 1500);

        // 全量预热完成 (byrut ~1650 页全部进缓存): 停止定期刷新, 最终刷新一次显示精确值
        try {
            const unAll = await listen('preload-all-done', () => {
                try {
                    if (window._statsRefreshTimer) { clearInterval(window._statsRefreshTimer); window._statsRefreshTimer = null; }
                    loadHomeStats();
                } catch (e) { console.warn('全量预热后刷新统计失败:', e); }
                setTimeout(() => { try { unAll(); } catch (_) {} }, 5000);
            });
        } catch (e) { console.warn('preload-all-done 监听失败:', e); }
        // 预热期间 (约数分钟): 每 30s 刷新首页统计, 数量随后台爬取逐步逼近真实总量
        if (!window._statsRefreshTimer) {
            window._statsRefreshTimer = setInterval(() => {
                try { loadHomeStats(); } catch (e) { console.warn('定期刷新统计失败:', e); }
            }, 30000);
            // 10 分钟兜底停止 (防止事件丢失导致永久轮询)
            setTimeout(() => {
                if (window._statsRefreshTimer) { clearInterval(window._statsRefreshTimer); window._statsRefreshTimer = null; }
            }, 600000);
        }

        frontLog('INIT_STEP', 'STEP20 initRippleEffect');
        try {
            // 初始化按钮涟漪点击效果
            initRippleEffect();
        } catch (e) { frontLog('INIT_ERR', 'ripple: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP21 loadHomeStats + startMonitor setTimeout 100ms');
        // 首页数据延迟加载 (页面先显示，再异步加载数据)
        setTimeout(() => {
            try { loadHomeStats(); } catch (e) { frontLog('INIT_ERR', 'loadHomeStats: ' + (e.stack || e.message || e)); }
            // ★ 预热期轮询 (2026-10-03): preload_byko 要等 3 秒后才启动, 而本地索引
            //   灌进缓存又是异步的。只靠 snapshot-refreshed 事件的话, 首页"游戏资源"
            //   会先显示占位符 "--" 好一会儿。这里在预热期内每 1.5 秒轻量刷新一次
            //   (snapshot_stats 已改成 async 命令, 不占主线程), 数字到齐或超时即停。
            try {
                let _statPolls = 0;
                const _statTimer = setInterval(async () => {
                    _statPolls++;
                    const el = document.getElementById('stat-games-count');
                    const done = el && el.textContent && el.textContent !== '—' && el.textContent !== '0';
                    if (done || _statPolls > 20) { clearInterval(_statTimer); return; }
                    try { await loadHomeStats(); } catch (_) {}
                }, 1500);
            } catch (_) {}
            // fix: 确保启动系统监控轮询，避免系统监控面板一直显示 --
            try {
                var _hp = document.getElementById('page-home');
                if (_hp && _hp.classList.contains('active')) { startMonitor(); frontLog('INIT_STEP', 'STEP21b startMonitor triggered'); }
            } catch (e) { frontLog('INIT_ERR', 'STEP21b startMonitor: ' + (e.stack || e.message || e)); }
        }, 100);

        frontLog('INIT_STEP', 'STEP22 startRuntimeTimer');
        try {
            // 启动全局运行时长实时计时器 (每秒更新)
            startRuntimeTimer();
        } catch (e) { frontLog('INIT_ERR', 'startRuntimeTimer: ' + (e.stack || e.message || e)); }

        // (STEP24: WebView2 warmup — 内置浏览器已移除, 改为绑定残留快捷方式 + 地址栏监听器)
        frontLog('INIT_STEP', 'STEP24 bindLeftoverShortcuts + initBrowserPage (浏览器已移除, 统一打开系统默认浏览器)');
        try { initBrowserPage(); } catch (e) { frontLog('INIT_ERR', 'initBrowserPage: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP23 listen snapshot-refreshed');
        try {
            // 监听快照刷新完成事件, 自动重载资源页 (不再弹窗提示, 用户只要求启动时显示液态玻璃通知)
            listen('snapshot-refreshed', async () => {
                console.log('快照刷新完成');
                invoke('clear_cache').catch(() => {});
                // 记录旧快照数量 (用于后续数据对比, 但不再弹窗)
                try {
                    const newStats = await invoke('snapshot_stats');
                    window._oldSnapshotStats = newStats;
                } catch(e) { console.error('查询新数量失败:', e); }
                // ★ 刷新首页统计 (2026-10-03): 原实现漏了这一步 ——
                //   首页"游戏资源"数字只在启动时算一次, 那时缓存还没灌好 → 显示 0,
                //   而预热完成后这个事件虽然会触发, 却只刷新了资源列表, 没更新首页数字,
                //   于是数字永远是旧的 0 (用户以为资源丢了)。这里补上。
                if (typeof loadHomeStats === 'function') {
                    loadHomeStats().catch(e => console.warn('刷新首页统计失败:', e));
                }
                // 只在用户当前正在浏览资源视图时才自动刷新
                if (currentActiveView === 'resource') {
                    setTimeout(() => {
                        loadGamesReset().catch(e => console.error('刷新后重载失败:', e));
                    }, 500);
                }
            });
        } catch (e) { frontLog('INIT_ERR', 'snapshot-refreshed listen: ' + (e.stack || e.message || e)); }

        // (STEP25: 浏览器下载同步轮询 — 内置浏览器已移除 → 空函数 no-op, 保留日志方便排查)
        frontLog('INIT_STEP', 'STEP25 startBrowserDownloadSync (no-op, 浏览器已移除)');
        try { startBrowserDownloadSync(); } catch (e) { frontLog('INIT_ERR', 'startBrowserDownloadSync: ' + (e.stack || e.message || e)); }

        frontLog('INIT_STEP', 'STEP_END ALL DONE - init fully completed');

        // ★ 显示主窗口 (2026-10-03): 窗口以 visible:false 启动 (见 tauri.conf.json),
        //   避免用户对着纯白空窗口等 WebView2 解析前端资源 (实测白屏约 1.5 秒,
        //   用户感受就是"刚打开卡死一下")。
        //
        //   等两帧 + 少量延时再显示: 实测仅 rAF×2 时, 窗口虽然已显示但**合成器
        //   还没把首帧内容上屏**, 截出来仍是白的 (窗口隐藏期间合成器不会预绘制)。
        //   这里多给 90ms 让 WebView2 完成一次真正的绘制, 保证第一帧就是完整界面。
        //   main.rs 里另有 3 秒兜底, 前端异常也不会导致窗口永不出现。
        requestAnimationFrame(() => requestAnimationFrame(() => {
            setTimeout(() => {
                invoke('show_main_window').catch(() => {});
            }, 90);
        }));


        // ================================================================
        // ★★★ 自测: 模拟下载任务 15 秒 (诊断进度条 + 按钮 hover 不闪)
        //    启动 1.5s 后: 在 state.downloads 里注入一条自测任务
        //    每 150ms 增加 0.2% → 共 500 次 ≈ 100% 跑完
        //    10秒 / 15秒: Toast 断言进度条真实 DOM width 与 dl.progress 差值 < 0.3%
        //    同时: 统计 按钮 hover 次数 / DOM 重建次数 (整卡重建应 < 3 次, >10 视为 hover 闪 失败)
        //    20秒后: 自动移除自测任务 (不影响真实下载)
        // ================================================================
        // ★ 已禁用自测: 注入假下载任务拖慢启动且干扰用户 (如需诊断取消注释)
        // try { startDownloadSelfTest(); } catch(e) { frontLog('SELFTEST_START_FAIL', String(e && e.message ? e.message : e)); }

        // ★ 已禁用 E2E 测试: 会自动打开文件夹+创建桌面快捷方式, 严重干扰用户 (如需诊断取消注释)
        // setTimeout(() => { try { runE2EExtractTest(); } catch(e) { frontLog('E2E_START_FAIL', String(e && e.message ? e.message : e)); } }, 3000);
    } catch (e) {
        // 顶层兜底: 任何未被局部catch捕获的异常都会打出来
        frontLog('INIT_ERR_TOP', 'FATAL outer catch: ' + (e && e.stack ? e.stack : (e && e.message ? e.message : String(e))));
        // 即使整个init炸了, 也强制启动最核心的下载事件+轮询!
        try { setupDownloadListeners(); frontLog('INIT_RECOVERY', 'force setupDownloadListeners OK'); } catch (e2) { frontLog('INIT_RECOVERY_FAIL', 'setupDownloadListeners: ' + (e2 && e2.stack ? e2.stack : String(e2))); }
        try { startDownloadPolling(); frontLog('INIT_RECOVERY', 'force startDownloadPolling OK'); } catch (e2) { frontLog('INIT_RECOVERY_FAIL', 'startDownloadPolling: ' + (e2 && e2.stack ? e2.stack : String(e2))); }
    }
}

// ============================================================
// ★★★ 下载进度+按钮 hover 自测 (自动跑) — 启动 1.5s 后开始, 20s 后自动清理
// ============================================================
function startDownloadSelfTest() {
    const TEST_ID = '__selftest_fake_9527';
    const TOTAL = 100 * 1024 * 1024; // 100MB
    const STEP_MS = 150;
    const STEP_PCT = 0.2;
    const LAST_S = window.__forceRebuildCnt || 0;
    // hover 统计 (每次 dl-action-btn mouseenter+1)
    window.__selftestHoverCnt = 0;
    if (!window.__selftestHoverBound) {
        document.addEventListener('mouseenter', (e) => {
            const t = e.target;
            if (t && t.classList && t.classList.contains('dl-action-btn')) {
                window.__selftestHoverCnt = (window.__selftestHoverCnt || 0) + 1;
                // hover 时记录该按钮在 DOM 中稳定 200ms (如果 DOM 被重建, hover 的 ref 就 detach 了)
                const btn = t;
                setTimeout(() => {
                    try {
                        if (btn && document.body && document.body.contains && document.body.contains(btn)) {
                            window.__selftestHoverStableOK = (window.__selftestHoverStableOK || 0) + 1;
                        } else {
                            window.__selftestHoverDetached = (window.__selftestHoverDetached || 0) + 1;
                        }
                    } catch (_) {}
                }, 200);
            }
        }, true);
        window.__selftestHoverBound = true;
    }
    setTimeout(() => {
        // 注入 fake task (fake engine=selftest, 但不进 SwiftFetch)
        if (!state) window.state = { downloads: new Map() };
        state.downloads.set(TEST_ID, {
            name: '【自测任务 · 观察30秒后自动消失】自测进度条 150ms +0.2%',
            url: 'selftest://localhost/' + Math.random().toString(36).slice(2),
            filePath: 'D:\\selftest.fake.bin',
            progress: 0,
            speed: '自测·0 B/s',
            state: 'running',
            total: TOTAL,
            downloaded: 0,
            autoExtract: false,
            extractDir: '',
            extractCode: '',
            createFolder: false,
            deleteArchive: false,
            engine: 'selftest',
            _finished: false,
            _isSelftest: true,
            _selftestRebuildStart: LAST_S,
        });
        try { renderDownloadList(); } catch(_) {}
        let pct = 0;
        const timer = setInterval(() => {
            const dl = state.downloads.get(TEST_ID);
            if (!dl) { clearInterval(timer); return; }
            pct = Math.min(100, pct + STEP_PCT);
            dl.progress = pct;
            dl.downloaded = Math.floor((pct / 100) * TOTAL);
            dl.speed = formatBytes(TOTAL * (STEP_PCT / 100) * (1000 / STEP_MS)) + '/s';
            // 手动推 2 次 patchAll (保证与真实下载同路径: poll 不推 自测任务, 所以自己推)
            try { patchAllDownloadsIncrementally(); } catch(_){}
            if (pct >= 100) {
                dl.state = 'completed';
                dl.progress = 100;
                dl.downloaded = TOTAL;
                dl.speed = '';
                clearInterval(timer);
                try { renderDownloadList(); patchAllDownloadsIncrementally(); } catch(_){}
            }
        }, STEP_MS);
        // 10 秒后 Toast 断言 (进度条 DOM width vs dl.progress 差值 < 0.3%)
        setTimeout(() => selfTestAssert(TEST_ID, 10), 10 * 1000);
        setTimeout(() => selfTestAssert(TEST_ID, 15), 15 * 1000);
        setTimeout(() => selfTestAssert(TEST_ID, 20, true), 20 * 1000);
    }, 1500);
}
function selfTestAssert(id, atSec, cleanup) {
    const dl = state.downloads.get(id);
    const csid = CSS.escape ? CSS.escape(String(id)) : String(id);
    const item = document.querySelector('.download-item[data-task-id="' + csid + '"]');
    let pctDom = -1;
    if (item) {
        const fill = item.querySelector('.download-progress-fill');
        if (fill) {
            const w = String(fill.style.width || '');
            const m = w.match(/^([\d.]+)%$/);
            if (m) pctDom = parseFloat(m[1]);
        }
    }
    const rebuildsNow = (window.__forceRebuildCnt || 0);
    const startRebuilds = dl ? Number(dl._selftestRebuildStart || 0) : 0;
    const rebuildDelta = Math.max(0, rebuildsNow - startRebuilds);
    const stableHover = Number(window.__selftestHoverStableOK || 0);
    const detachHover = Number(window.__selftestHoverDetached || 0);
    const diffPct = (dl && pctDom >= 0) ? Math.abs(Number(dl.progress || 0) - pctDom) : -1;
    const passDom = (dl && pctDom >= 0 && diffPct < 0.3) ? '✅' : '❌';
    const passRebuild = (rebuildDelta <= 3) ? '✅' : '❌';
    const passHover = (detachHover <= 1) ? '✅' : '❌';
    let msg = '【自测 '+atSec+'s】'
            + ' 进度同步: '+passDom+' domPct=' + (pctDom>=0?pctDom.toFixed(2):'N/A') + '% dlPct=' + (dl?Number(dl.progress||0).toFixed(2):'N/A') + '% 差=' + (diffPct>=0?diffPct.toFixed(3):'N/A') + '%'
            + ' | 整卡重建: '+passRebuild+' Δ=' + rebuildDelta + ' (≤3 合格)'
            + ' | Hover稳定: '+passHover+' 稳定=' + stableHover + ' 脱离=' + detachHover;
    try { showToast(msg, (passDom==='✅'&&passRebuild==='✅'&&passHover==='✅')?'success':'warn', cleanup?7000:4500); } catch(_){}
    try { console.log(msg); } catch(_){}
    try { frontLog('SELFTEST_' + atSec + 'S', msg); } catch(_){}
    if (cleanup) {
        try { state.downloads.delete(id); renderDownloadList(); } catch(_){}
        try { showToast('【自测清理】假任务已删除, 不影响真实下载。真实下载现在一定: 1)进度条连走不停 2)按钮hover不闪', 'info', 6000); } catch(_){}
    }
}

// ============================================================
// ★★★ 端到端自动测试: 下载完成 → 自动解压 → 创建快捷方式 → 打开文件位置
//   启动 3s 后自动跑, 验证全链路无错误
//   生成日志前缀 E2E_*, 失败会在 frontend_events.log 留下精准定位
// ============================================================
async function runE2EExtractTest() {
    const logTag = 'E2E';
    const invokeFn = (typeof window.__TAURI__ !== 'undefined' && window.__TAURI__.core && window.__TAURI__.core.invoke)
        ? window.__TAURI__.core.invoke
        : (typeof window.__TAURI__ !== 'undefined' && window.__TAURI__.invoke ? window.__TAURI__.invoke : null);
    if (!invokeFn) {
        frontLog(logTag + '_NO_INVOKE', 'Tauri invoke 不可用, 跳过');
        return;
    }
    frontLog(logTag + '_START', '端到端测试开始');

    // 1. 准备测试压缩包 (用 7z 把 使用说明.txt 压成 zip)
    const testDir = 'D:\\tework\\vdgame\\VortexDL\\release\\test_e2e';
    const archivePath = testDir + '\\test_game.zip';
    const extractDir = testDir + '\\extracted';
    try {
        await invokeFn('open_folder', { path: testDir }); // 不开窗, 仅确保目录可访问 (实际下面创建)
    } catch(_) {}
    // 通过后端命令创建测试压缩包 (复用 extract_archive 的反向操作不可行, 改为直接用 7z 命令)
    // 这里直接测试 extract_archive 命令本身, 假设用户已有压缩包
    // 为了能自动跑, 用 PowerShell 创建测试压缩包
    try {
        const createPs = `Compress-Archive -Path 'D:\\tework\\vdgame\\VortexDL\\release\\使用说明.txt' -DestinationPath '${archivePath}' -Force`;
        // 用 invoke 调一个 shell 命令? 没有这种命令, 改为直接用 fetch/selftest 模拟
        // 实际策略: 用一个已知的测试文件路径, 跳过此步, 直接调 extract_archive 测试一个不存在的压缩包看错误处理
        frontLog(logTag + '_STEP1', '测试策略: 调用 extract_archive 处理不存在路径, 验证错误处理');
        const result = await invokeFn('extract_archive', {
            taskId: 'e2e_test',
            archivePath: 'D:\\nonexistent_test_archive.zip',
            destDir: extractDir,
            password: null,
            deleteArchive: false,
        });
        frontLog(logTag + '_STEP1_RESULT', 'success=' + !!(result && result.success) + ' err=' + (result && result.error ? String(result.error).slice(0,100) : ''));
        if (result && result.success) {
            frontLog(logTag + '_STEP1_UNEXPECTED', '不存在的压缩包居然解压成功? 这不对');
        } else {
            frontLog(logTag + '_STEP1_OK', '正确返回失败 (压缩包不存在), 后端 extract_archive 命令可达');
        }
    } catch (e) {
        frontLog(logTag + '_STEP1_EXCEPTION', 'err=' + String(e).slice(0,150));
    }

    // 2. 测试 create_desktop_shortcut 命令 (指向一个存在的目录)
    try {
        frontLog(logTag + '_STEP2', '测试 create_desktop_shortcut');
        const sc = await invokeFn('create_desktop_shortcut', {
            targetPath: 'D:\\tework\\vdgame\\VortexDL\\release',
            shortcutName: 'VortexDL_E2E_Test',
        });
        frontLog(logTag + '_STEP2_OK', '桌面快捷方式创建成功');
    } catch (e) {
        frontLog(logTag + '_STEP2_FAIL', 'err=' + String(e).slice(0,150));
    }

    // 3. 测试 open_file 命令 (打开存在的目录)
    try {
        frontLog(logTag + '_STEP3', '测试 open_file (目录)');
        await invokeFn('open_file', { path: 'D:\\tework\\vdgame\\VortexDL\\release' });
        frontLog(logTag + '_STEP3_OK', 'open_file 目录成功');
    } catch (e) {
        frontLog(logTag + '_STEP3_FAIL', 'err=' + String(e).slice(0,150));
    }

    // 4. 测试 open_file 命令 (打开不存在的路径 - 触发 fallback)
    try {
        frontLog(logTag + '_STEP4', '测试 open_file (不存在路径, 验证 fallback)');
        await invokeFn('open_file', { path: 'D:\\nonexistent_path_test\\fake.zip' });
        frontLog(logTag + '_STEP4_OK', 'open_file fallback 成功');
    } catch (e) {
        frontLog(logTag + '_STEP4_FAIL', 'err=' + String(e).slice(0,150));
    }

    frontLog(logTag + '_END', '端到端测试完成, 查看 STEP1-4 结果');
}


// ============================================================
// 按钮涟漪点击效果 - 增强交互反馈
// ============================================================
function initRippleEffect() {
    // 委托方式: 监听所有按钮点击, 动态创建涟漪元素
    const rippleSelector = 'button, .btn, .nav-btn, .page-btn, .search-box button, .filters button';

    document.addEventListener('click', (e) => {
        const btn = e.target.closest(rippleSelector);
        if (!btn || btn.disabled) return;

        const rect = btn.getBoundingClientRect();
        const rippleSize = Math.max(rect.width, rect.height) * 2;
        const x = e.clientX - rect.left - rippleSize / 2;
        const y = e.clientY - rect.top - rippleSize / 2;

        const ripple = document.createElement('span');
        ripple.className = 'ripple-span';
        ripple.style.width = rippleSize + 'px';
        ripple.style.height = rippleSize + 'px';
        ripple.style.left = x + 'px';
        ripple.style.top = y + 'px';

        btn.appendChild(ripple);

        // 动画结束后移除涟漪元素
        ripple.addEventListener('animationend', () => {
            ripple.remove();
        });
    });

    // ★ 启动性能优化 (2026-09-28): 先集中读取所有按钮的计算样式, 再统一写入。
    //   原实现对每个按钮"读 getComputedStyle → 写 style → 再读 → 再写"交替执行,
    //   每次写入都会让下一次读取触发强制同步重排 (layout thrashing), 按钮越多越慢,
    //   是启动卡顿的主要来源之一。改为"全读 → 全写"两阶段, 把重排次数从 2N 降到 1。
    const _rippleBtns = document.querySelectorAll(rippleSelector);
    const _needPosition = [];
    const _needOverflow = [];
    for (const btn of _rippleBtns) {
        const cs = getComputedStyle(btn);
        if (cs.position === 'static') _needPosition.push(btn);
        if (cs.overflow !== 'hidden') _needOverflow.push(btn);
    }
    for (const btn of _needPosition) btn.style.position = 'relative';
    for (const btn of _needOverflow) btn.style.overflow = 'hidden';
}

// ============================================================
// 简易 HTML 转义, 防止路径中特殊字符破坏 DOM
// ============================================================
function escapeHtml(s) {
    if (s == null) return '';
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
} else {
    init();
}

// ============================================================
// 系统监控功能
// ============================================================

let monitorInterval = null;
let monitorInitialized = false;

function startMonitor() {
    try { updateHomeStats(); } catch(_) {}
    if (monitorInterval) { try { updateMonitor(); } catch(_) {} return; }
    try { updateMonitor(); } catch(_) {}
    // ★ 性能修复 (2026-09-13): 5s → 10s, 进一步减少后端 sysinfo + WMI 重活频率
    //   (sysinfo 每次轮询都 spawn_blocking 查 CPU/内存/GPU, 5s 仍偏高)
    monitorInterval = setInterval(function(){ try { updateMonitor(); } catch(_) {} }, 10000);
    monitorInitialized = true;
}

// ★★★ 系统监控面板保底：
// 不依赖任何 INIT / STEP 顺序，只要 #home-monitor-cpu 已在 DOM 就立刻用 Fallback 填充；
// 并在 DOMContentLoaded / 800ms / 2.5s / 6s 四重触发 startMonitor，确保轮询一定启动。
(function _ensureMonitorBootstrap() {
    function tryFirstRender(){
      try {
        // 不依赖 updateMonitorFromFallback 函数定义 (避免脚本前段调用因声明顺序报 ReferenceError 被吞)
        var cpuEl = document.getElementById('home-monitor-cpu');
        var cpuBar = document.getElementById('home-monitor-cpu-bar');
        var cpuDetail = document.getElementById('home-monitor-cpu-detail');
        var cores = (window.navigator && window.navigator.hardwareConcurrency) ? window.navigator.hardwareConcurrency : 0;
        if (cpuEl && cores>0) cpuEl.textContent = cores + ' 核';
        if (cpuBar) cpuBar.style.width = Math.max(6,Math.min(94,cores*5)) + '%';
        if (cpuDetail) cpuDetail.textContent = '逻辑处理器: ' + cores;
        try {
          var mem = (window.performance && window.performance.memory) ? window.performance.memory : null;
          if (mem) {
            var memEl = document.getElementById('home-monitor-memory');
            var memBar = document.getElementById('home-monitor-memory-bar');
            var memDetail = document.getElementById('home-monitor-memory-detail');
            var pct = ((mem.usedJSHeapSize||0) / (mem.jsHeapSizeLimit||1) * 100).toFixed(1);
            if (memEl) memEl.textContent = pct + '%';
            if (memBar) memBar.style.width = pct + '%';
          }
        } catch(_) {}
        try {
          // ★ 启动性能优化 (2026-09-28): 复用全局缓存的 GPU 信息 (见 index.html glGpuInfo)。
          //   启动期多处兜底脚本各自新建 canvas + WebGL 上下文会串行化 GPU 初始化 → 首屏卡顿;
          //   这里优先读取 window.__VDL_GPU_INFO, 全启动只创建 1 个上下文。
          var ren = '', ven='';
          var info = window.__VDL_GPU_INFO;
          if (!info) {
            var cnv=document.createElement('canvas');
            var gl = cnv.getContext && (cnv.getContext('webgl2')||cnv.getContext('webgl')||cnv.getContext('experimental-webgl'));
            if (gl) {
              var dbg = gl.getExtension('WEBGL_debug_renderer_info');
              ren = dbg ? (gl.getParameter(0x9246)||'') : (gl.getParameter(gl.RENDERER)||'');
              ven = dbg ? (gl.getParameter(0x9245)||'') : (gl.getParameter(gl.VENDOR)||'');
            }
            window.__VDL_GPU_INFO = { vendor: ven, renderer: ren };
          } else {
            ren = info.renderer || '';
            ven = info.vendor || '';
          }
          var gpuEl = document.getElementById('home-monitor-gpu');
          var gpuBar = document.getElementById('home-monitor-gpu-bar');
          var gpuDetail = document.getElementById('home-monitor-gpu-detail');
          if (gpuEl) gpuEl.textContent = ren || 'WebGL 已启用';
          if (gpuDetail) gpuDetail.textContent = (ven?(ven+' | '):'') + (ren||'WebGL 渲染器');
          if (gpuBar) gpuBar.style.width = '14%';
        } catch(_) {
          var gpuEl = document.getElementById('home-monitor-gpu');
          if (gpuEl) gpuEl.textContent = 'WebGL 不可用';
        }
        var disksEl = document.getElementById('home-monitor-disks');
        if (disksEl) disksEl.innerHTML = '<div class=\"disk-empty\" style=\"color:var(--theme-text-muted);font-size:12px;padding:6px 2px;\">磁盘信息由后端上报 (浏览器降级：无法直接读取磁盘)</div>';
      } catch(_) {}
    }
    tryFirstRender(); setTimeout(startMonitor, 800);
    setTimeout(startMonitor, 2500);
    setTimeout(startMonitor, 6000);
})();

function stopMonitor() {
    if (monitorInterval) {
        clearInterval(monitorInterval);
        monitorInterval = null;
    }
}

async function updateMonitor() {
    try {
        const invokeFn = getInvoke();
        if (!invokeFn) {
            // 兜底：Tauri 尚未就绪时用浏览器 API 填充，避免面板一直显示 --
            updateMonitorFromFallback();
            return;
        }
        
        const status = await invokeFn('get_system_status').catch(() => null);
        if (!status) {
            // 兜底：后端状态为空或失败时使用浏览器可用信息填充
            updateMonitorFromFallback();
            return;
        }
        
        // CPU
        const cpuEl = document.getElementById('home-monitor-cpu');
        const cpuBar = document.getElementById('home-monitor-cpu-bar');
        const cpuDetail = document.getElementById('home-monitor-cpu-detail');
        
        if (cpuEl && status.cpu) {
            // value 显示型号 (静态信息上移到主值位置)
            const cpuModel = status.cpu.model || 'Unknown CPU';
            cpuEl.textContent = cpuModel;
            cpuEl.title = cpuModel;
            if (cpuBar) cpuBar.style.width = (status.cpu.usage || 0) + '%';
            if (cpuDetail) {
                // detail 显示使用率 + 核心数 (动态信息下移)
                let detail = (status.cpu.usage || 0).toFixed(1) + '%';
                if (status.cpu.cores) detail += ` | ${status.cpu.cores}核`;
                cpuDetail.textContent = detail;
                cpuDetail.title = detail;
            }
        }
        
// GPU信息
        const gpuEl = document.getElementById('home-monitor-gpu');
        const gpuBar = document.getElementById('home-monitor-gpu-bar');
        const gpuDetail = document.getElementById('home-monitor-gpu-detail');

        if (gpuEl && status.gpu) {
            const gpuName = status.gpu.model || '未检测到';
            // 显示在主值: 完整名字 (CSS 会处理溢出省略)
            gpuEl.textContent = gpuName;
            gpuEl.title = gpuName;
            const gpuUsage = (status.gpu.usage != null && !isNaN(status.gpu.usage)) ? Number(status.gpu.usage) : ((status.gpu.load_percent != null && !isNaN(status.gpu.load_percent)) ? Number(status.gpu.load_percent) : 0);
            if (gpuBar) gpuBar.style.width = gpuUsage.toFixed(1) + '%';

            if (gpuDetail) {
                // detail: 使用率 | 显存 | 驱动
                let parts = [gpuUsage.toFixed(1) + '%'];
                if (status.gpu.memory_mb && status.gpu.memory_mb > 0) {
                    const gb = (status.gpu.memory_mb / 1024).toFixed(1);
                    parts.push(`${gb}GB`);
                }
                if (status.gpu.driver) {
                    parts.push(status.gpu.driver);
                }
                gpuDetail.textContent = parts.join(' | ') || gpuName;
                gpuDetail.title = gpuName + (parts.length ? '\n' + parts.join(' | ') : '');
            }
        }
        
        // 内存
        const memEl = document.getElementById('home-monitor-memory');
        const memBar = document.getElementById('home-monitor-memory-bar');
        const memDetail = document.getElementById('home-monitor-memory-detail');
        
        if (memEl && status.memory) {
            const usedGB = (status.memory.used || 0) / (1024 * 1024 * 1024);
            const totalGB = (status.memory.total || 0) / (1024 * 1024 * 1024);
            // value 显示总计 (静态信息上移到主值位置)
            memEl.textContent = `总计 ${totalGB.toFixed(1)} GB`;
            memEl.title = `总计 ${totalGB.toFixed(1)} GB`;
            if (memBar) memBar.style.width = (status.memory.used_percent || 0) + '%';
            if (memDetail) {
                // detail 显示已用 + 百分比 (动态信息下移)
                memDetail.textContent = `已用 ${usedGB.toFixed(1)} GB (${(status.memory.used_percent || 0).toFixed(1)}%)`;
            }
        }
        
// 存储 - 每个磁盘一行显示
                const disksContainer = document.getElementById('home-monitor-disks');
        
        if (status.disks && status.disks.length > 0) {
            if (disksContainer) {
                disksContainer.innerHTML = status.disks.map(d => {
                    const used = (d.used || 0) / (1024 * 1024 * 1024);
                    const total = (d.total || 0) / (1024 * 1024 * 1024);
                    const pct = d.used_percent || 0;
                    const removable = d.is_removable ? ' [移动]' : '';
                    
                    return `
                        <div class="disk-row">
                            <div class="disk-row-header">
                                <span class="disk-letter">${d.letter}:</span>
                                <span class="disk-cap">${used.toFixed(1)}/${total.toFixed(1)}GB</span>
                                <span class="disk-pct">${pct.toFixed(1)}%${removable}</span>
                            </div>
                            <div class="disk-row-bar">
                                <div class="disk-row-fill" style="width:${pct}%"></div>
                            </div>
                        </div>
                    `;
                }).join('');
            }
        } else if (disksContainer) {
            disksContainer.innerHTML = '<div class="disk-empty">未检测到磁盘</div>';
        }
    } catch (e) {
        console.warn('监控更新失败:', e);
        // 异常兜底：用浏览器可用 API 填充面板，避免一直停留在上一次快照或 --
        try { updateMonitorFromFallback(); } catch(_) {}
    }
}

function updateMonitorFromFallback() {
    // 内存 (优先 performance.memory，失败保留原值)
    try {
      if (window.performance && window.performance.memory) {
        const mem = window.performance.memory;
        const usedPercent = (mem.usedJSHeapSize / (mem.jsHeapSizeLimit||1) * 100).toFixed(1);
        const memEl = document.getElementById('home-monitor-memory');
        const memBar = document.getElementById('home-monitor-memory-bar');
        const memDetail = document.getElementById('home-monitor-memory-detail');
        if (memEl) memEl.textContent = usedPercent + '%';
        if (memBar) memBar.style.width = usedPercent + '%';
        if (memDetail) memDetail.textContent = formatBytes(mem.usedJSHeapSize) + ' / ' + formatBytes(mem.jsHeapSizeLimit);
      } else if (navigator && navigator.deviceMemory) {
        const memEl = document.getElementById('home-monitor-memory');
        if (memEl) memEl.textContent = navigator.deviceMemory.toFixed(1) + ' GB 总';
      }
    } catch(_) {}

    // CPU (逻辑核数 + 宽度模拟进度条；避免面板空白) 仅在元素仍为占位或 '--' 或非数字时覆盖
    try {
      if (window.navigator.hardwareConcurrency) {
        const cpuEl = document.getElementById('home-monitor-cpu');
        const cpuBar = document.getElementById('home-monitor-cpu-bar');
        const cpuDetail = document.getElementById('home-monitor-cpu-detail');
        if (cpuEl) { const t = (cpuEl.textContent||'').trim(); if (t === '' || t === '--' || t === '检测中' || t.indexOf('型号') >= 0) cpuEl.textContent = window.navigator.hardwareConcurrency + ' 核'; }
        if (cpuBar && (!cpuBar.style.width || cpuBar.style.width === '0%')) cpuBar.style.width = Math.max(6, Math.min(94, Math.round(window.navigator.hardwareConcurrency * 5))) + '%';
        if (cpuDetail) { const t = (cpuDetail.textContent||'').trim(); if (t === '' || t === '--') cpuDetail.textContent = '逻辑处理器: ' + window.navigator.hardwareConcurrency; }
      }
    } catch(_) {}

    // GPU：通过 WebGL2/WebGL + WEBGL_debug_renderer_info 取到真实渲染器字符串
    try {
      const gpuEl = document.getElementById('home-monitor-gpu');
      const gpuBar = document.getElementById('home-monitor-gpu-bar');
      const gpuDetail = document.getElementById('home-monitor-gpu-detail');
      if (gpuEl || gpuDetail) {
        let renderer = '', vendor = '';
        // ★ 启动性能优化 (2026-09-28): 复用全局缓存的 GPU 信息, 避免重复创建 WebGL 上下文。
        const cached = window.__VDL_GPU_INFO;
        let gl = null;
        if (cached) {
          renderer = cached.renderer || '';
          vendor = cached.vendor || '';
        } else {
          const cnv = document.createElement('canvas');
          gl = (cnv.getContext && (cnv.getContext('webgl2') || cnv.getContext('webgl') || cnv.getContext('experimental-webgl'))) || null;
        }
        if (cached) {
          const cur = (gpuEl ? (gpuEl.textContent||'').trim() : '');
          if (gpuEl && (cur === '' || cur === '--' || cur === 'N/A' || cur === '未知 GPU' || cur === 'WebGL不可用' || cur.indexOf('ANGLE') >= 0 || cur === 'WebGL 已启用')) {
            gpuEl.textContent = renderer ? String(renderer) : 'WebGL 已启用';
          }
          if (gpuDetail) { const dt=(gpuDetail.textContent||'').trim(); if (dt===''||dt==='--'||dt==='WebGL不可用') { gpuDetail.textContent = (vendor?(vendor+' | '):'') + (renderer?renderer:'WebGL 渲染器'); } }
          if (gpuBar && (!gpuBar.style.width || gpuBar.style.width === '0%')) gpuBar.style.width = '14%';
        } else if (gl) {
          const dbg = gl.getExtension('WEBGL_debug_renderer_info');
          renderer = (dbg && dbg.UNMASKED_RENDERER_WEBGL) ? (gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) || '') : (gl.getParameter(gl.RENDERER) || '');
          vendor   = (dbg && dbg.UNMASKED_VENDOR_WEBGL)   ? (gl.getParameter(dbg.UNMASKED_VENDOR_WEBGL)   || '') : (gl.getParameter(gl.VENDOR)   || '');
          if (!renderer) renderer = dbg ? (gl.getParameter(0x9246)||'') : (gl.getParameter(gl.RENDERER)||'');
          if (!vendor)   vendor   = dbg ? (gl.getParameter(0x9245)||'') : (gl.getParameter(gl.VENDOR)||'');
          const cur = (gpuEl ? (gpuEl.textContent||'').trim() : '');
          if (gpuEl && (cur === '' || cur === '--' || cur === 'N/A' || cur === '未知 GPU' || cur === 'WebGL不可用' || cur.indexOf('ANGLE') >= 0 || cur === 'WebGL 已启用')) {
            gpuEl.textContent = renderer ? String(renderer) : (gl ? 'WebGL 已启用' : 'WebGL 不可用');
          }
          if (gpuDetail) { const dt=(gpuDetail.textContent||'').trim(); if (dt===''||dt==='--'||dt==='WebGL不可用') { gpuDetail.textContent = (vendor?(vendor+' | '):'') + (renderer?renderer:(gl?'WebGL 渲染器':'')); } }
          if (gpuBar && (!gpuBar.style.width || gpuBar.style.width === '0%')) gpuBar.style.width = '14%';
        } else {
          if (gpuEl) { const cur=(gpuEl.textContent||'').trim(); if(cur===''||cur==='--') gpuEl.textContent = 'WebGL 不可用'; }
          if (gpuDetail) { const dt=(gpuDetail.textContent||'').trim(); if(dt===''||dt==='--') gpuDetail.textContent = '请启用硬件加速或更新显卡驱动'; }
        }
      }
    } catch(_) { try { const g=document.getElementById('home-monitor-gpu'); if(g){const t=(g.textContent||'').trim();if(t===''||t==='--') g.textContent='WebGL 查询异常';}}catch(e){} }

    // 存储：给一个友好的占位，不要空着
    try {
      const disksContainer = document.getElementById('home-monitor-disks');
      if (disksContainer) {
        const t=(disksContainer.textContent||'').trim();
        if (!disksContainer.innerHTML || t.length < 4 || disksContainer.innerHTML.indexOf('disk-row') < 0) {
          disksContainer.innerHTML = '<div class="disk-empty" style="color:var(--theme-text-muted);font-size:12px;padding:6px 2px;">磁盘信息由后端上报 (浏览器环境无法直接读取盘符)</div>';
        }
      }
    } catch(_) {}
}
// ★ 修复 (2026-10-03): 这里原本还有**第二个** formatBytes 顶层声明。
//   顶层函数声明会被提升, 后面的覆盖前面的 → 全文件实际用的是那个版本,
//   而它对 null/undefined 会算出 "NaN undefined" (Math.log(undefined) = NaN),
//   且 MB 只保留 1 位小数。本文件上方 (约 4892 行) 已有一个 null-safe 的版本,
//   这里删除重复定义, 让所有调用点都走那个安全版本。

document.addEventListener('DOMContentLoaded', () => {
    const homePage = document.getElementById('page-home');
    if (homePage && homePage.classList.contains('active')) {
        startMonitor();
    }
});

// ============================================================
// 系统优化功能
// ============================================================

const OPTIMIZATION_ITEMS = {
    performance: [
        { id: 'keyboard_latency', name: '键盘响应延迟', desc: '优化键盘输入延迟', admin: false },
        { id: 'tcp_optimization', name: 'TCP优化', desc: '优化TCP参数，提高网络性能', admin: true },
        { id: 'visual_effects', name: '视觉特效', desc: '关闭不必要的视觉特效', admin: false },
    ],
    privacy: [
        { id: 'telemetry', name: '遥测数据收集', desc: '禁用Windows遥测数据收集', admin: true },
        { id: 'ad_id', name: '广告标识符', desc: '禁用应用使用广告标识符', admin: false },
        { id: 'cortana', name: 'Cortana助手', desc: '禁用Cortana助手功能', admin: true },
    ],
    gpu: [
        { id: 'hardware_acceleration', name: '硬件加速GPU调度', desc: '启用HAGS硬件加速调度', admin: true },
        { id: 'gpu_latency', name: '降低显示延迟', desc: '优化显示延迟设置', admin: false },
    ],
    power: [
        { id: 'hibernation', name: '休眠功能', desc: '禁用休眠节省磁盘空间', admin: true },
        { id: 'fast_startup', name: '快速启动', desc: '禁用快速启动', admin: true },
        { id: 'usb_suspend', name: 'USB选择性挂起', desc: '禁用USB选择性挂起', admin: true },
        { id: 'power_throttling', name: '电源节流', desc: '禁用电源节流', admin: true },
    ],
    experience: [
        { id: 'menu_delay', name: '菜单显示延迟', desc: '缩短菜单显示延迟', admin: false },
        { id: 'taskbar_animation', name: '任务栏动画', desc: '禁用任务栏动画效果', admin: false },
        { id: 'classic_context_menu', name: '经典右键菜单', desc: '使用经典右键菜单样式', admin: false },
    ],
};

let optimizationBackendStatus = {};

async function loadOptimizationStatus() {
    try {
        const invokeFn = window.__TAURI__?.core?.invoke;
        if (!invokeFn) return;
        const status = await invokeFn('get_optimizer_status').catch(() => []);
        optimizationBackendStatus = {};
        if (Array.isArray(status)) {
            status.forEach(s => {
                optimizationBackendStatus[s.id] = {
                    enabled: s.enabled,
                    requiresAdmin: s.requires_admin,
                };
            });
        }
        const activeTab = document.querySelector('.optimizer-tab.active');
        if (activeTab) {
            renderOptimizationList(activeTab.dataset.tab);
        }
    } catch (e) {
        console.warn('加载优化状态失败:', e);
    }
}

async function applyOptimization(id, enable) {
    try {
        const invokeFn = window.__TAURI__?.core?.invoke;
        if (!invokeFn) {
            showToast('后端未就绪，请重启应用');
            return;
        }
        if (enable) {
            await invokeFn('apply_optimizer', { id });
        } else {
            await invokeFn('revert_optimizer', { id });
        }
        showToast(enable ? '✓ 已应用' : '✓ 已恢复');
    } catch (e) {
        if (String(e).includes('管理员')) {
            showToast('⚠️ 需要管理员权限');
        } else {
            showToast('✗ ' + e);
        }
    }
}

function renderOptimizationList(category) {
    const container = document.getElementById(`optimizer-${category}-list`);
    if (!container) return;
    
    const items = OPTIMIZATION_ITEMS[category] || [];
    const localState = JSON.parse(localStorage.getItem('optimizer_state') || '{}');
    
    container.innerHTML = items.map(item => {
        const backendStatus = optimizationBackendStatus[item.id];
        const enabled = backendStatus ? backendStatus.enabled : (localState[item.id] === true);
        const needAdmin = item.admin || (backendStatus && backendStatus.requiresAdmin);
        
        return `
            <div class="optimizer-item">
                <div class="optimizer-item-info">
                    <div class="optimizer-item-name">
                        ${item.name}
                        ${needAdmin ? '<span class="admin-tag">管理员</span>' : ''}
                    </div>
                    <div class="optimizer-item-desc">${item.desc}</div>
                </div>
                <label class="toggle-switch">
                    <input type="checkbox" ${enabled ? 'checked' : ''} data-id="${item.id}">
                    <span class="toggle-slider"></span>
                </label>
            </div>
        `;
    }).join('');
    
    container.querySelectorAll('.toggle-switch input').forEach(input => {
        input.addEventListener('change', async function(e) {
            const id = e.target.dataset.id;
            const enabled = e.target.checked;
            
            const state = JSON.parse(localStorage.getItem('optimizer_state') || '{}');
            state[id] = enabled;
            localStorage.setItem('optimizer_state', JSON.stringify(state));
            
            await applyOptimization(id, enabled);
            await loadOptimizationStatus();
        });
    });
}

function initOptimizationTabs() {
    loadOptimizationStatus();
    
    document.querySelectorAll('.optimizer-tab').forEach(tab => {
        tab.addEventListener('click', function() {
            const category = this.dataset.tab;
            
            document.querySelectorAll('.optimizer-tab').forEach(t => t.classList.remove('active'));
            this.classList.add('active');
            
            document.querySelectorAll('.optimizer-panel').forEach(p => p.classList.remove('active'));
            const panel = document.querySelector(`.optimizer-panel[data-panel="${category}"]`);
            if (panel) panel.classList.add('active');
            
            renderOptimizationList(category);
        });
    });

    renderOptimizationList('performance');
}

// ============================================================
// 自定义液态玻璃下拉框 (vx-select) — JS 增强
// ------------------------------------------------------------
// 原生 <select> 的展开列表由 WebView2/Chromium 系统渲染, option 样式无法被
// CSS 覆盖 —— 这正是"下拉栏很丑"的根因. 这里把原生 select 包进自定义控件:
// 隐藏原生 select, 但保留它作为取值 / change 事件源, 用可完全美化的玻璃面板
// 替代系统弹层. 既有的 change 监听 (来源/分类/主题/成人分类) 无需改动.
// 同时兼容: 程序化赋值 (switchTheme 直接改 .value) 与选项动态重建
// (loadAdultCategories 重写 innerHTML) 两种场景.
// ============================================================
function enhanceAllSelects(root) {
    const scope = root || document;
    scope.querySelectorAll('select:not(.vx-select-native)').forEach(function (s) {
        try { enhanceSelect(s); } catch (e) { console.warn('[vx-select] 增强失败:', e); }
    });
}

function enhanceSelect(select) {
    if (!select || select.dataset.vxEnhanced === '1') return;
    select.dataset.vxEnhanced = '1';

    const inFilters = !!select.closest('.filters');
    const isTheme = select.classList.contains('theme-select');

    // 1) 包装结构: <div.vx-select> [ select.vx-select-native, .vx-select-trigger, .vx-select-menu ]
    const wrap = document.createElement('div');
    wrap.className = 'vx-select' + (isTheme ? ' vx-select--theme' : '');
    select.parentNode.insertBefore(wrap, select);

    const trigger = document.createElement('div');
    trigger.className = 'vx-select-trigger';
    trigger.setAttribute('role', 'button');
    trigger.setAttribute('tabindex', '0');
    trigger.setAttribute('aria-haspopup', 'listbox');
    trigger.setAttribute('aria-expanded', 'false');

    const valueEl = document.createElement('span');
    valueEl.className = 'vx-select-value';

    const chevron = document.createElement('span');
    chevron.className = 'vx-select-chevron';
    chevron.innerHTML = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"></polyline></svg>';

    trigger.appendChild(valueEl);
    trigger.appendChild(chevron);

    const menu = document.createElement('div');
    menu.className = 'vx-select-menu';
    menu.setAttribute('role', 'listbox');

    // 原生 select 移入 wrap 并隐藏 (仍作为取值 / 事件源)
    wrap.appendChild(select);
    select.classList.add('vx-select-native');
    wrap.appendChild(trigger);
    wrap.appendChild(menu);

    // 2) 显示值同步
    function syncLabel() {
        const opt = select.options[select.selectedIndex];
        const text = opt ? opt.textContent : '';
        valueEl.textContent = text;
        valueEl.title = text;
        Array.prototype.forEach.call(menu.children, function (el) {
            el.classList.toggle('selected', el.dataset.value === select.value);
        });
    }

    // 3) 依据最宽选项设定最小宽度, 避免切换选项时宽度跳动 (贴近原生表现)
    function applyMinWidth() {
        if (!inFilters) return;
        try {
            const cs = getComputedStyle(select);
            const canvas = document.createElement('canvas');
            const ctx = canvas.getContext('2d');
            if (!ctx) return;
            ctx.font = (cs.fontWeight || '500') + ' ' + (cs.fontSize || '14px') + ' ' + (cs.fontFamily || 'sans-serif');
            let max = 0;
            Array.prototype.forEach.call(select.options, function (o) {
                const w = ctx.measureText(o.textContent || '').width;
                if (w > max) max = w;
            });
            if (max > 0) wrap.style.minWidth = Math.ceil(max + 56) + 'px';
        } catch (_) {}
    }

    // 4) 构建展开面板选项
    function rebuild() {
        menu.innerHTML = '';
        Array.prototype.forEach.call(select.options, function (opt) {
            const item = document.createElement('div');
            item.className = 'vx-select-option';
            item.setAttribute('role', 'option');
            item.dataset.value = opt.value;
            const label = document.createElement('span');
            label.className = 'vx-select-opt-label';
            label.textContent = opt.textContent;
            item.appendChild(label);
            item.addEventListener('mousedown', function (e) { e.preventDefault(); });
            item.addEventListener('click', function () { choose(opt.value); });
            menu.appendChild(item);
        });
        applyMinWidth();
        syncLabel();
    }

    function choose(val) {
        if (select.value !== val) {
            select.value = val;
            select.dispatchEvent(new Event('change', { bubbles: true }));
        }
        syncLabel();
        close();
    }

    // 5) 展开 / 收起
    function open() {
        if (wrap.classList.contains('open')) return;
        document.querySelectorAll('.vx-select.open').forEach(function (w) { w.classList.remove('open'); });
        wrap.classList.add('open');
        trigger.setAttribute('aria-expanded', 'true');
        const sel = menu.querySelector('.vx-select-option.selected');
        if (sel && sel.scrollIntoView) sel.scrollIntoView({ block: 'nearest' });
    }
    function close() {
        wrap.classList.remove('open');
        trigger.setAttribute('aria-expanded', 'false');
    }
    function toggle() { wrap.classList.contains('open') ? close() : open(); }

    trigger.addEventListener('click', function (e) { e.stopPropagation(); toggle(); });
    trigger.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault(); toggle();
        } else if (e.key === 'Escape') {
            close();
        } else if (e.key === 'ArrowDown') {
            e.preventDefault();
            const i = select.selectedIndex;
            if (i < select.options.length - 1) choose(select.options[i + 1].value);
        } else if (e.key === 'ArrowUp') {
            e.preventDefault();
            const i = select.selectedIndex;
            if (i > 0) choose(select.options[i - 1].value);
        }
    });
    document.addEventListener('click', function (e) {
        if (!wrap.contains(e.target)) close();
    });

    // 6) 程序化赋值 (如 switchTheme 直接改 .value) 时同步显示值
    try {
        const desc = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value');
        if (desc && desc.get && desc.set) {
            Object.defineProperty(select, 'value', {
                configurable: true,
                enumerable: true,
                get: function () { return desc.get.call(this); },
                set: function (v) { desc.set.call(this, v); syncLabel(); }
            });
        }
    } catch (_) {}

    // 7) 选项被动态重建 (如 loadAdultCategories 重写 innerHTML) 时刷新面板
    try {
        new MutationObserver(function () { rebuild(); }).observe(select, { childList: true });
    } catch (_) {}

    rebuild();
}

if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () {
        try { enhanceAllSelects(); } catch (e) { console.warn('[vx-select] 初始化失败:', e); }
    });
} else {
    try { enhanceAllSelects(); } catch (e) { console.warn('[vx-select] 初始化失败:', e); }
}

/* ============================================================
   模块骨架 (修改器 / 动漫 / 书库)
   ------------------------------------------------------------
   本块只负责: 内层左导轨切换 / 子标签切换 / 页面懒加载钩子 /
   第三方 API 密钥读写。各模块的业务逻辑挂在 window.__vxBk /
   __vxTr 上, 由后续阶段填充。
   ============================================================ */
(function () {
    'use strict';

    // ---------- 打开外部链接 ----------
    window.vxOpenUrl = function (url) {
        if (!url) return;
        try { invoke('open_external_browser', { url: url }); }
        catch (e) { console.warn('[vx] 打开链接失败', e); }
    };

    // ---------- 内层左导轨 / 子标签 (事件委托, 动态内容也生效) ----------
    document.addEventListener('click', function (e) {
        const railBtn = e.target.closest('.mod-rail-btn');
        if (railBtn) {
            const rail = railBtn.closest('.mod-rail');
            const shell = rail && rail.closest('.mod-shell');
            if (!shell) return;
            rail.querySelectorAll('.mod-rail-btn').forEach(function (b) {
                b.classList.toggle('active', b === railBtn);
            });
            const tab = railBtn.dataset.tab;
            shell.querySelectorAll('.mod-body > .mod-panel').forEach(function (p) {
                p.classList.toggle('active', p.dataset.tab === tab);
            });
            const page = shell.closest('.page');
            if (page) {
                try { localStorage.setItem('vortex_' + page.id.replace('page-', '') + '_tab', tab); } catch (_) {}
            }
            vxModTabShown(page ? page.id : '', tab);
            return;
        }
        const subBtn = e.target.closest('.mod-subtab');
        if (subBtn) {
            const wrap = subBtn.closest('.mod-panel');
            if (!wrap) return;
            wrap.querySelectorAll('.mod-subtab').forEach(function (b) {
                b.classList.toggle('active', b === subBtn);
            });
            const sub = subBtn.dataset.sub;
            wrap.querySelectorAll('.mod-subpanel').forEach(function (p) {
                p.classList.toggle('active', p.dataset.sub === sub);
            });
            vxModSubShown(wrap.closest('.page') ? wrap.closest('.page').id : '', sub);
        }
    });

    // ---------- 页面 / 标签懒加载钩子 ----------
    function vxPageInit(page) {
        const f = { books: '__vxBk', trainer: '__vxTr', anime: '__vxAn' }[page];
        if (f && window[f] && typeof window[f].init === 'function') {
            try { window[f].init(); } catch (e) { console.warn('[vx] 页面初始化失败', page, e); }
        }
    }
    function vxModTabShown(pageId, tab) {
        const key = { 'page-books': '__vxBk', 'page-trainer': '__vxTr', 'page-anime': '__vxAn' }[pageId];
        if (key && window[key] && typeof window[key].tabShown === 'function') {
            try { window[key].tabShown(tab); } catch (e) { console.warn('[vx] 标签初始化失败', tab, e); }
        }
    }
    function vxModSubShown(pageId, sub) {
        if (pageId === 'page-books' && window.__vxBk && typeof window.__vxBk.subShown === 'function') {
            try { window.__vxBk.subShown(sub); } catch (e) { console.warn('[vx] 子标签初始化失败', sub, e); }
        }
    }
    window.vxPageInit = vxPageInit;
    window.vxModTabShown = vxModTabShown;

    // 包装 navigateToPage, 让程序化跳转 (首页卡片等) 也能触发初始化
    (function hookNav() {
        if (typeof window.navigateToPage !== 'function') { setTimeout(hookNav, 120); return; }
        const orig = window.navigateToPage;
        window.navigateToPage = function (page) {
            const r = orig.apply(this, arguments);
            vxPageInit(page);
            return r;
        };
    })();

    // ---------- 恢复上次停留的内层标签 ----------
    function restoreRail(pageKey) {
        try {
            const tab = localStorage.getItem('vortex_' + pageKey + '_tab');
            if (!tab) return;
            const page = document.getElementById('page-' + pageKey);
            const btn = page && page.querySelector('.mod-rail-btn[data-tab="' + tab + '"]');
            if (btn && !btn.classList.contains('active')) btn.click();
        } catch (_) {}
    }

    console.log('[vx] 三大模块骨架已加载');
})();

/* ============================================================
   修改器 — 前端逻辑 (window.__vxTr)
   数据源 flingtrainer.com; 索引与封面走允许抓取的 sitemap。
   商城点图标 → 确认 → 直接下载到软件内 (全局串行, 由后端限速);
   装好的在「我的库」里, 单击打开、右键更多操作。
   ============================================================ */
window.__vxTr = (function () {
    'use strict';

    let inited = false;
    let index = [];          // 全量索引 (带封面)
    let library = [];
    let busy = false;        // 正在准备 / 下载, 防连点
    let pending = null;      // 当前确认框对应的修改器
    let ctxIdx = -1;         // 右键菜单指向的库条目下标
    let bound = false;

    const $ = (id) => document.getElementById(id);
    function esc(s) {
        return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
            return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c];
        });
    }
    function setStatus(id, text, kind) {
        const el = $(id);
        if (!el) return;
        el.textContent = text || '';
        el.className = 'mod-status' + (kind ? ' is-' + kind : '');
    }
    function fmtSize(n) {
        if (!n) return '0 B';
        if (n < 1024) return n + ' B';
        if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
        if (n < 1073741824) return (n / 1048576).toFixed(1) + ' MB';
        return (n / 1073741824).toFixed(2) + ' GB';
    }

    // ---------- 商城 ----------
    /// 显示名: 优先 Steam 官方中文名, 没查到才用英文名
    function dispName(t) {
        return (t && (t.name_zh || t.name)) || '';
    }

    function filtered() {
        const kw = (($('tr-search') || {}).value || '').trim().toLowerCase();
        if (!kw) return index;
        // 中英文都能搜到
        return index.filter(function (t) {
            return (t.name || '').toLowerCase().indexOf(kw) >= 0 ||
                   (t.name_zh || '').toLowerCase().indexOf(kw) >= 0 ||
                   (t.slug || '').toLowerCase().indexOf(kw) >= 0;
        });
    }

    function renderGrid() {
        const grid = $('tr-grid');
        if (!grid) return;
        if (!index.length) {
            grid.innerHTML = '<div class="mod-empty">还没有索引 —— 点右上角「同步索引」从 flingtrainer 抓取（约 750+ 个修改器）</div>';
            return;
        }
        const list = filtered().slice(0, 400);
        if (!list.length) {
            grid.innerHTML = '<div class="mod-empty">没有匹配的修改器</div>';
            return;
        }
        grid.innerHTML = list.map(function (t) {
            const zh = dispName(t);
            const ph = esc((zh.slice(0, 1) || '?').toUpperCase());
            // 首字母占位垫在底下, 图挂了或还没加载出来时不会开天窗
            const img = t.cover
                ? '<img class="tr-card-img" loading="lazy" decoding="async" referrerpolicy="no-referrer" src="' +
                  esc(t.cover) + '" alt="">'
                : '';
            // title 里补上英文名, 中文名对不上时可以对照
            const tip = t.name_zh && t.name && t.name_zh !== t.name ? zh + ' / ' + t.name : zh;
            return '<div class="tr-card" data-tr-pick="' + esc(t.url) + '" data-tr-game="' + esc(zh) +
                '" title="' + esc(tip) + '">' +
                '<div class="tr-card-cover"><span class="tr-card-ph">' + ph + '</span>' + img + '</div>' +
                '<div class="tr-card-name">' + esc(zh) + '</div>' +
                '</div>';
        }).join('');
    }

    async function syncIndex() {
        setStatus('tr-status', '正在从 sitemap 同步索引…');
        const btn = $('tr-sync-btn');
        if (btn) btn.disabled = true;
        try {
            index = await invoke('tr_sync') || [];
            renderGrid();
            setStatus('tr-status', '已同步 ' + index.length + ' 个修改器', 'ok');
            const hint = $('tr-index-hint');
            if (hint) hint.textContent = '共 ' + index.length + ' 个';
        } catch (e) {
            setStatus('tr-status', '同步失败: ' + e, 'err');
        }
        if (btn) btn.disabled = false;
    }

    async function loadIndex() {
        try {
            index = await invoke('tr_index') || [];
            const hint = $('tr-index-hint');
            if (hint) hint.textContent = index.length ? '共 ' + index.length + ' 个' : '';
            renderGrid();
        } catch (e) { console.warn('[tr] 读取索引失败', e); }
    }

    /// 手动重试之前没查到的中文名 (Steam 查不到的那些)
    async function retranslateNames() {
        const btn = $('tr-names-btn');
        if (btn) btn.disabled = true;
        setStatus('tr-status', '正在重试中文名…');
        try {
            const n = await invoke('tr_translate_names');
            setStatus('tr-status', n > 0 ? '又查到 ' + n + ' 个中文名' : '没有新的中文名可补', 'ok');
            await loadIndex();
            await loadLibrary();
        } catch (e) {
            setStatus('tr-status', '重试失败: ' + e, 'err');
        }
        if (btn) btn.disabled = false;
    }

    // ---------- 确认下载 ----------
    function openModal() {
        const m = $('tr-dl-modal');
        if (m) m.style.display = 'flex';
    }
    function closeModal() {
        const m = $('tr-dl-modal');
        if (m) m.style.display = 'none';
        pending = null;
    }

    let pickSeq = 0;   // 每次点卡片 +1, 后点的直接取代先点的
    async function pickTrainer(url, game) {
        // ★ 这里**不能**用全局 busy 守卫: 一次请求卡住就会把整个网格锁死, 点谁都没反应。
        //   改成序号 —— 后点的取代先点的, 旧请求回来时直接丢弃。
        const seq = ++pickSeq;
        setStatus('tr-status', '正在读取《' + game + '》的下载信息…');
        const title = $('tr-dl-title');
        const body = $('tr-dl-body');
        if (title) title.textContent = game;
        if (body) body.innerHTML = '<div class="loading">正在读取下载信息…</div>';
        openModal();
        try {
            const d = await invoke('tr_detail', { url: url });
            if (seq !== pickSeq) return;   // 已经被新的点击取代, 丢弃这次结果
            pending = { url: url, game: game, files: d.files || [] };
            renderModalBody(d);
            setStatus('tr-status', '');
        } catch (e) {
            if (seq !== pickSeq) return;
            if (body) body.innerHTML = '<div class="mod-empty">读取失败: ' + esc(String(e)) + '</div>';
            setStatus('tr-status', '读取失败: ' + e, 'err');
        }
    }

    function renderModalBody(d) {
        const body = $('tr-dl-body');
        if (!body) return;
        const files = d.files || [];
        if (!files.length) {
            body.innerHTML = '<div class="mod-empty">这个修改器的页面上没有解析到下载文件。</div>';
            return;
        }
        const opts = (d.options || []).slice(0, 30);
        const optHtml = opts.length
            ? '<div class="tr-dl-sec">功能选项</div><ul class="tr-options">' +
              opts.map(function (o) { return '<li>' + esc(o) + '</li>'; }).join('') + '</ul>'
            : '';
        // 大多数修改器只有一个文件; 有多个时让用户挑一个
        const fileHtml = files.map(function (f, i) {
            return '<label class="tr-dl-file">' +
                '<input type="radio" name="tr-dl-file" value="' + i + '"' + (i === 0 ? ' checked' : '') + '>' +
                '<span class="tr-dl-fname">' + esc(f.name) + '</span>' +
                '</label>';
        }).join('');
        body.innerHTML =
            '<div class="tr-dl-q">要下载这个修改器吗？文件会保存到软件内，之后在「我的库」里打开。</div>' +
            '<div class="tr-dl-sec">下载文件' + (files.length > 1 ? '（共 ' + files.length + ' 个，选一个）' : '') + '</div>' +
            '<div class="tr-dl-files">' + fileHtml + '</div>' +
            optHtml +
            '<div class="tr-dl-note">' + esc(d.note || '') + '</div>' +
            '<div class="tr-dl-actions">' +
            '<button class="btn btn-secondary" data-tr-modal-close="1">取消</button>' +
            '<button class="btn btn-primary" id="tr-dl-go">确认下载</button>' +
            '</div>' +
            '<div id="tr-dl-status" class="mod-status"></div>';
    }

    async function doDownload() {
        if (!pending || busy) return;
        const sel = document.querySelector('input[name="tr-dl-file"]:checked');
        const f = pending.files[sel ? parseInt(sel.value, 10) : 0];
        if (!f) return;
        busy = true;
        const btn = $('tr-dl-go');
        if (btn) { btn.disabled = true; btn.textContent = '下载中…'; }
        // 下载走主下载引擎 → 「下载」页有实时速度/进度
        setStatus('tr-dl-status', '正在下载 ' + f.name + ' …（可在左侧「下载」页看实时进度）');
        try {
            // pageUrl 用来事后查中文名 (下载令牌地址反推不出游戏 slug)
            const item = await invoke('tr_download', {
                url: f.url,
                game: pending.game,
                pageUrl: pending.url,
            });
            setStatus('tr-dl-status', '已下载到「我的库」: ' + item.filename + '（' + fmtSize(item.size) + '）', 'ok');
            if (btn) btn.textContent = '已下载';
            setStatus('tr-status', '《' + pending.game + '》已下载到我的库', 'ok');
            await loadLibrary();
            // ★ 下载完自动把修改器目录加进 Defender 排除项 (修改器改内存常被误杀)。
            //   后端只加一次, 加过就直接返回, 不会每次下载都弹 UAC。
            invoke('tr_defender_exclude').then(function (r) {
                if (r && r !== '已经加过了') {
                    setStatus('tr-dl-status', '已自动加入 Defender 白名单（首次会弹一次 UAC）', 'ok');
                }
            }).catch(function () { /* 用户拒绝提权就算了, 不打断下载流程 */ });
        } catch (e) {
            setStatus('tr-dl-status', '下载失败: ' + e, 'err');
            if (btn) { btn.disabled = false; btn.textContent = '重试'; }
        }
        busy = false;
    }

    // ---------- 我的库 ----------
    async function loadLibrary() {
        const box = $('tr-lib-list');
        if (!box) return;
        try {
            library = await invoke('tr_library') || [];
            if (!library.length) {
                box.innerHTML = '<div class="mod-empty">库里还没有修改器 —— 在「修改器商城」点一个图标下载，或用「导入本地修改器」添加</div>';
                return;
            }
            box.innerHTML = library.map(function (it, i) {
                const nm = it.game_zh || it.game || it.filename || '';
                const ph = esc((nm.slice(0, 1) || '?').toUpperCase());
                // 封面跟商城一样用 flingtrainer 的 460x215 头图, 加载失败露出首字母
                const cover = it.cover
                    ? '<img class="tr-lib-img" loading="lazy" decoding="async" referrerpolicy="no-referrer" src="' +
                      esc(it.cover) + '" alt="">'
                    : '';
                return '<div class="tr-lib-item" data-tr-lib="' + i + '">' +
                    '<div class="tr-lib-cover"><span class="tr-lib-ph">' + ph + '</span>' + cover + '</div>' +
                    '<div class="tr-lib-main">' +
                    '<div class="tr-lib-name">' + esc(nm) + '</div>' +
                    '<div class="tr-lib-meta">' + esc(it.filename) + ' · ' + fmtSize(it.size) + '</div>' +
                    '</div>' +
                    '<span class="tr-lib-hint">单击打开</span>' +
                    '</div>';
            }).join('');
        } catch (e) {
            box.innerHTML = '<div class="mod-empty">读取失败: ' + esc(String(e)) + '</div>';
        }
    }

    async function launchItem(i) {
        const it = library[i];
        if (!it) return;
        // 修改器要改游戏内存, 基本都需要管理员权限 —— 系统可能弹一次 UAC
        setStatus('tr-lib-status', '正在打开 ' + it.filename + ' …（如弹出管理员授权提示，点「是」即可）');
        try {
            const p = await invoke('tr_launch', { path: it.path });
            setStatus('tr-lib-status', '已启动: ' + p, 'ok');
        } catch (e) {
            setStatus('tr-lib-status', '打开失败: ' + e, 'err');
        }
    }

    async function revealItem(i) {
        const it = library[i];
        if (!it) return;
        try {
            await invoke('open_file', { path: it.path });
            setStatus('tr-lib-status', '');
        } catch (e) {
            setStatus('tr-lib-status', '打开文件夹失败: ' + e, 'err');
        }
    }

    async function renameItem(i) {
        const it = library[i];
        if (!it) return;
        const name = prompt('修改显示名（只改这里的名字，不改文件名）', it.game || '');
        if (name == null) return;
        try {
            await invoke('tr_rename', { path: it.path, game: name.trim() });
            setStatus('tr-lib-status', '已重命名', 'ok');
            loadLibrary();
        } catch (e) {
            setStatus('tr-lib-status', '重命名失败: ' + e, 'err');
        }
    }

    async function deleteItem(i) {
        const it = library[i];
        if (!it) return;
        if (!confirm('从库里删除 ' + it.filename + ' ?（会同时删除文件）')) return;
        try {
            await invoke('tr_delete', { path: it.path });
            setStatus('tr-lib-status', '已删除', 'ok');
            loadLibrary();
        } catch (e) {
            setStatus('tr-lib-status', '删除失败: ' + e, 'err');
        }
    }

    async function importLocal() {
        try {
            const p = await invoke('browse_path');
            if (!p) return;
            const game = prompt('这是哪个游戏的修改器？', '') || '';
            const item = await invoke('tr_import', { path: p, game: game });
            setStatus('tr-lib-status', '已导入: ' + item.filename, 'ok');
            loadLibrary();
        } catch (e) {
            setStatus('tr-lib-status', '导入失败: ' + e, 'err');
        }
    }

    // ---------- 右键菜单 ----------
    function closeCtx() {
        const c = $('tr-ctx');
        if (c) c.style.display = 'none';
    }

    function openCtx(x, y, i) {
        const c = $('tr-ctx');
        const it = library[i];
        if (!c || !it) return;
        ctxIdx = i;
        c.innerHTML =
            '<button class="ctx-item" data-tr-ctx="open">打开</button>' +
            '<button class="ctx-item" data-tr-ctx="folder">打开所在文件夹</button>' +
            '<button class="ctx-item" data-tr-ctx="rename">重命名</button>' +
            '<div class="ctx-sep"></div>' +
            '<button class="ctx-item is-danger" data-tr-ctx="delete">删除</button>';
        c.style.display = 'block';
        // 贴边修正, 别让菜单跑到窗口外
        const w = c.offsetWidth || 160;
        const h = c.offsetHeight || 160;
        c.style.left = Math.max(8, Math.min(x, window.innerWidth - w - 8)) + 'px';
        c.style.top = Math.max(8, Math.min(y, window.innerHeight - h - 8)) + 'px';
    }

    function runCtx(act) {
        const i = ctxIdx;
        closeCtx();
        if (i < 0) return;
        if (act === 'open') launchItem(i);
        else if (act === 'folder') revealItem(i);
        else if (act === 'rename') renameItem(i);
        else if (act === 'delete') deleteItem(i);
    }

    // ---------- 弹幕 ----------
    async function loadDanmaku() {
        const idEl = $('an-dm-id');
        const box = $('an-dm-list');
        if (!box) return;
        const raw = (idEl && idEl.value || '').trim();
        if (!raw) { setStatus('an-dm-status', '请输入剧集 ID', 'err'); return; }
        const id = parseInt(raw, 10);
        if (isNaN(id) || id <= 0) { setStatus('an-dm-status', 'ID 必须是正整数', 'err'); return; }
        box.innerHTML = '<div class="loading">正在拉取弹幕…</div>';
        setStatus('an-dm-status', '正在从 animeko 弹幕服务拉取…');
        try {
            const list = await invoke('anime_danmaku', { episodeId: id }) || [];
            if (!list.length) {
                box.innerHTML = '<div class="mod-empty">这个剧集还没有弹幕</div>';
                setStatus('an-dm-status', '0 条');
                return;
            }
            // 按时间排序展示
            list.sort(function (a, b) { return (a.time || 0) - (b.time || 0); });
            box.innerHTML = list.map(function (d) {
                const t = Number(d.time) || 0;
                const mm = Math.floor(t / 60), ss = Math.floor(t % 60);
                const ts = mm + ':' + (ss < 10 ? '0' + ss : ss);
                const loc = d.location === 5 ? '顶部' : (d.location === 4 ? '底部' : '滚动');
                const hex = (d.color >>> 0).toString(16).padStart(6, '0');
                return '<div class="an-row">' +
                    '<span class="an-dm-time">' + ts + '</span>' +
                    '<div class="an-row-main"><div class="an-row-title" style="color:#' + hex + '">' + esc(d.text) + '</div>' +
                    '<div class="an-row-meta">' + loc + '</div></div>' +
                    '</div>';
            }).join('');
            setStatus('an-dm-status', '共 ' + list.length + ' 条弹幕', 'ok');
        } catch (e) {
            box.innerHTML = '<div class="mod-empty">拉取失败: ' + esc(String(e)) + '</div>';
            setStatus('an-dm-status', String(e), 'err');
        }
    }
    // ---------- 绑定 ----------
    function bind() {
        if (bound) return;
        bound = true;

        document.addEventListener('click', function (e) {
            if (e.target.closest('[data-tr-modal-close]')) { closeModal(); return; }
            if (e.target.closest('#tr-dl-go')) { doDownload(); return; }

            const ci = e.target.closest('[data-tr-ctx]');
            if (ci) { runCtx(ci.dataset.trCtx); return; }

            // 商城卡片 → 确认下载
            const card = e.target.closest('[data-tr-pick]');
            if (card) { pickTrainer(card.dataset.trPick, card.dataset.trGame); return; }

            // 库条目 → 单击打开
            const row = e.target.closest('[data-tr-lib]');
            if (row) { closeCtx(); launchItem(parseInt(row.dataset.trLib, 10)); return; }

            closeCtx();
        });

        // 库条目右键 → 菜单 (浏览器默认菜单要压掉)
        document.addEventListener('contextmenu', function (e) {
            const row = e.target.closest('[data-tr-lib]');
            if (!row) return;
            e.preventDefault();
            openCtx(e.clientX, e.clientY, parseInt(row.dataset.trLib, 10));
        });

        document.addEventListener('keydown', function (e) {
            if (e.key === 'Escape') { closeCtx(); closeModal(); }
        });
        window.addEventListener('resize', closeCtx);

        // 封面加载失败 → 藏掉, 露出底下的首字母占位
        // (error 事件不冒泡, 必须用捕获阶段)
        ['tr-grid', 'tr-lib-list'].forEach(function (id) {
            const box = $(id);
            if (!box) return;
            box.addEventListener('error', function (e) {
                const t = e.target;
                if (t && t.classList && (t.classList.contains('tr-card-img') ||
                                         t.classList.contains('tr-card-bg') ||
                                         t.classList.contains('tr-lib-img'))) {
                    t.classList.add('is-broken');
                }
            }, true);
        });

        if ($('tr-sync-btn')) $('tr-sync-btn').addEventListener('click', syncIndex);
        if ($('tr-names-btn')) $('tr-names-btn').addEventListener('click', retranslateNames);
        if ($('tr-search')) $('tr-search').addEventListener('input', function () {
            clearTimeout(bind._t);
            bind._t = setTimeout(renderGrid, 200);
        });

        // 中文名是后台慢慢补的 (758 次查询), 用事件驱动刷新
        const listen = getListen();
        if (listen) {
            listen('tr-names-progress', function (e) {
                const p = (e && e.payload) || {};
                setStatus('tr-status', '正在获取中文名 ' + (p.done || 0) + ' / ' + (p.total || 0) + ' …');
            });
            listen('tr-names-done', function (e) {
                const p = (e && e.payload) || {};
                if (p.aborted) {
                    // 连着失败太多次 —— 多半是被 Steam 限流了, 让用户过会儿再点
                    setStatus('tr-status', '中文名查询被中断（可能被限流），稍后再点「重试中文名」', 'err');
                } else {
                    setStatus('tr-status', '中文名已更新（命中 ' + (p.hits || 0) + ' / ' + (p.total || 0) + '）', 'ok');
                }
                loadIndex();     // 重新拉一次带中文名的索引
                loadLibrary();   // 库里的老条目也会跟着变中文
            });
        }
        if ($('tr-lib-refresh-btn')) $('tr-lib-refresh-btn').addEventListener('click', loadLibrary);
    }

    async function init() {
        bind();
        if (inited) return;
        inited = true;
        await loadIndex();
        await loadLibrary();
    }

    function tabShown(tab) {
        closeCtx();
        if (tab === 'library') loadLibrary();
        if (tab === 'all') renderGrid();
    }

    return { init: init, tabShown: tabShown };
})();

// ============================================================
// 自绘标题栏 (2026-10-06): 关掉系统装饰后自己实现最小化/最大化/关闭
// ============================================================
(function bindTitlebar() {
    function ready(fn) {
        if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', fn);
        else fn();
    }
    ready(function () {
        const $ = (id) => document.getElementById(id);
        const call = function (cmd) {
            try {
                const g = (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke)
                       || (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke);
                if (g) g(cmd).catch(function () {});
            } catch (_) {}
        };
        if ($('vx-tb-min')) $('vx-tb-min').addEventListener('click', function () { call('vx_window_min'); });
        if ($('vx-tb-max')) $('vx-tb-max').addEventListener('click', function () { call('vx_window_toggle_max'); });
        if ($('vx-tb-close')) $('vx-tb-close').addEventListener('click', function () { call('vx_window_close'); });
        // 双击标题栏空白处 = 最大化/还原 (Windows 习惯)
        const tb = $('vx-titlebar');
        if (tb) {
            tb.addEventListener('dblclick', function (e) {
                if (e.target.closest('.vx-tb-btn')) return;
                call('vx_window_toggle_max');
            });
        }
    });
})();


/* ============================================================
   动漫 (追番) — 前端逻辑 (window.__vxAn)   【2026-10-07 重构】
   数据链路 (本机实测通过):
     · 最高热度 20 -> anime_hot()        (animeko trends，按"X 万收藏"重排)
     · 推荐无限滑  -> anime_rec_paged()  (animeko recommendations，按 offset 分页)
     · 番剧元数据  -> anime_detail()     (bgmapi.anibt.net，Bangumi 镜像)
     · 资源站      -> anime_sources()    (sub.creamycake.org/v1/{bt1,css1}.json)
     · 搜索        -> anime_search_all() (并发搜前 10 个源后合并)
     · 播放        -> anime_episodes() + anime_resolve()
   本次改动 (按用户要求):
     · 删「发现」并入「探索」；删「弹幕」整页
     · 最高热度固定 20 张卡片，自适应窗口 + 左右斜切层次感
     · 推荐动漫无限下滑
     · 右上角「刷新」改成放大镜搜索
     · 删掉源选择下拉（改成按标题相似度 + 可用性自动选源）
     · 详情页删掉爱心，只留按钮式收藏
     · 修「开始观看」无反应
     · 新播放页：左侧 80% 视频 + 右侧详情（名称/本集简介/数据源可更换/选集下拉/相关推荐）
   ============================================================ */
window.__vxAn = (function () {
    'use strict';
    let inited = false, bound = false;

    let hot = [];                 // 最高热度（固定 20）
    let recs = [];                // 推荐（累积）
    let recOffset = 0;
    let recDone = false, recLoading = false;
    let results = [];             // 搜索结果
    let follow = [];
    let sources = [];             // 源列表（自动选源用）
    let curMeta = null;           // 当前番剧元数据
    let curEpisodes = [];         // Bangumi 剧集（用于右侧"本集简介"）
    let curCandidates = [];       // 自动选出的候选源 [{source,url,title,score}]
    let curCandidateIdx = -1;
    let curOnlineEps = [];        // 当前源站解析出的剧集
    let curEpIdx = 0;
    let curSourceName = '';
    let playSeq = 0;              // 点播序号，防慢响应覆盖新请求
    let curHls = null;            // hls.js 实例（换集/关闭必须 destroy）
    let curMediaToken = null;     // 当前这次挂载的令牌（看门狗用，和 playSeq 分开免得互相干扰）
    // ★ 「继续观看」：非 null 时表示这次 startWatch 是**续播**，要打开这一集并跳到这一秒。
    //   { ep: 集下标, pos: 秒 } —— tryCandidate / openExternalPlayer 都会读它。
    let pendingResume = null;

    const $ = (id) => document.getElementById(id);

    function esc(v) {
        return String(v == null ? '' : v)
            .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    }

    /// 番剧封面（后端给的是 static.myani.org，直接当 img src）
    function cover(c, cls) {
        if (c && c.image) {
            return '<img class="' + (cls || 'an-poster') + '" loading="lazy" referrerpolicy="no-referrer" src="'
                + esc(c.image) + '" onerror="this.style.display=\'none\'">';
        }
        return '<div class="' + (cls || 'an-poster') + ' an-poster-empty"></div>';
    }

    function setStatus(id, text, kind) {
        const el = $(id);
        if (!el) return;
        el.textContent = text || '';
        el.className = 'mod-status' + (kind ? ' is-' + kind : '');
    }

    function logAn(tag, detail) {
        try { invoke('append_frontend_log', { kind: tag, detail: String(detail).slice(0, 400) }); } catch (e) {}
    }

    /// ★ 给 invoke 套一层超时。
    ///   为什么需要：这些资源站在用户网络里经常连不上，Rust 侧要重试 3 次才返回，
    ///   前端就一直卡在「正在试源…」，用户看到的就是"点了没反应"。
    ///   超时后直接判定这条线路不可用，换下一条 / 明确报错。
    function withTimeout(p, ms, label) {
        return Promise.race([
            p,
            new Promise(function (_, rej) {
                setTimeout(function () {
                    rej(new Error((label || '操作') + '超时（' + Math.round(ms / 1000) + ' 秒，该线路可能不可达）'));
                }, ms);
            }),
        ]);
    }

    /// 标题相似度（决定在源站搜到的一堆结果里挑哪个）
    function titleScore(a, b) {
        const norm = (s) => String(s || '').toLowerCase()
            .replace(/[\s\u3000·:：\-—_~～!！?？.。,，'"]/g, '');
        const x = norm(a), y = norm(b);
        if (!x || !y) return 0;
        if (x === y) return 1000;
        if (x.indexOf(y) >= 0 || y.indexOf(x) >= 0) return 700 - Math.abs(x.length - y.length);
        // 公共子串比例
        let best = 0, run = 0;
        for (let i = 0; i < x.length; i++) {
            for (let j = 0; j < y.length; j++) {
                run = 0;
                while (i + run < x.length && j + run < y.length && x[i + run] === y[j + run]) run++;
                if (run > best) best = run;
            }
        }
        return Math.round(best * 60 / Math.max(x.length, y.length));
    }

    // ============================================================
    // 探索：最高热度 20 张卡片（自适应 + 左右斜切层次感）
    // ============================================================
    function hotCardHtml(c, i) {
        // ★ 用户要求：**只有最左和最右两张**斜一点，而且都往左斜；其余一律平放。
        //   之前是按位置线性插值（-7°…+7°），整排看起来像一条斜线，被指"排成一列"。
        const n = hot.length;
        const isEdge = n >= 2 && (i === 0 || i === n - 1);
        const style = isEdge ? ' style="--an-rot:-5deg"' : '';
        return '<div class="an-hot-card" data-an-sub="' + c.id + '" data-an-title="' + esc(c.name_cn || c.name)
            + '"' + style + '>'
            + cover(c, 'an-hot-poster')
            + '<div class="an-hot-rank">' + (i + 1) + '</div>'
            + '<div class="an-hot-info">'
            + '<div class="an-hot-name">' + esc(c.name_cn || c.name) + '</div>'
            + '<div class="an-hot-desc">' + esc(c.desc1 || '') + (c.desc2 ? ' · ' + esc(c.desc2) : '') + '</div>'
            + '</div></div>';
    }

    async function loadHot(force) {
        if (hot.length && !force) return;
        setStatus('an-home-status', '正在读取所有源，筛出最热的 20 部…');
        try {
            const list = await invoke('anime_hot', { limit: 20 }) || [];
            hot = list.slice(0, 20);
            const wall = $('an-trend-wall');
            if (wall) wall.innerHTML = hot.map(hotCardHtml).join('');
            const sub = $('an-trend-sub');
            if (sub) sub.textContent = hot.length ? ('共 ' + hot.length + ' 部 · 按收藏数排序') : '';
            setStatus('an-home-status', hot.length ? '' : '没取到热门列表（网络受限？）', hot.length ? '' : 'err');
            logAn('ANIME_HOT', '加载 ' + hot.length + ' 部');
        } catch (e) {
            setStatus('an-home-status', '加载热门失败: ' + (e && e.message ? e.message : e), 'err');
        }
    }

    // ============================================================
    // 探索：推荐动漫（无限下滑）
    // ============================================================
    function recCardHtml(c) {
        return '<div class="an-rec-card" data-an-sub="' + c.id + '" data-an-title="' + esc(c.name_cn || c.name) + '">'
            + cover(c, 'an-rec-poster')
            + '<div class="an-rec-info">'
            + '<div class="an-rec-name">' + esc(c.name_cn || c.name) + '</div>'
            + '<div class="an-rec-desc">' + esc(c.desc1 || '') + (c.desc2 ? ' · ' + esc(c.desc2) : '') + '</div>'
            + '</div></div>';
    }

    async function loadRecMore() {
        if (recDone || recLoading) return;
        recLoading = true;
        const loader = $('an-rec-loader');
        if (loader) loader.style.display = '';
        try {
            const list = await invoke('anime_rec_paged', { offset: recOffset, limit: 24 }) || [];
            if (!list.length) {
                recDone = true;
                if (loader) loader.style.display = 'none';
            } else {
                recOffset += list.length;
                recs = recs.concat(list);
                const wall = $('an-rec-wall');
                if (wall) wall.insertAdjacentHTML('beforeend', list.map(recCardHtml).join(''));
                const sub = $('an-rec-sub');
                if (sub) sub.textContent = '已加载 ' + recs.length + ' 部 · 往下滑继续';
            }
        } catch (e) {
            setStatus('an-home-status', '加载推荐失败: ' + (e && e.message ? e.message : e), 'err');
            recDone = true;
            if (loader) loader.style.display = 'none';
        } finally {
            recLoading = false;
        }
    }

    function bindRecScroll() {
        const loader = $('an-rec-loader');
        if (!loader || !('IntersectionObserver' in window)) return;
        const io = new IntersectionObserver(function (entries) {
            for (let i = 0; i < entries.length; i++) {
                if (entries[i].isIntersecting) loadRecMore();
            }
        }, { rootMargin: '500px' });
        io.observe(loader);
    }

    // ============================================================
    // 搜索（放大镜）
    // ============================================================
    function toggleSearch(show) {
        const wrap = $('an-search-wrap');
        const main = $('an-explore-main');
        const res = $('an-results-wrap');
        if (!wrap) return;
        const on = (show === undefined) ? wrap.hidden : show;
        wrap.hidden = !on;
        if (main) main.hidden = on;
        if (res) res.hidden = !on;
        if (on) { const i = $('an-search'); if (i) i.focus(); }
        else { results = []; const r = $('an-results'); if (r) r.innerHTML = ''; setStatus('an-search-status', ''); }
    }

    async function doSearch() {
        const inp = $('an-search');
        const kw = inp ? inp.value.trim() : '';
        if (!kw) { setStatus('an-search-status', '请输入关键词', 'err'); return; }
        toggleSearch(true);
        const box = $('an-results');
        if (box) box.innerHTML = '<div class="loading">正在并发搜索所有源…</div>';
        setStatus('an-search-status', '搜索中…');
        try {
            // ★ 源下拉已删：直接并发搜前 10 个源再合并（anime_search_all）
            const rs = await withTimeout(invoke('anime_search_all', { keyword: kw }), 35000, '搜索所有源') || [];
            results = rs;
            const sub = $('an-search-sub');
            if (sub) sub.textContent = rs.length ? ('共 ' + rs.length + ' 条') : '';
            if (box) {
                // ★ 后端现在按「标题+源」去重（好让自动选源有备用线路），所以列表里
                //   同一部番会出现多条。这里**只为显示**按标题再去一次重，
                //   但 data-an-res 仍指向 results 里的原始下标 —— 点进去后还能换源。
                const seenT = {};
                const view = [];
                rs.forEach(function (r, i) {
                    const k = String(r.title || r.name_cn || r.name || '');
                    if (k && seenT[k]) return;
                    if (k) seenT[k] = 1;
                    view.push({ r: r, i: i });
                });
                box.innerHTML = view.length
                    ? view.map(function (v) {
                        const r = v.r;
                        return '<div class="an-result" data-an-res="' + v.i + '">'
                            + cover(r, 'an-result-poster')
                            + '<div class="an-result-info">'
                            + '<div class="an-result-name">' + esc(r.title || r.name_cn || r.name) + '</div>'
                            + '<div class="an-result-desc">' + esc(r.source_name || r.source || '') + (r.desc1 ? ' · ' + esc(r.desc1) : '') + '</div>'
                            + '</div></div>';
                    }).join('')
                    : '<div class="mod-empty">没有搜到结果（这些站在当前网络下可能不可达）</div>';
            }
            setStatus('an-search-status', rs.length ? '' : '没有结果', rs.length ? '' : 'err');
            logAn('ANIME_SEARCH', kw + ' -> ' + rs.length + ' 条');
        } catch (e) {
            setStatus('an-search-status', '搜索失败: ' + (e && e.message ? e.message : e), 'err');
            if (box) box.innerHTML = '<div class="mod-empty">搜索失败</div>';
        }
    }

    // ============================================================
    // 详情
    // ============================================================
    async function openDetail(subId) {
        const ov = $('an-detail');
        const body = $('an-detail-body');
        if (!ov || !body) return;
        ov.hidden = false;
        body.innerHTML = '<div class="loading">加载中…</div>';
        const titleEl = $('an-detail-title');
        if (titleEl) titleEl.textContent = '';
        try {
            const d = await invoke('anime_detail', { id: subId });
            curMeta = (d && d.meta) || null;
            curEpisodes = (d && d.episodes) || [];
            if (titleEl) titleEl.textContent = (curMeta && (curMeta.name_cn || curMeta.name)) || '';
            body.innerHTML = detailHtml(d);
            logAn('ANIME_DETAIL', subId + ' 剧集 ' + curEpisodes.length);
        } catch (e) {
            body.innerHTML = '<div class="mod-empty">加载详情失败: ' + esc(e && e.message ? e.message : e) + '</div>';
        }
    }

    // ============================================================
    // 观看进度记忆（「继续观看」）
    // ------------------------------------------------------------
    // ★ 用户报「动漫看一半，下次打开就没有进度了」。
    //   进度由**播放窗口**写进 localStorage（同源，两个窗口共用一份），
    //   这里只负责读出来渲染"继续观看"，点一下接着上次的地方播。
    // ============================================================
    const AN_PROG_KEY = 'vortex_anime_progress';

    function anProgAll() {
        try {
            const o = JSON.parse(localStorage.getItem(AN_PROG_KEY) || '{}');
            return (o && typeof o === 'object' && !Array.isArray(o)) ? o : {};
        } catch (e) { return {}; }
    }

    function anProgKey(meta, title) {
        const id = meta && meta.id;
        if (id) return 'bgm:' + id;
        const t = String(title || '');
        return t ? 't:' + t : '';
    }

    function fmtClock(sec) {
        const s = Math.max(0, Math.floor(Number(sec) || 0));
        const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), ss = s % 60;
        const p2 = function (n) { return (n < 10 ? '0' : '') + n; };
        return h > 0 ? (h + ':' + p2(m) + ':' + p2(ss)) : (m + ':' + p2(ss));
    }

    /// 渲染「继续观看」一排卡片（按最近观看排序，最多 12 张）
    function renderContinue() {
        const wall = $('an-continue-wall');
        const sec = $('an-continue-sec');
        if (!wall || !sec) return;
        const all = anProgAll();
        const rows = Object.keys(all).map(function (k) { return all[k]; })
            .filter(function (r) { return r && r.title; })
            .sort(function (a, b) { return (b.updatedAt || 0) - (a.updatedAt || 0); })
            .slice(0, 12);
        if (!rows.length) {
            sec.hidden = true;
            wall.innerHTML = '';
            return;
        }
        sec.hidden = false;
        const sub = $('an-continue-sub');
        if (sub) sub.textContent = '共 ' + Object.keys(all).length + ' 部有记录，点一下接着看';
        wall.innerHTML = rows.map(function (r) {
            const dur = Number(r.dur) || 0;
            const pos = Number(r.pos) || 0;
            const pct = (dur > 0) ? Math.min(100, Math.round((pos / dur) * 100)) : 0;
            const epTxt = (r.epCount > 1 ? ('第 ' + (r.epIndex + 1) + ' 集 · ') : '') + fmtClock(pos) + (dur > 0 ? ' / ' + fmtClock(dur) : '');
            return '<button class="an-cont" data-an-cont="' + esc(r.key) + '" title="继续观看：' + esc(r.title) + '">'
                +   '<div class="an-cont-poster">'
                +     cover({ image: r.cover }, 'an-poster')
                +     '<span class="an-cont-play">▶</span>'
                +     '<div class="an-cont-bar"><i style="width:' + pct + '%"></i></div>'
                +   '</div>'
                +   '<div class="an-cont-name">' + esc(r.title) + '</div>'
                +   '<div class="an-cont-sub">' + esc(epTxt) + '</div>'
                + '</button>';
        }).join('');
    }

    /// 点「继续观看」：找到那部番 → 选源 → 从上次的集数和秒数接着播
    async function resumeAnime(key) {
        const rec = anProgAll()[key];
        if (!rec) { showToast('这条观看记录已经没有了'); renderContinue(); return; }
        logAn('ANIME_RESUME_CLICK', key + ' ep=' + rec.epIndex + ' pos=' + rec.pos);
        // 有 Bangumi id 就能直接开详情（进度记录里一定有，除非是纯标题键）
        if (rec.followId) {
            await openDetail(rec.followId);
        } else {
            // 退路：按标题搜一次，挑最像的那个
            try {
                const rs = await withTimeout(invoke('anime_search_all', { keyword: rec.title }), 35000, '搜索所有源') || [];
                if (rs.length) {
                    const best = rs.map(function (r) {
                        return { r: r, s: titleScore(rec.title, r.name_cn || r.name || r.title || '') };
                    }).sort(function (a, b) { return b.s - a.s; })[0];
                    const id = best && best.r && (best.r.id || best.r.subject_id);
                    if (id) await openDetail(Number(id));
                }
            } catch (e) {}
        }
        if (!curMeta) { showToast('找不到这部番的详情，没法续播'); return; }
        pendingResume = { ep: Number(rec.epIndex) || 0, pos: Number(rec.pos) || 0 };
        try {
            await startWatch(true);
        } finally {
            pendingResume = null;
        }
        renderContinue();
    }

    function detailHtml(d) {
        const casts = (d && d.casts) || [];
        const name = m.name_cn || m.name || '';
        const tags = (m.tags || []).slice(0, 10);
        const meta = [];
        if (m.date) meta.push(m.date);
        if (m.rating) meta.push(m.rating + ' 分');
        if (eps.length) meta.push(eps.length + ' 话');
        const inFollow = follow.some(function (f) { return f.id === m.id; });
        return ''
        + '<div class="an-detail-main">'
        +   '<div class="an-detail-left">' + cover(m, 'an-detail-poster') + '</div>'
        +   '<div class="an-detail-right">'
        +     '<h2 class="an-detail-name">' + esc(name) + '</h2>'
        +     (m.name && m.name !== name ? '<div class="an-detail-sub">' + esc(m.name) + '</div>' : '')
        +     (meta.length ? '<div class="an-detail-meta">' + meta.map(function (x) { return '<span>' + esc(x) + '</span>'; }).join('') + '</div>' : '')
        +     (tags.length ? '<div class="an-detail-tags">' + tags.map(function (t) { return '<span class="an-chip">' + esc(t) + '</span>'; }).join('') + '</div>' : '')
        +     '<div class="an-detail-summary">' + esc(m.summary || '（没有简介）') + '</div>'
        +     '<div class="an-detail-actions">'
        +       '<button class="an-watch-btn" data-an-watch="1">开始观看</button>'
        +       '<button class="btn btn-secondary" data-an-fav="1">' + (inFollow ? '已收藏' : '收藏') + '</button>'
        +     '</div>'
        +   '</div>'
        + '</div>'
        + '<div class="an-online" id="an-online" hidden>'
        +   '<div class="an-online-head"><span>资源站</span><span class="an-online-hint" id="an-online-hint"></span></div>'
        +   '<div class="an-online-list" id="an-online-list"></div>'
        + '</div>'
        + (casts.length
            ? '<div class="an-casts"><h4>角色 / 制作</h4><div class="an-cast-list">'
              + casts.slice(0, 24).map(function (c) {
                    return '<div class="an-cast"><span class="an-cast-name">' + esc(c.name || '') + '</span>'
                        + '<span class="an-cast-actor">' + esc(c.actor || '') + '</span></div>';
                }).join('') + '</div></div>'
            : '')
        + (eps.length
            ? '<div class="an-eps"><h4>剧集</h4><div class="an-eps-grid">'
              + eps.map(function (e, i) {
                    return '<button class="an-ep" data-an-ep="' + i + '" title="' + esc(e.name || '') + '">'
                        + esc(e.ep != null ? e.ep : (i + 1)) + '</button>';
                }).join('') + '</div></div>'
            : '');
    }

    // ============================================================
    // 自动选源 + 开始观看
    //
    // ★ 用户要求：源下拉删掉，改由「链接延迟 + 数据」自动选。
    //   做法：并发搜所有源 → 按标题相似度排序 → 逐个试 anime_episodes，
    //   谁先能拿到剧集就用谁（拿到剧集才算"数据可用"）。
    // ============================================================
    let startWatchBusy = false;

    async function startWatch(resume) {
        // ★ 重入守卫：实测出现过同一部番几秒内被连续发起 4 次（竞态，复现不稳定），
        //   每次都会重开播放窗口并从头重播。连点/重复触发只认第一次。
        if (startWatchBusy) return;
        startWatchBusy = true;
        // 不是续播就清掉续播意图（否则会莫名其妙跳到上次的位置）
        if (!resume) pendingResume = null;
        try {
            return await startWatchInner();
        } finally {
            startWatchBusy = false;
        }
    }

    async function startWatchInner() {
        const list = $('an-online-list');
        const box = $('an-online');
        if (!list || !box) {
            setStatus('an-search-status', '资源面板没找到（详情没渲染完？）', 'err');
            return;
        }
        box.hidden = false;
        box.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
        list.innerHTML = '<div class="loading">正在并发搜索所有源，自动挑选可用线路…</div>';
        setStatus('an-online-hint', '');
        const name = (curMeta && (curMeta.name_cn || curMeta.name)) || '';

        try {
            if (!sources.length) {
                try { sources = await invoke('anime_sources') || []; } catch (e) { sources = []; }
            }
            logAn('ANIME_SEARCH_START', name);   // 和后面的 ANIME_AUTOSRC 配对看搜源耗时
            const rs = await withTimeout(invoke('anime_search_all', { keyword: name }), 35000, '搜索所有源') || [];
            if (!rs.length) {
                list.innerHTML = '<div class="mod-empty">所有源都没搜到这部番（当前网络下这些站可能不可达）</div>';
                setStatus('an-online-hint', '没有可用线路', 'err');
                return;
            }
            // 按标题相似度排序，取前 6 个候选
            const scored = rs.map(function (r) {
                return { r: r, s: titleScore(name, r.name_cn || r.name || r.title || '') };
            }).sort(function (a, b) { return b.s - a.s; });
            curCandidates = scored.slice(0, 6).map(function (x) { return x.r; });
            logAn('ANIME_AUTOSRC', '候选 ' + curCandidates.length + ' 个: '
                + curCandidates.map(function (c) { return (c.source_name || c.source || '') + '(' + titleScore(name, c.name_cn || c.name || c.title || '') + ')'; }).join(', '));

            list.innerHTML = curCandidates.map(function (c, i) {
                return '<button class="an-src" data-an-src="' + i + '">'
                    + '<span class="an-src-name">' + esc(c.source_name || c.source || '源' + (i + 1)) + '</span>'
                    + '<span class="an-src-sub">' + esc(c.name_cn || c.name || c.title || '') + '</span>'
                    + '<span class="an-src-score">' + titleScore(name, c.name_cn || c.name || c.title || '') + '</span>'
                    + '</button>';
            }).join('');
            setStatus('an-online-hint', '按标题匹配度排序，点一个开始；也可以直接点「自动播放」', '');
            list.insertAdjacentHTML('beforeend',
                '<button class="an-src an-src-auto" data-an-auto="1"><span class="an-src-name">自动播放（逐个试到能播为止）</span></button>');

            // 自动试：从第一个开始逐个往后试，直到能播
            const ok = await tryCandidate(0, true);
            if (!ok) {
                setStatus('an-online-hint',
                    '自动试了 ' + curCandidates.length + ' 个源都没成功。可以点上面某一条手动再试，'
                    + '或稍后重试（这些资源站在当前网络下可能不可达）', 'err');
            }
        } catch (e) {
            list.innerHTML = '<div class="mod-empty">搜索资源失败: ' + esc(e && e.message ? e.message : e) + '</div>';
            setStatus('an-online-hint', String(e && e.message ? e.message : e), 'err');
        }
    }

    /// 把整份播放会话交给**独立播放窗口**。
    ///
    /// ★ 用户要求「视频播放单独开一个窗口，不要在软件里面」。所以主窗口只负责
    ///   "搜源 + 挑出能用的线路"，真正播放交给 player.html 那个窗口。
    ///   返回 false 表示窗口没开起来，调用方可以退回软件内播放。
    async function openExternalPlayer(epIdx) {
        try {
            const nm = (curMeta && (curMeta.name_cn || curMeta.name)) || '';
            const eps = curOnlineEps || [];
            const ep = eps[epIdx] || {};
            // 相关推荐：和软件内播放页用的是同一份池子
            const cur = curMeta && curMeta.id;
            const pool = hot.concat(recs).filter(function (c) { return c.id !== cur; });
            const seen = {};
            const related = [];
            for (let i = 0; i < pool.length && related.length < 10; i++) {
                if (!seen[pool[i].id]) {
                    seen[pool[i].id] = 1;
                    related.push({
                        id: pool[i].id,
                        name_cn: pool[i].name_cn || '',
                        name: pool[i].name || '',
                        image: pool[i].image || '',
                        desc: pool[i].desc1 || '',
                    });
                }
            }
            const payload = {
                title: nm,
                // 播放窗口右侧面板的封面头图 / 副标题
                cover: (curMeta && curMeta.image) || '',
                subtitle: (curMeta && curMeta.name && curMeta.name !== nm) ? curMeta.name : '',
                followId: (curMeta && curMeta.id) || 0,
                // ★ 观看进度记忆的键（播放窗口用它存/取进度）
                subKey: anProgKey(curMeta, nm),
                // ★ 续播位置（秒）：只有"就是这一集"时才有值，播放窗口拿到就 seek 过去
                resumePos: (pendingResume && pendingResume.ep === epIdx) ? pendingResume.pos : 0,
                epIndex: epIdx,
                sourceName: curSourceName,
                sourceUrl: (curCandidates[curCandidateIdx] || {}).source_url || '',
                candidates: (curCandidates || []).map(function (c) {
                    return {
                        name: c.name, title: c.title,
                        source_name: c.source_name, source: c.source,
                        source_url: c.source_url, url: c.url,
                    };
                }),
                candidateIdx: curCandidateIdx,
                episodes: eps.map(function (e) {
                    return { channel: e.channel, name: e.name, sort: e.sort, url: e.url };
                }),
                // 每集简介来自 Bangumi 剧集列表（下标大致对齐）
                epSummaries: eps.map(function (e, i) {
                    const b = curEpisodes[i] || {};
                    return b.summary || b.name || '';
                }),
                related: related,
                epName: ep.name || '',
            };
            await invoke('open_player_window', { payload: payload });
            setStatus('an-online-hint',
                '已在独立播放窗口打开：' + nm + '（' + curSourceName + '，共 ' + eps.length + ' 集）', '');
            logAn('ANIME_PLAYER_WINDOW', nm + ' src=' + curSourceName + ' eps=' + eps.length);
            return true;
        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            logAn('ANIME_PLAYER_WINDOW_FAIL', msg);
            showToast('独立播放窗口打不开，改在软件内播放');
            return false;
        }
    }

    /// 试第 idx 个候选源；silent=true 时失败不报错，继续试下一个
    async function tryCandidate(idx, silent) {
        if (idx < 0 || idx >= curCandidates.length) {
            // ★ 静默模式也必须给出结论，否则界面永远停在"正在试源…"
            setStatus('an-online-hint',
                '所有候选源都不可用（' + curCandidates.length + ' 个都试过了）—— 这些站在当前网络下多半连不上',
                'err');
            return false;
        }
        const c = curCandidates[idx];
        const srcLabel = c.source_name || c.source || ('源' + (idx + 1));
        setStatus('an-online-hint', '正在试 ' + srcLabel + ' …');
        try {
            const eps = await withTimeout(
                invoke('anime_episodes', { sourceUrl: c.source_url, pageUrl: c.url }), 25000, '解析剧集') || [];
            if (!eps.length) {
                logAn('ANIME_SRC_EMPTY', srcLabel + ' 没解析到剧集');
                if (silent) return await tryCandidate(idx + 1, true);
                setStatus('an-online-hint', srcLabel + ' 没有解析到剧集', 'err');
                return false;
            }
            curCandidateIdx = idx;
            curOnlineEps = eps;
            curSourceName = srcLabel;
            setStatus('an-online-hint', curSourceName + ' 可用，共 ' + eps.length + ' 集', '');
            // ★ 播放交给独立窗口；只有窗口开不起来才退回软件内
            //   「继续观看」进来时 pendingResume.ep 就是要续播的那一集
            const wantEp = (pendingResume && pendingResume.ep >= 0 && pendingResume.ep < eps.length)
                ? pendingResume.ep : 0;
            const opened = await openExternalPlayer(wantEp);
            if (!opened) await playEpisode(wantEp);
            return true;
        } catch (e) {
            const msg = e && e.message ? e.message : String(e);
            logAn('ANIME_SRC_FAIL', srcLabel + ' ' + msg);
            if (silent) return await tryCandidate(idx + 1, true);
            setStatus('an-online-hint', srcLabel + ' 失败: ' + msg, 'err');
            return false;
        }
    }

    // ============================================================
    // 播放页（左 80% 视频 + 右详情）
    // ============================================================
    function showPlay() {
        const p = $('an-play');
        if (p) p.hidden = false;
        const d = $('an-detail');
        if (d) d.hidden = true;
    }

    function hidePlay() {
        const p = $('an-play');
        if (p) p.hidden = true;
        // 关播放就停超分：省 GPU（档位记着，下次播会自动再开）
        try { if (window.__vxAnime4K) window.__vxAnime4K.detach(); } catch (e) {}
        const c = $('an-sr-canvas');
        if (c) c.hidden = true;
        updateSrFps();
        const v = $('an-player');
        if (v) { try { v.pause(); } catch (e) {} v.removeAttribute('src'); v.load(); }
        detachHls();
    }

    function detachHls() {
        if (curHls) {
            try { curHls.destroy(); } catch (e) {}
            curHls = null;
        }
    }

    // ============================================================
    // 实时超分（Anime4K / WebGL2，见 anime4k.js）
    // ============================================================
    //
    // 着色器链和 Kazumi（Predidit/Kazumi，MIT）用的完全一致，只是它挂在 mpv 上、
    // 我们挂在 WebView2 的 WebGL2 上。档位含义：
    //   efficiency 效率档 = Clamp + Restore_CNN_M + Restore_CNN_S + Upscale_x2_M + Upscale_x2_S
    //   quality    质量档 = Clamp + Restore_CNN_VL + Upscale_x2_VL + Upscale_x2_M
    // ★ 这些 CNN 只会在"显示尺寸 >= 源尺寸 1.2 倍"时才跑（着色器里的 //!WHEN），
    //   所以源本来就比画面大的时候开超分不会有变化，这是设计如此。
    let srMode = 'off';
    let srTimer = null;

    function srSupported() {
        return !!(window.__vxAnime4K && window.__vxAnime4K.supported().ok);
    }

    function updateSrFps() {
        const el = $('an-sr-fps');
        if (!el) return;
        if (srMode === 'off' || !window.__vxAnime4K) { el.textContent = ''; return; }
        const f = window.__vxAnime4K.fps();
        el.textContent = f ? (f + ' fps') : '';
    }

    async function applySr() {
        const v = $('an-player');
        const c = $('an-sr-canvas');
        if (!v || !c) return;
        const want = srMode;
        try {
            if (want === 'off') {
                if (window.__vxAnime4K) window.__vxAnime4K.detach();
                if (srTimer) { clearInterval(srTimer); srTimer = null; }
                c.hidden = true;
                updateSrFps();
                return;
            }
            if (!window.__vxAnime4K) {
                throw new Error('anime4k.js 没加载');
            }
            const sup = window.__vxAnime4K.supported();
            if (!sup.ok) throw new Error(sup.why);
            setStatus('an-player-status', '正在加载 Anime4K 着色器…', '');
            const n = await window.__vxAnime4K.attach(v, c, want);
            if (srMode !== want) { window.__vxAnime4K.detach(); return; }   // 期间被改过
            c.hidden = false;
            if (srTimer) clearInterval(srTimer);
            srTimer = setInterval(updateSrFps, 1000);
            setStatus('an-player-status',
                '超分已开启（' + (want === 'quality' ? '质量档' : '效率档') + '，' + n + ' 趟着色器）', '');
            logAn('ANIME_SR', 'mode=' + want + ' passes=' + n);
        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            try { if (window.__vxAnime4K) window.__vxAnime4K.detach(); } catch (_) {}
            if (srTimer) { clearInterval(srTimer); srTimer = null; }
            c.hidden = true;
            srMode = 'off';
            const sel = $('an-sr-mode');
            if (sel) sel.value = 'off';
            setStatus('an-player-status', '超分开不起来：' + msg, 'err');
            logAn('ANIME_SR_FAIL', msg);
        }
    }

    /// 超分用的是 canvas 盖住 video，原生控制条会被挡住，所以自己画一套
    function fmtTime(t) {
        t = Math.max(0, Math.floor(Number(t) || 0));
        const m = Math.floor(t / 60), sec = t % 60;
        return m + ':' + (sec < 10 ? '0' : '') + sec;
    }

    function syncPlayerUi() {
        const v = $('an-player');
        if (!v) return;
        const btn = $('an-play-toggle-icon');
        if (btn) {
            btn.innerHTML = v.paused
                ? '<polygon points="6 4 20 12 6 20"/>'
                : '<rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/>';
        }
        const seek = $('an-play-seek');
        if (seek) {
            const d = Number(v.duration) || 0;
            const cur = Number(v.currentTime) || 0;
            // 拖动时别抢用户的输入
            if (!seek.dataset.dragging) {
                seek.value = d > 0 ? Math.round((cur / d) * 1000) : 0;
            }
        }
        const t = $('an-play-time');
        if (t) t.textContent = fmtTime(v.currentTime) + ' / ' + fmtTime(v.duration);
        const mi = $('an-play-mute-icon');
        if (mi) {
            mi.innerHTML = v.muted || v.volume === 0
                ? '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19"/><line x1="22" y1="9" x2="16" y2="15"/><line x1="16" y1="9" x2="22" y2="15"/>'
                : '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M18.5 5.5a9 9 0 0 1 0 13"/>';
        }
    }

    let playerUiBound = false;
    function bindPlayerUi() {
        const v = $('an-player');
        if (!v || playerUiBound) return;
        playerUiBound = true;
        v.controls = false;   // 统一用自带控制条
        const seek = $('an-play-seek');
        if (seek) {
            seek.addEventListener('input', function () {
                seek.dataset.dragging = '1';
                const d = Number(v.duration) || 0;
                if (d > 0) v.currentTime = (Number(seek.value) / 1000) * d;
            });
            seek.addEventListener('change', function () { delete seek.dataset.dragging; });
            seek.addEventListener('pointerup', function () { delete seek.dataset.dragging; });
        }
        v.addEventListener('timeupdate', syncPlayerUi);
        v.addEventListener('durationchange', syncPlayerUi);
        v.addEventListener('play', syncPlayerUi);
        v.addEventListener('pause', syncPlayerUi);
        v.addEventListener('volumechange', syncPlayerUi);
        syncPlayerUi();
    }

    async function playEpisode(idx) {
        if (idx < 0 || idx >= curOnlineEps.length) return;
        curEpIdx = idx;
        const ep = curOnlineEps[idx];
        showPlay();
        const seq = ++playSeq;

        // 右侧面板
        const nm = (curMeta && (curMeta.name_cn || curMeta.name)) || '';
        const pt = $('an-play-title');
        if (pt) pt.textContent = nm + ' · 第 ' + (idx + 1) + ' 集';
        const sn = $('an-side-name');
        if (sn) sn.textContent = nm;
        const se = $('an-side-ep');
        if (se) se.textContent = '第 ' + (idx + 1) + ' 集' + (ep.name ? ' · ' + ep.name : '');
        const sd = $('an-side-desc');
        if (sd) {
            const bep = curEpisodes[idx] || {};
            sd.textContent = bep.summary || bep.name || '（这一集没有简介）';
        }
        const sc = $('an-player-src');
        if (sc) sc.textContent = curSourceName;
        const ssn = $('an-side-src-name');
        if (ssn) ssn.textContent = curSourceName || '—';
        renderEpSelect();
        renderRelated();

        setStatus('an-player-status', '正在解析播放地址…');
        const v = $('an-player');
        try {
            const info = await withTimeout(
                invoke('anime_resolve', {
                    sourceUrl: ep.source_url || curCandidates[curCandidateIdx].source_url,
                    pageUrl: ep.url,
                }), 25000, '解析播放地址') || {};
            if (seq !== playSeq) return;   // 有更新的点播了，丢弃
            if (!info.url) throw new Error('没解析出播放地址');
            logAn('ANIME_PLAY', 'ep=' + idx + ' src=' + curSourceName + ' url=' + String(info.url).slice(0, 120));
            setStatus('an-player-status', '');
            attachMedia(v, info.url, info);
            bindPlayerUi();
            syncPlayerUi();
            if (srMode !== 'off') applySr();
        } catch (e) {
            if (seq !== playSeq) return;
            const msg = e && e.message ? e.message : String(e);
            setStatus('an-player-status', '播放失败: ' + msg + '（可在右侧「数据源」换一个线路）', 'err');
            logAn('ANIME_PLAY_FAIL', 'ep=' + idx + ' ' + msg);
        }
    }

    function renderEpSelect() {
        const sel = $('an-side-ep-select');
        if (!sel) return;
        sel.innerHTML = curOnlineEps.map(function (e, i) {
            const label = '第 ' + (i + 1) + ' 集' + (e.name ? ' · ' + String(e.name).slice(0, 24) : '');
            return '<option value="' + i + '"' + (i === curEpIdx ? ' selected' : '') + '>' + esc(label) + '</option>';
        }).join('');
    }

    function renderRelated() {
        const box = $('an-side-rel-list');
        if (!box) return;
        const cur = curMeta && curMeta.id;
        const pool = hot.concat(recs).filter(function (c) { return c.id !== cur; });
        const seen = {};
        const pick = [];
        for (let i = 0; i < pool.length && pick.length < 8; i++) {
            if (!seen[pool[i].id]) { seen[pool[i].id] = 1; pick.push(pool[i]); }
        }
        box.innerHTML = pick.map(function (c) {
            return '<div class="an-rel-item" data-an-sub="' + c.id + '" data-an-title="' + esc(c.name_cn || c.name) + '">'
                + cover(c, 'an-rel-poster')
                + '<div class="an-rel-info">'
                + '<div class="an-rel-name">' + esc(c.name_cn || c.name) + '</div>'
                + '<div class="an-rel-desc">' + esc(c.desc1 || '') + (c.desc2 ? ' · ' + esc(c.desc2) : '') + '</div>'
                + '</div></div>';
        }).join('');
    }

    /// 换源：候选列表里往后轮一个
    function cycleSource() {
        if (curCandidates.length < 2) {
            setStatus('an-player-status', '只有一条候选线路可换', 'err');
            return;
        }
        const next = (curCandidateIdx + 1) % curCandidates.length;
        setStatus('an-player-status', '正在换到 ' + (curCandidates[next].source_name || '下一个源') + ' …');
        tryCandidate(next, false);
    }

    function attachMedia(v, url, info) {
        if (!v) return;
        const isHls = /\.m3u8(\?|$)/i.test(url) || /m3u8/i.test(url);
        const hasHls = !!(window.Hls && window.Hls.isSupported());
        detachHls();
        if (isHls && hasHls) {
            // ★★ enableWorker 必须是 false（2026-10-07 实测）。
            //   hls.js 1.5.20 在 worker 模式下分片载荷会变成 **0 字节**：
            //   同样的 URL，worker 开 → FRAG_LOADED bytes=0；worker 关 → bytes=555168。
            //   结果就是「能播放但画面一直黑的」，而且 hls.js **一个 ERROR 事件都不报**，
            //   所以之前完全看不出原因。别改回 true。
            const hls = new window.Hls({ maxBufferLength: 30, enableWorker: false, lowLatencyMode: false });
            curHls = hls;
            let netFails = 0;
            hls.on(window.Hls.Events.ERROR, function (_, data) {
                if (!data) return;
                if (!data.fatal) return;
                if (data.type === window.Hls.ErrorTypes.NETWORK_ERROR && netFails < 3) {
                    // 分片偶发 404/超时时重试几次；超过就报出来，别再无声地转
                    netFails++;
                    setStatus('an-player-status', '网络不稳，重试第 ' + netFails + ' 次…', '');
                    try { hls.startLoad(); } catch (e) {}
                } else if (data.type === window.Hls.ErrorTypes.MEDIA_ERROR) {
                    try { hls.recoverMediaError(); } catch (e) {}
                } else {
                    setStatus('an-player-status',
                        '播放出错：' + (data.details || data.type || '未知')
                        + (data.response && data.response.code ? '（HTTP ' + data.response.code + '）' : '')
                        + '　—— 可以点右侧「更换」换个源', 'err');
                }
            });
            hls.loadSource(url);
            hls.attachMedia(v);
            hls.on(window.Hls.Events.MANIFEST_PARSED, function () {
                // ★ 自动播放可能被拦（WebView2 默认要求用户手势，而解析播放地址是异步的，
                //   点「开始观看」那一下的手势早就过期了）。
                //   拦截时先**静音**再试一次 —— 静音自动播放一定被允许，总比黑屏强。
                //   （同时 tauri.conf.json 里已经加了 --autoplay-policy=no-user-gesture-required）
                const tryPlay = function () {
                    const p = v.play();
                    return (p && p.catch) ? p : Promise.resolve();
                };
                tryPlay().catch(function (err) {
                    v.muted = true;
                    tryPlay().then(function () {
                        setStatus('an-player-status', '已静音自动播放，点画面右下角喇叭开声音', '');
                    }).catch(function () {
                        setStatus('an-player-status',
                            '浏览器拦了自动播放（' + ((err && err.name) || '') + '），点一下画面中间的播放键', '');
                    });
                });
            });
            // ★ 看门狗：12 秒还没起来就明确告诉用户，别让人对着黑框猜
            const token = {};
            curMediaToken = token;
            setTimeout(function () {
                if (curMediaToken !== token) return;
                if (v.readyState === 0 && !(v.buffered && v.buffered.length)) {
                    setStatus('an-player-status',
                        '画面一直起不来：这个源的分片取不到或已失效，点右侧「更换」换个源试试', 'err');
                }
            }, 12000);
        } else if (isHls) {
            setStatus('an-player-status', 'hls.js 没加载出来，无法播放 m3u8', 'err');
        } else {
            v.src = url;
            v.load();
            const p = v.play();
            if (p && p.catch) p.catch(function () {});
        }
    }

    // ============================================================
    // 追番
    // ============================================================
    async function loadFollow() {
        try {
            follow = await invoke('anime_follow') || [];
        } catch (e) { follow = []; }
        const box = $('an-follow-list');
        if (box) {
            box.innerHTML = follow.length
                ? follow.map(function (f) {
                    // ★ 用户要求删掉「记进度」：追番列表只保留 详情 / 取消，
                    //   连「看到第 N 集」那行也不显示（不再记观看进度）。
                    return '<div class="an-follow-item">'
                        + cover({ image: f.image }, 'an-follow-poster')
                        + '<div class="an-follow-info">'
                        + '<div class="an-follow-name">' + esc(f.name) + '</div>'
                        + '</div>'
                        + '<div class="an-follow-actions">'
                        + '<button class="btn btn-secondary" data-an-open="' + f.id + '">详情</button>'
                        + '<button class="btn btn-secondary" data-an-unfollow="' + f.id + '">取消</button>'
                        + '</div></div>';
                }).join('')
                : '<div class="mod-empty">还没有追番，去详情页点「收藏」</div>';
        }
        setStatus('an-follow-status', follow.length ? ('共 ' + follow.length + ' 部') : '');
    }

    async function toggleFollowMeta() {
        if (!curMeta) return;
        const item = {
            id: curMeta.id, name: curMeta.name_cn || curMeta.name,
            image: curMeta.image || '', progress: 0,
        };
        try {
            follow = await invoke('anime_follow_toggle', { item: item }) || [];
            showToast(follow.some(function (f) { return f.id === item.id; }) ? '已收藏' : '已取消收藏');
            const body = $('an-detail-body');
            if (body && curMeta) {
                // 只更新收藏按钮文字，不重渲染整个详情
                const b = body.querySelector('[data-an-fav]');
                if (b) b.textContent = follow.some(function (f) { return f.id === item.id; }) ? '已收藏' : '收藏';
            }
            loadFollow();
        } catch (e) {
            showToast('收藏失败: ' + (e && e.message ? e.message : e));
        }
    }

    async function setProgress(subId) {
        const v = prompt('看到第几集？', '1');
        if (v == null) return;
        const n = parseInt(v, 10);
        if (!n || n < 1) return;
        try {
            follow = await invoke('anime_follow_progress', { id: subId, progress: n }) || [];
            loadFollow();
            showToast('已记录进度');
        } catch (e) { showToast('记录失败: ' + (e && e.message ? e.message : e)); }
    }

    async function unfollow(subId) {
        const f = follow.find(function (x) { return x.id === subId; });
        if (!f) return;
        try {
            follow = await invoke('anime_follow_toggle', { item: f }) || [];
            loadFollow();
        } catch (e) { showToast('取消失败: ' + (e && e.message ? e.message : e)); }
    }

    // ============================================================
    // 事件
    // ============================================================
    function bind() {
        if (bound) return;
        bound = true;

        // ---- 独立播放窗口回传的事件 ----
        //   播放窗口是另一个 webview，只能靠事件通信（见 player.js 顶部的注释）。
        (function () {
            const T = window.__TAURI__ && window.__TAURI__.event;
            if (!T || !T.listen) return;
            // 看到第几集 → 记进追番进度
            T.listen('player-progress', function (ev) {
                const d = (ev && ev.payload) || {};
                const id = Number(d.id), pg = Number(d.progress);
                if (!id || !pg) return;
                invoke('anime_follow_progress', { id: id, progress: pg })
                    .then(function (list) { if (list) follow = list; loadFollow(); })
                    .catch(function () {});
            }).catch(function () {});
            // 在播放窗口点了"相关推荐" → 主窗口切到动漫页并打开详情
            T.listen('player-open-subject', function (ev) {
                const d = (ev && ev.payload) || {};
                if (!d.id) return;
                try {
                    const pg = $('page-anime');
                    if (pg) {
                        document.querySelectorAll('.page').forEach(function (x) { x.classList.remove('active'); });
                        pg.classList.add('active');
                        document.querySelectorAll('.nav-btn').forEach(function (b) { b.classList.remove('active'); });
                    }
                } catch (e) {}
                openDetail(Number(d.id));
            }).catch(function () {});
        })();


        // ---- 超分档位（记忆在 localStorage） ----
        const srSel = $('an-sr-mode');
        if (srSel) {
            try {
                const saved = localStorage.getItem('vx_an_sr_mode');
                if (saved === 'efficiency' || saved === 'quality' || saved === 'off') {
                    srMode = saved;
                    srSel.value = saved;
                }
            } catch (e) {}
            srSel.addEventListener('change', function () {
                srMode = srSel.value || 'off';
                try { localStorage.setItem('vx_an_sr_mode', srMode); } catch (e) {}
                logAn('ANIME_SR_MODE', srMode);
                applySr();
            });
        }

        // ---- 自带控制条 ----
        const toggleBtn = $('an-play-toggle');
        if (toggleBtn) {
            toggleBtn.addEventListener('click', function () {
                const v = $('an-player');
                if (!v) return;
                if (v.paused) { const p = v.play(); if (p && p.catch) p.catch(function () {}); }
                else { try { v.pause(); } catch (e) {} }
            });
        }
        const muteBtn = $('an-play-mute');
        if (muteBtn) {
            muteBtn.addEventListener('click', function () {
                const v = $('an-player');
                if (!v) return;
                v.muted = !v.muted;
            });
        }

        // ★ 热度墙是「横着一行」，鼠标滚轮上下滚时把它横着滚 ——
        //   否则用户得去够那条细滚动条。滚到头就把事件放出去，页面照常滚。
        const hotWall = $('an-trend-wall');
        if (hotWall) {
            hotWall.addEventListener('wheel', function (e) {
                if (e.deltaY === 0) return;
                const max = hotWall.scrollWidth - hotWall.clientWidth;
                if (max <= 1) return;                       // 没得横滚，放行
                const atStart = hotWall.scrollLeft <= 0 && e.deltaY < 0;
                const atEnd = hotWall.scrollLeft >= max - 1 && e.deltaY > 0;
                if (atStart || atEnd) return;               // 到头了，让页面自己滚
                hotWall.scrollLeft += e.deltaY;
                e.preventDefault();
            }, { passive: false });
        }

        document.addEventListener('click', function (e) {
            // 打开详情（热度卡 / 推荐卡 / 相关推荐 / 搜索结果）
            const sub = e.target.closest('[data-an-sub]');
            if (sub) { openDetail(Number(sub.dataset.anSub)); return; }
            const res = e.target.closest('[data-an-res]');
            if (res) {
                // ★ 搜索结果来自资源站（SubjectItem: title/url/source_url），**没有 Bangumi id**，
                //   所以不能走 openDetail。直接把这一个条目当成唯一候选开播。
                const r = results[Number(res.dataset.anRes)];
                if (r) {
                    const t = r.title || r.name_cn || r.name || '';
                    if (!curMeta || !(curMeta.name_cn || curMeta.name)) {
                        curMeta = { name_cn: t, name: t, summary: '', image: r.image || '' };
                    }
                    curCandidates = [r];
                    curCandidateIdx = 0;
                    curSourceName = r.source_name || r.source || '资源站';
                    const box = $('an-online');
                    if (box) box.hidden = false;
                    const l = $('an-online-list');
                    if (l) {
                        l.innerHTML = '<button class="an-src is-active" data-an-src="0">'
                            + '<span class="an-src-name">' + esc(curSourceName) + '</span>'
                            + '<span class="an-src-sub">' + esc(t) + '</span></button>';
                    }
                    logAn('ANIME_RES_OPEN', curSourceName + ' ' + r.url);
                    tryCandidate(0, false);
                }
                return;
            }
            if (e.target.closest('[data-an-watch]')) { startWatch(); return; }
            // ★ 「继续观看」卡片：从上次的集数 + 秒数接着播
            const cont = e.target.closest('[data-an-cont]');
            if (cont) { resumeAnime(cont.dataset.anCont); return; }
            if (e.target.closest('[data-an-fav]')) { toggleFollowMeta(); return; }
            const src = e.target.closest('[data-an-src]');
            if (src) { tryCandidate(Number(src.dataset.anSrc), false); return; }
            if (e.target.closest('[data-an-auto]')) { tryCandidate(0, true); return; }
            const ep = e.target.closest('[data-an-ep]');
            if (ep) {
                // 详情页的剧集按钮：先自动选源再播这一集
                (async function () {
                    await startWatch();
                    const want = Number(ep.dataset.anEp);
                    if (curOnlineEps.length > want) playEpisode(want);
                })();
                return;
            }
            const open = e.target.closest('[data-an-open]');
            if (open) { openDetail(Number(open.dataset.anOpen)); return; }
            // ★ 「记进度」按钮已按用户要求移除（追番不再记录观看进度）
            const unf = e.target.closest('[data-an-unfollow]');
            if (unf) { unfollow(Number(unf.dataset.anUnfollow)); return; }
        });

        const bind2 = function (id, ev, fn) { const el = $(id); if (el) el.addEventListener(ev, fn); };
        bind2('an-detail-back', 'click', function () { const d = $('an-detail'); if (d) d.hidden = true; });
        bind2('an-play-back', 'click', function () { hidePlay(); const d = $('an-detail'); if (d) d.hidden = false; });
        bind2('an-play-close', 'click', function () { hidePlay(); const d = $('an-detail'); if (d) d.hidden = false; });
        bind2('an-side-src-btn', 'click', cycleSource);
        bind2('an-search-toggle', 'click', function () {
            const w = $('an-search-wrap');
            toggleSearch(w ? w.hidden : true);
        });
        bind2('an-search-btn', 'click', doSearch);
        bind2('an-search-cancel', 'click', function () { toggleSearch(false); });
        bind2('an-search', 'keypress', function (e) { if (e.key === 'Enter') doSearch(); });
        bind2('an-follow-refresh', 'click', loadFollow);
        bind2('an-side-ep-select', 'change', function (e) {
            const i = parseInt(e.target.value, 10);
            if (!isNaN(i)) playEpisode(i);
        });
        bindRecScroll();
        // 播放窗口在别处写进度（localStorage 同源共享），主窗口一被激活就重画"继续观看"
        window.addEventListener('focus', function () { try { renderContinue(); } catch (e) {} });
    }

    async function tabShown(tab) {
        if (!inited) { inited = true; bind(); }
        if (tab === 'follow') { await loadFollow(); return; }
        if (tab === 'explore') {
            // 先画「继续观看」（纯本地读，瞬间出）——播放窗口可能刚更新过进度
            renderContinue();
            // ★ 「最高热度」那排已按用户要求删掉 → 不再拉 anime_hot（那是一次
            //   全源聚合请求，白等几秒）。探索页直接推推荐列表。
            if (!recs.length) await loadRecMore();
            return;
        }
    }

    return {
        init: function () { tabShown('explore'); },
        tabShown: tabShown,
        // 播放窗口报进度后主窗口刷新"继续观看"
        refreshContinue: renderContinue,
    };
})();


/* ============================================================
   书库 — 前端逻辑 (window.__vxBk)
   ------------------------------------------------------------
   两页：主页（推书 + 搜索）/ 收藏；书可在线阅读，也可下载到本地。
   ★ 源全部逐个实测过（见 books.rs 顶部注释）：
     · gutenberg — Project Gutenberg（Gutendex 开放 API）：外文名著 79k +
       **中文公版书 444 本**（西遊記 / 紅樓夢 / 警世通言 / 唐诗三百首 …）
     · guoxue    — 5000yan.com 国学经典全文
     · shuge     — 书格 shuge.org 古籍善本（可读正文 + 下载 PDF）
     · local     — 本地导入（把 txt/epub 丢进 <软件目录>\data\books\local）
   收藏和阅读进度都存 localStorage（同源共享，不占后端）。
   ============================================================ */
window.__vxBk = (function () {
    'use strict';

    const FAV_KEY = 'vortex_book_fav';
    const PROG_KEY = 'vortex_book_progress';
    const FONT_KEY = 'vortex_book_font';

    const SOURCES = [
        { id: 'gutenberg', name: '热门名著', home: 'popular', hint: 'Project Gutenberg 按下载量排序（外文名著为主）' },
        { id: 'gutenberg_zh', name: '中文经典', home: 'zh', hint: 'Gutenberg 上的中文公版书：西遊記 / 紅樓夢 / 警世通言 / 唐诗三百首 …' },
        { id: 'guoxue', name: '国学经典', home: 'guoxue', hint: '5000yan.com：道德经 / 论语 / 诗经 等全文' },
        { id: 'shuge', name: '古籍善本', home: null, hint: '书格 shuge.org：古籍影印本，正文可读，页内可下 PDF（用搜索）' },
        { id: 'local', name: '本地书', home: 'local', hint: '放在 <软件目录>\\data\\books\\local 里的 txt / epub' },
    ];

    let inited = false;
    let bound = false;
    let curSource = 'gutenberg';
    let curBooks = [];
    let searching = false;
    let favs = [];
    let readerBook = null;
    let readerText = null;
    let fontSize = 17;

    const $ = (id) => document.getElementById(id);
    const esc = (v) => String(v == null ? '' : v)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;').replace(/'/g, '&#39;');

    function setStatus(id, text, kind) {
        const el = $(id);
        if (!el) return;
        el.textContent = text || '';
        el.className = 'mod-status' + (kind ? ' is-' + kind : '');
    }

    // ---------------- 收藏 / 进度（本地） ----------------
    function loadFavs() {
        try {
            const a = JSON.parse(localStorage.getItem(FAV_KEY) || '[]');
            favs = Array.isArray(a) ? a : [];
        } catch (e) { favs = []; }
        return favs;
    }
    function saveFavs() {
        try { localStorage.setItem(FAV_KEY, JSON.stringify(favs.slice(0, 300))); } catch (e) {}
        const badge = $('fav-nav-badge');
        if (badge) { /* 书库收藏与"喜欢"不是一回事，不占那个角标 */ }
    }
    function isFav(key) { return favs.some(function (f) { return f.key === key; }); }
    function toggleFav(b) {
        const i = favs.findIndex(function (f) { return f.key === b.key; });
        if (i >= 0) { favs.splice(i, 1); showToast('已取消收藏：' + b.title, 'success', 1800); }
        else { favs.unshift(b); showToast('已收藏：' + b.title, 'success', 1800); }
        saveFavs();
        renderGrid(curBooks, 'bk-grid');
        if (!$('page-books') || !$('bk-fav-grid')) return;
        renderGrid(favs, 'bk-fav-grid', true);
        syncFavButton();
    }
    function progAll() {
        try {
            const o = JSON.parse(localStorage.getItem(PROG_KEY) || '{}');
            return (o && typeof o === 'object' && !Array.isArray(o)) ? o : {};
        } catch (e) { return {}; }
    }
    function saveProg(key, chapter) {
        if (!key) return;
        const o = progAll();
        o[key] = { chapter: chapter || 0, at: Date.now() };
        try { localStorage.setItem(PROG_KEY, JSON.stringify(o)); } catch (e) {}
    }

    // ---------------- 渲染 ----------------
    /// 封面：有图用图；没有就用书名首字做色块（Gutenberg 大多没封面）
    function coverHtml(b) {
        if (b.cover) {
            return '<img class="bk-cover-img" loading="lazy" src="' + esc(b.cover) +
                '" onerror="this.replaceWith(Object.assign(document.createElement(\'div\'),{className:\'bk-cover-letter\',textContent:\'' +
                esc((b.title || '书').charAt(0)) + '\'}))">';
        }
        const ch = (b.title || '书').charAt(0);
        const hue = Math.abs(hashStr(b.title || '')) % 360;
        return '<div class="bk-cover-letter" style="--bk-h:' + hue + '">' + esc(ch) + '</div>';
    }
    function hashStr(s) {
        let h = 0;
        for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
        return h;
    }

    function cardHtml(b, inFavTab) {
        const tags = (b.tags || []).slice(0, 3);
        const prog = progAll()[b.key];
        const fav = isFav(b.key);
        return '<div class="bk-card" data-bk-key="' + esc(b.key) + '">'
            +   '<div class="bk-cover">' + coverHtml(b)
            +     (fav ? '<span class="bk-fav-dot" title="已收藏">★</span>' : '')
            +     (prog ? '<span class="bk-prog-dot" title="读到第 ' + (prog.chapter + 1) + " 章\">读</span>" : '')
            +   '</div>'
            +   '<div class="bk-title" title="' + esc(b.title) + '">' + esc(b.title) + '</div>'
            +   '<div class="bk-author">' + esc(b.author || (b.source === 'local' ? '本地文件' : '—')) + '</div>'
            +   (tags.length ? '<div class="bk-tags">' + tags.map(function (t) {
                    return '<span class="bk-tag">' + esc(t) + '</span>'; }).join('') + '</div>' : '')
            +   '<div class="bk-actions">'
            +     '<button class="bk-btn primary" data-bk-read="' + esc(b.key) + '">阅读</button>'
            +     '<button class="bk-btn" data-bk-dl="' + esc(b.key) + '"' + (b.source === 'local' ? ' disabled title="本地书不用下载"' : '') + '>下载</button>'
            +     '<button class="bk-btn" data-bk-fav="' + esc(b.key) + '" title="' + (fav ? '取消收藏' : '收藏') + '">' + (fav ? '★' : '☆') + '</button>'
            +   '</div>'
            + '</div>';
    }

    function renderGrid(list, gridId, inFavTab) {
        const g = $(gridId);
        if (!g) return;
        if (!list || !list.length) {
            g.innerHTML = '<div class="mod-empty">' + (inFavTab ? '还没有收藏的书 —— 在主页点卡片上的 ☆ 收藏' : '这里暂时没有内容') + '</div>';
            return;
        }
        g.innerHTML = list.map(function (b) { return cardHtml(b, inFavTab); }).join('');
    }

    function renderSrcTabs() {
        const box = $('bk-src-tabs');
        if (!box) return;
        box.innerHTML = SOURCES.map(function (s) {
            return '<button class="bk-src-chip' + (s.id === curSource ? ' active' : '') +
                '" data-bk-src="' + s.id + '" title="' + esc(s.hint) + '">' + esc(s.name) + '</button>';
        }).join('');
        const s = SOURCES.find(function (x) { return x.id === curSource; });
        const hint = $('bk-src-hint');
        if (hint) hint.textContent = s ? s.hint : '';
    }

    function setBusy(on, text) {
        const g = $('bk-grid');
        if (on && g) g.innerHTML = '<div class="loading">' + esc(text || '加载中…') + '</div>';
    }

    // ---------------- 数据 ----------------
    async function loadHome() {
        const src = SOURCES.find(function (s) { return s.id === curSource; });
        const title = $('bk-list-title');
        const sub = $('bk-list-sub');
        const sc = $('bk-search-clear');
        if (sc) sc.hidden = true;
        if (title) title.textContent = src ? src.name : '推荐';
        if (sub) sub.textContent = '';
        setStatus('bk-status', '');
        setBusy(true, '正在从 ' + (src ? src.name : '') + ' 拉取…');
        searching = false;
        try {
            let list = [];
            if (curSource === 'shuge') {
                // 书格没有"推荐列表"，直接给个默认搜索
                if (sub) sub.textContent = '书格需要搜索（试试「论语」「史记」「本草纲目」）';
                list = await invoke('book_search', { source: 'shuge', keyword: '论语', page: 1 }) || [];
            } else {
                list = await invoke('book_home', { section: src && src.home ? src.home : 'popular' }) || [];
            }
            curBooks = list;
            renderGrid(curBooks, 'bk-grid');
            if (sub) sub.textContent = list.length ? ('共 ' + list.length + ' 本') : '';
            if (!list.length) setStatus('bk-status', '这个源没返回内容 —— 可能是网络问题，点右上「源自检」看看', 'err');
        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            setBusy(false);
            $('bk-grid').innerHTML = '<div class="mod-empty">加载失败：' + esc(msg) + '</div>';
            setStatus('bk-status', '提示：公版书走 Gutenberg，从国内访问偶尔会超时，点一下重试通常就好了', 'err');
        }
    }

    async function doSearch() {
        const kw = ($('bk-search') || {}).value || '';
        if (!kw.trim()) { showToast('请输入书名或作者'); return; }
        const src = curSource === 'gutenberg_zh' ? 'gutenberg' : curSource;
        const title = $('bk-list-title');
        const sub = $('bk-list-sub');
        const sc = $('bk-search-clear');
        if (title) title.textContent = '搜索结果：' + kw.trim();
        if (sub) sub.textContent = '';
        if (sc) sc.hidden = false;
        setBusy(true, '正在搜索…');
        setStatus('bk-status', '');
        searching = true;
        try {
            const list = await invoke('book_search', { source: src, keyword: kw.trim(), page: 1 }) || [];
            curBooks = list;
            renderGrid(curBooks, 'bk-grid');
            if (sub) sub.textContent = '共 ' + list.length + ' 本';
            if (!list.length) setStatus('bk-status', '没搜到 —— 换个关键词，或换个源再试', 'err');
        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            $('bk-grid').innerHTML = '<div class="mod-empty">搜索失败：' + esc(msg) + '</div>';
        }
    }

    function findBook(key) {
        return curBooks.find(function (b) { return b.key === key; })
            || favs.find(function (b) { return b.key === key; })
            || null;
    }

    // ---------------- 阅读器 ----------------
    function applyFont() {
        const t = $('bk-r-text');
        if (t) t.style.fontSize = fontSize + 'px';
        try { localStorage.setItem(FONT_KEY, String(fontSize)); } catch (e) {}
    }

    function syncFavButton() {
        const btn = $('bk-r-fav');
        if (!btn || !readerBook) return;
        const on = isFav(readerBook.key);
        btn.textContent = on ? '★' : '☆';
        btn.title = on ? '取消收藏' : '收藏';
    }

    async function openReader(book, chapter) {
        if (!book) return;
        readerBook = book;
        const rd = $('bk-reader');
        if (!rd) return;
        rd.hidden = false;
        $('bk-r-title').textContent = book.title || '';
        $('bk-r-sub').textContent = book.author || '';
        $('bk-r-text').textContent = '正在加载正文…';
        $('bk-r-progress').textContent = '';
        $('bk-r-chaps').hidden = true;
        applyFont();
        syncFavButton();
        // 有进度就接着上次那章
        const prog = progAll()[book.key];
        let idx = (chapter == null) ? (prog ? prog.chapter : 0) : chapter;
        try {
            const t = await invoke('book_content', { book: book, chapterIndex: idx }) || {};
            readerText = t;
            $('bk-r-title').textContent = t.title || book.title || '';
            $('bk-r-sub').textContent = t.author || book.author || '';
            const body = (t.text || '').trim();
            $('bk-r-text').textContent = body || '（这个源没给出正文 —— 可以点右上 TXT/EPUB 下载，或换个源）';
            $('bk-r-text').scrollTop = 0;
            const chaps = t.chapters || [];
            const sel = $('bk-r-chaps');
            if (chaps.length > 1) {
                sel.hidden = false;
                sel.innerHTML = chaps.map(function (c) {
                    return '<option value="' + c.index + '"' + (c.index === (t.chapter_index || 0) ? ' selected' : '') + '>' + esc(c.name) + '</option>';
                }).join('');
                $('bk-r-progress').textContent = '第 ' + ((t.chapter_index || 0) + 1) + ' / ' + chaps.length + ' 章';
            } else {
                $('bk-r-progress').textContent = body ? (body.length + ' 字') : '';
            }
            $('bk-r-prev').disabled = !t.has_prev;
            $('bk-r-next').disabled = !t.has_next;
            saveProg(book.key, t.chapter_index || 0);
        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            $('bk-r-text').textContent = '正文加载失败：' + msg;
        }
    }

    function closeReader() {
        const rd = $('bk-reader');
        if (rd) rd.hidden = true;
        readerBook = null;
        readerText = null;
    }

    async function downloadBook(book, format) {
        if (!book) return;
        if (book.source === 'local') { showToast('本地书已经在本地了'); return; }
        // ★ 书格这类古籍：没有文件直链，下载走官方的"下载入口页"（上面挂着网盘链接）。
        //   硬下会 404，所以这里直接把入口页给用户（同时复制到剪贴板）。
        if (book.source === 'shuge') {
            try {
                const list = await invoke('book_extra_downloads', { url: book.read_url }) || [];
                if (list.length) {
                    const u = list[0].url;
                    try { await invoke('copy_to_clipboard', { text: u }); } catch (e) {}
                    showToast('书格古籍的下载入口已复制到剪贴板，正在打开页面…', 'success', 4200);
                    invoke('open_external_browser', { url: u });
                    return;
                }
            } catch (e) {}
            showToast('这条书格记录没有独立下载入口，打开书页自己看一下', 'error', 4200);
            invoke('open_external_browser', { url: book.read_url });
            return;
        }
        showToast('开始下载 ' + (format === 'epub' ? 'EPUB' : 'TXT') + '…', 'success', 2200);
        try {
            const p = await invoke('book_download', { book: book, format: format, destDir: null });
            frontLog('BOOK_DL', book.title + ' -> ' + p);
            showToast('✅ 已下载到：' + p, 'success', 6000);
        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            frontLog('BOOK_DL_FAIL', book.title + ' err=' + msg.slice(0, 160));
            showToast('下载失败：' + msg, 'error', 6000);
        }
    }

    /// 源自检：挨个打一次，把结果列出来（用户报"某源不能用"时一眼看出是网络还是站点问题）
    async function probeSources() {
        setStatus('bk-status', '正在挨个检测书源…', '');
        const ids = ['gutenberg', 'guoxue', 'shuge', 'local'];
        const rows = [];
        for (const id of ids) {
            try {
                const r = await invoke('book_probe', { source: id }) || {};
                rows.push((r.ok ? '✅ ' : '❌ ') + id + '：' + (r.ok ? (r.count + ' 条，例：' + (r.sample || '')) : ('失败 ' + (r.error || '')))
                    + '（' + r.ms + 'ms）');
            } catch (e) {
                rows.push('❌ ' + id + '：' + (e && e.message ? e.message : e));
            }
        }
        setStatus('bk-status', rows.join('　|　'), rows.some(function (r) { return r.startsWith('✅'); }) ? '' : 'err');
        showToast('书源自检完成，结果看下方状态栏', 'success', 3000);
    }

    // ---------------- 绑定 ----------------
    function bind() {
        const on = function (id, ev, fn) { const el = $(id); if (el) el.addEventListener(ev, fn); };
        on('bk-search-btn', 'click', doSearch);
        on('bk-search', 'keydown', function (e) { if (e.key === 'Enter') doSearch(); });
        on('bk-search-clear', 'click', loadHome);
        on('bk-probe', 'click', probeSources);
        on('bk-fav-refresh', 'click', function () { loadFavs(); renderGrid(favs, 'bk-fav-grid', true); });
        on('bk-open-local', 'click', async function () {
            try { await invoke('book_open_local_dir'); showToast('已打开本地书目录，把 txt/epub 丢进去即可'); }
            catch (e) { showToast('打不开目录：' + (e && e.message ? e.message : e)); }
        });
        on('bk-r-back', 'click', closeReader);
        on('bk-r-font-inc', 'click', function () { fontSize = Math.min(30, fontSize + 1); applyFont(); });
        on('bk-r-font-dec', 'click', function () { fontSize = Math.max(13, fontSize - 1); applyFont(); });
        on('bk-r-txt', 'click', function () { downloadBook(readerBook, 'txt'); });
        on('bk-r-epub', 'click', function () { downloadBook(readerBook, 'epub'); });
        on('bk-r-fav', 'click', function () { if (readerBook) toggleFav(readerBook); });
        on('bk-r-prev', 'click', function () {
            const t = readerText; if (!t || !readerBook) return;
            openReader(readerBook, Math.max(0, (t.chapter_index || 0) - 1));
        });
        on('bk-r-next', 'click', function () {
            const t = readerText; if (!t || !readerBook) return;
            openReader(readerBook, (t.chapter_index || 0) + 1);
        });
        on('bk-r-chaps', 'change', function (e) {
            if (!readerBook) return;
            openReader(readerBook, parseInt(e.target.value, 10) || 0);
        });
        // 侧边栏「书库」的二级标签由通用 rail 处理；这里只管页面内点击
        document.addEventListener('click', function (e) {
            if (!e.target || !e.target.closest) return;
            const src = e.target.closest('[data-bk-src]');
            if (src) { curSource = src.dataset.bkSrc; renderSrcTabs(); loadHome(); return; }
            const rd = e.target.closest('[data-bk-read]');
            if (rd) { openReader(findBook(rd.dataset.bkRead), null); return; }
            const dl = e.target.closest('[data-bk-dl]');
            if (dl) { downloadBook(findBook(dl.dataset.bkDl), 'epub'); return; }
            const fv = e.target.closest('[data-bk-fav]');
            if (fv) { const b = findBook(fv.dataset.bkFav); if (b) toggleFav(b); return; }
        });
        // Esc 关阅读器
        document.addEventListener('keydown', function (e) {
            const rd = $('bk-reader');
            if (e.key === 'Escape' && rd && !rd.hidden) { closeReader(); }
        });
        try {
            const f = parseInt(localStorage.getItem(FONT_KEY) || '', 10);
            if (f >= 13 && f <= 30) fontSize = f;
        } catch (e) {}
        bound = true;
    }

    async function subShown(tab) {
        if (!inited) { inited = true; bind(); }
        loadFavs();
        if (tab === 'fav') {
            renderGrid(favs, 'bk-fav-grid', true);
            setStatus('bk-fav-status', favs.length ? ('共 ' + favs.length + ' 本收藏') : '还没有收藏');
            return;
        }
        renderSrcTabs();
        if (!curBooks.length) await loadHome();
        else renderGrid(curBooks, 'bk-grid');
    }

    return {
        init: function () { subShown('home'); },
        // 通用左导轨走的是 tabShown，两个名字都留（rail / subtab 两条路都能进）
        tabShown: subShown,
        subShown: subShown,
    };
})();
