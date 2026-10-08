// ============================================================================
// 独立播放窗口（player.html 的脚本）
// ============================================================================
//
// ★ 用户要求「视频播放单独开一个窗口，不要在软件里面」。
//
// 分工：
//   主窗口  —— 只负责"发起播放"：并发搜源 → 找到能用的线路 → 把整份播放会话
//              （标题/剧集/候选源/相关推荐）交给本窗口（`open_player_window`）。
//   本窗口  —— 自己调 `anime_resolve` / `anime_episodes` 解析并播放；
//              选集、换源、超分、超帧都在这里做，不再打扰主窗口。
//
// 回给主窗口的两个事件：
//   player-progress          { id, progress }  —— 看到第几集（追番进度）
//   focus_main_and_open(...) —— 点了"相关推荐"：**把主窗口调到前台**并开详情
//                               （走 Rust 命令，原因见下面的注释）
//
// 注意：`window.__TAURI__` 由 withGlobalTauri 提供；本窗口的权限在
// src-tauri/capabilities/default.json 的 windows 列表里（必须含 "player"）。

(function () {
    'use strict';

    const $ = (id) => document.getElementById(id);
    const invoke = window.__TAURI__.core.invoke;
    const emit = window.__TAURI__.event.emit;
    const listen = window.__TAURI__.event.listen;

    // ---- 播放会话 ----
    let payload = null;
    let candidates = [];
    let candIdx = 0;
    let episodes = [];
    let epIdx = 0;
    let sourceName = '';
    let followId = 0;
    let curHls = null;
    let playSeq = 0;
    let lastReportedEp = -1;
    let curMediaToken = null;
    // 进度条：用户正在拖 / 刚提交 seek 的短暂保护窗口（见 syncUi 注释）
    let seeking = false;
    let seekGuardUntil = 0;
    // 换片（相关推荐）用：后一次点击直接取代前一次
    let switchSeq = 0;
    let srTimer2 = null;
    // 看门狗提示挂着的时候，第一帧真到了要能撤掉它
    let stallMsgOn = false;
    // 自动换源计数（每次用户主动选片/选集重置，见 applyPayload）
    let autoSwitched = 0;
    let lastAutoAt = 0;
    // 「继续观看」要跳到的秒数（applyPayload 里算，attachMedia 成功后用掉）
    let resumeAt = 0;
    // 媒体真的挂上去了没有 —— 没挂上之前不许写进度，
    // 否则"刚打开就切集/换片"会把上一集的记录冲成 0 秒
    let progressReady = false;
    // 待跳转位置（等 loadedmetadata 才能 seek）
    let pendingSeek = 0;

    // ============================================================
    // 观看进度记忆
    // ------------------------------------------------------------
    // ★ 用户报「动漫看一半，下次打开就没有进度了」。
    //   播放窗口和主窗口**同源**，localStorage 是同一份，所以进度直接写这里，
    //   主窗口（动漫页的「继续观看」）读得到，不用走后端。
    //   存的是「看到第几集 + 这一集看到第几秒」，键用 Bangumi id（没有就用标题）。
    // ============================================================
    const AN_PROG_KEY = 'vortex_anime_progress';

    function progAll() {
        try {
            const o = JSON.parse(localStorage.getItem(AN_PROG_KEY) || '{}');
            return (o && typeof o === 'object' && !Array.isArray(o)) ? o : {};
        } catch (e) { return {}; }
    }

    function progKeyOf(p, fid) {
        if (fid) return 'bgm:' + fid;
        const t = (p && (p.title || p.subtitle)) || '';
        return t ? 't:' + t : '';
    }

    /// 写一条进度。pos 是**当前这一集**的秒数；播完（离结尾 <20s）就记成"看完"。
    function saveWatchProgress() {
        const v = $('pl-video');
        if (!v || !payload || !progressReady) return;
        const key = payload.__subKey || progKeyOf(payload, followId);
        if (!key) return;
        const dur = Number(v.duration) || 0;
        const pos = Number(v.currentTime) || 0;
        // 一集刚开始（<5s）且没有上一集记录时不必写，免得"刚点开就退"把进度冲成 0
        const all = progAll();
        const prev = all[key];
        if (pos < 5 && !prev) return;
        const ep = episodes[epIdx] || {};
        all[key] = {
            key: key,
            title: payload.title || '',
            cover: payload.cover || '',
            followId: followId || 0,
            epIndex: epIdx,
            epName: ep.name || '',
            epCount: episodes.length,
            pos: Math.round(pos),
            dur: Math.round(dur),
            // 播到结尾就当这集看完，下次直接从下一集开头开始
            done: dur > 0 && (dur - pos) < 20,
            updatedAt: Date.now(),
        };
        // 只留最近 200 条，避免 localStorage 无限涨
        const keys = Object.keys(all);
        if (keys.length > 200) {
            keys.sort(function (a, b) { return (all[a].updatedAt || 0) - (all[b].updatedAt || 0); });
            for (let i = 0; i < keys.length - 200; i++) delete all[keys[i]];
        }
        try { localStorage.setItem(AN_PROG_KEY, JSON.stringify(all)); } catch (e) {}
    }

    /// 读一条进度（"继续观看"和恢复播放都要用）
    function loadWatchProgress(key) {
        if (!key) return null;
        const r = progAll()[key];
        return (r && typeof r === 'object') ? r : null;
    }

    /// 把待跳转位置应用到视频上（元数据没到就先等着，canplay/loadedmetadata 会再叫一次）
    function applyPendingSeek() {
        if (!pendingSeek) return;
        const v = $('pl-video');
        if (!v) { pendingSeek = 0; return; }
        const dur = Number(v.duration) || 0;
        if (!(dur > 0)) return;                       // 元数据还没到，下次再试
        const target = pendingSeek;
        pendingSeek = 0;
        if (target < dur - 3) {
            seekGuardUntil = Date.now() + 2000;
            try { v.currentTime = target; } catch (e) {}
            plog('ANIME_RESUME_SEEK', 'seek -> ' + target + 's / ' + Math.round(dur) + 's');
            setStatus('已从 ' + fmtTime(target) + ' 继续播放');
            setTimeout(function () {
                const st = $('pl-status');
                // 只撤掉自己那条提示，别把换源/报错盖掉
                if (st && st.textContent && st.textContent.indexOf('继续播放') >= 0) setStatus('');
            }, 3200);
        }
    }

    /// 播放器侧日志：写 <exe目录>/logs/frontend_events.log。
    /// ★ 「源不可以」这类问题以前只能靠猜，把 hls.js 的错误记下来就能直接看原因。
    function plog(kind, detail) {
        try { invoke('append_frontend_log', { kind: kind, detail: String(detail).slice(0, 300) }); } catch (e) {}
    }

    /// 这个源不行 → 自己换下一个候选线路。
    ///
    /// ★ 多源的意义就在这：以前第一条源黑屏就只会挂一条报错，用户得自己点「数据源」。
    ///   现在致命错/画面起不来会自动往后试，最多 3 次、每次间隔 4 秒（防打转）。
    function tryNextSource(reason) {
        const now = Date.now();
        if (autoSwitched >= 3) return false;
        if (now - lastAutoAt < 4000) return false;
        if (!candidates || candidates.length < 2) return false;
        autoSwitched++;
        lastAutoAt = now;
        plog('PLAYER_AUTO_SWITCH', reason + ' -> 第 ' + autoSwitched + ' 次换源');
        setStatus('这个源' + reason + '，自动换下一个线路…（第 ' + autoSwitched + ' 次）');
        cycleSource();
        return true;
    }

    // ---- 超分 / 超帧 ----
    const SR_CYCLE = ['off', 'efficiency', 'quality'];
    const SR_LABEL = { off: '超分·关', efficiency: '超分·效率', quality: '超分·质量' };
    let srMode = 'off';
    let interpOn = false;
    let fpsTimer = null;

    function setStatus(text, kind) {
        const el = $('pl-status');
        if (!el) return;
        el.textContent = text || '';
        el.className = 'pl-status' + (kind ? ' is-err' : '');
    }

    function fmtTime(t) {
        t = Math.max(0, Math.floor(Number(t) || 0));
        const m = Math.floor(t / 60), sec = t % 60;
        return m + ':' + (sec < 10 ? '0' : '') + sec;
    }

    // ============================================================
    // 超分 / 超帧
    // ============================================================

    function updateSrButtons() {
        const b1 = $('pl-sr-btn');
        if (b1) {
            b1.textContent = SR_LABEL[srMode] || '超分·关';
            b1.classList.toggle('on', srMode !== 'off');
        }
        const b2 = $('pl-interp-btn');
        if (b2) {
            b2.textContent = interpOn ? '超帧·开' : '超帧·关';
            b2.classList.toggle('on', interpOn);
        }
    }

    function startFps() {
        if (fpsTimer) clearInterval(fpsTimer);
        fpsTimer = setInterval(function () {
            const el = $('pl-fps');
            if (!el) return;
            if (!window.__vxAnime4K || !window.__vxAnime4K.isOn()) { el.textContent = ''; return; }
            const f = window.__vxAnime4K.fps();
            el.textContent = f ? (f + ' fps') : '';
            // ★ 状态栏里的「源→输出（倍数）链 N 次/秒」要等渲染过才有值，而挂管线那一刻
            //   视频往往还没解码出第一帧 → 那一行永远是空的（用户看不到超分到底在做什么）。
            //   所以每秒刷一次；只在状态栏显示的是超分信息时刷，别覆盖掉错误/卡顿提示。
            const st = $('pl-status');
            if (st && srMode !== 'off' && st.textContent.indexOf('已开启') === 0) {
                st.textContent = srStatusText();
            }
        }, 1000);
    }

    function stopFps() {
        if (fpsTimer) { clearInterval(fpsTimer); fpsTimer = null; }
        const el = $('pl-fps');
        if (el) el.textContent = '';
    }

    /// 把管线调整到当前设置（超分 + 超帧 任一开着就要有 WebGL 管线）
    /// 拼状态栏文本。
    ///
    /// ★ 尺寸/倍数/链频率这些要等**渲染过一帧**才有值，所以 applyPipeline 里
    ///   先写一遍、1.2 秒后再刷一遍（链频率要 1 秒才统计得出来）。
    function srStatusText() {
        const bits = [];
        try {
            const a = window.__vxAnime4K;
            if (srMode !== 'off') {
                let info = '';
                const nat = (a && a.nativeSize()) || [0, 0];
                const out = (a && a.outSize()) || [0, 0];
                if (nat[0] && out[0]) {
                    const sc = a.lastScale() || 1;
                    const cn = a.chainName();
                    info = '　' + nat[0] + '×' + nat[1] + ' → ' + out[0] + '×' + out[1]
                        + '（' + sc.toFixed(2) + '×，' + ((cn === 'quality' ? '大模型' : '标准')) + '）';
                    const cf = a.chainFps();
                    if (cf) info += '　链 ' + cf + ' 次/秒';
                }
                bits.push('超分 ' + (srMode === 'quality' ? '质量档' : '效率档') + '（链 ' + a.passCount() + ' 趟）' + info);
            }
        } catch (e) {}
        if (interpOn) bits.push('超帧开');
        return bits.length ? ('已开启：' + bits.join(' + ')) : '';
    }

    async function applyPipeline() {
        const v = $('pl-video');
        const c = $('pl-canvas');
        if (!v || !c) return;
        const need = srMode !== 'off' || interpOn;
        updateSrButtons();
        try {
            if (!window.__vxAnime4K) throw new Error('anime4k.js 没加载');
            if (!need) {
                window.__vxAnime4K.detach();
                c.hidden = true;
                stopFps();
                // ★ 关掉超分/超帧后，之前那条「已开启：超分 质量档 …」就不成立了，
                //   必须清掉 —— 否则状态栏一直挂着已经关掉的档位（用户会以为还开着）。
                const stEl = $('pl-status');
                if (stEl && stEl.textContent.indexOf('已开启') === 0) setStatus('');
                return;
            }
            const sup = window.__vxAnime4K.supported();
            if (!sup.ok) throw new Error(sup.why);
            if (window.__vxAnime4K.isOn()) {
                // 已经挂着管线：只改设置，不用重新加载着色器
                window.__vxAnime4K.setInterp(interpOn);
                await window.__vxAnime4K.setSr(srMode);
            } else {
                setStatus('正在加载 Anime4K 着色器…');
                await window.__vxAnime4K.attach(v, c, srMode, interpOn);
            }
            c.hidden = false;
            // ★ 暂停状态下切档位：驱动循环用的是 requestVideoFrameCallback，
            //   暂停时它不触发 → 画布一直是空白（黑屏）。手动渲染一次当前帧补上。
            if (v.paused) {
                try { window.__vxAnime4K.renderOnce(); } catch (_e) {}
            }
            startFps();
            // 尺寸/倍数/链频率要渲染过才有值 → 稍后再刷一次状态栏
            if (srTimer2) clearTimeout(srTimer2);
            srTimer2 = setTimeout(function () {
                try { setStatus(srStatusText()); } catch (e) {}
                // 记一条诊断：源/画布/倍数/这一帧真跑了几趟着色器（趟数 < 总趟数 说明
                // Anime4K 的 WHEN 守卫跳过了放大链 —— 以前画布不到源的 1.2 倍就会这样）
                try {
                    const a = window.__vxAnime4K;
                    const nat = a.nativeSize() || [0, 0];
                    const out = a.outSize() || [0, 0];
                    plog('SR', srMode + ' 源 ' + nat[0] + 'x' + nat[1]
                        + ' 画布 ' + out[0] + 'x' + out[1]
                        + ' 倍数 ' + (a.lastScale() || 0).toFixed(2)
                        + ' 趟 ' + a.passRuns() + '/' + a.passCount()
                        + ' 链 ' + a.chainFps() + '/s 帧 ' + a.fps() + 'fps');
                } catch (e) {}
            }, 1300);
            setStatus(srStatusText());

        } catch (e) {
            const msg = String(e && e.message ? e.message : e);
            try { if (window.__vxAnime4K) window.__vxAnime4K.detach(); } catch (_) {}
            c.hidden = true;
            stopFps();
            srMode = 'off';
            interpOn = false;
            updateSrButtons();
            setStatus('开不起来：' + msg, 'err');
        }
    }

    function savePrefs() {
        try {
            localStorage.setItem('vx_an_sr_mode', srMode);
            localStorage.setItem('vx_an_interp', interpOn ? '1' : '0');
        } catch (e) {}
    }

    function loadPrefs() {
        try {
            const s = localStorage.getItem('vx_an_sr_mode');
            if (SR_CYCLE.indexOf(s) >= 0) srMode = s;
            interpOn = localStorage.getItem('vx_an_interp') === '1';
        } catch (e) {}
        updateSrButtons();
    }

    // ============================================================
    // 播放
    // ============================================================

    function detachHls() {
        if (curHls) { try { curHls.destroy(); } catch (e) {} curHls = null; }
    }

    /// 画面真的来了就把看门狗的提示撤掉，别让「在播 + 底下挂着报错」同时出现。
    /// （监听只挂一次，在 bind() 里；换集时不要重复挂，否则越积越多）
    function clearStall() {
        if (!stallMsgOn) return;
        const v = $('pl-video');
        if (v && (v.readyState >= 2 || (v.buffered && v.buffered.length))) {
            stallMsgOn = false;
            setStatus('');
        }
    }

    /// 播放中途卡死的看门狗。
    ///
    /// ★ 源站分片断流时 hls.js 只会反复 nudge / 报 bufferStalledError（**非致命**，
    ///   不会进 tryNextSource），画面就永久黑在那儿 —— 用户只能自己去点「数据源」。
    ///   这里只要「没暂停、没拖动、进度 12 秒没往前走」就自动换下一个源。
    ///   每次挂新媒体源都重启这个计时，所以换源后不会被上一次的计数误伤。
    let stallWatchId = null;
    let lastProgressAt = 0;
    let lastProgressT = -1;
    function startStallWatch() {
        if (stallWatchId) clearInterval(stallWatchId);
        lastProgressAt = Date.now();
        lastProgressT = -1;
        stallWatchId = setInterval(function () {
            const v = $('pl-video');
            if (!v) return;
            // 暂停/拖动中不算卡（用户自己停的）
            if (v.paused || v.seeking || v.readyState === 0) {
                lastProgressAt = Date.now();
                return;
            }
            if (v.currentTime !== lastProgressT) {
                lastProgressT = v.currentTime;
                lastProgressAt = Date.now();
                return;
            }
            if (Date.now() - lastProgressAt >= 12000) {
                lastProgressAt = Date.now();
                if (!tryNextSource('播放卡住 12 秒没动')) {
                    setStatus('这个源卡住了：点「数据源」可以换一个', 'err');
                }
            }
        }, 2000);
    }

    function attachMedia(url) {
        stallMsgOn = false;   // 换了媒体源，上一次的看门狗提示作废
        startStallWatch();
        const v = $('pl-video');
        const isHls = /\.m3u8(\?|$)/i.test(url) || /m3u8/i.test(url);
        const hasHls = !!(window.Hls && window.Hls.isSupported());
        detachHls();
        if (isHls && hasHls) {
            // ★ enableWorker 必须是 false：hls.js 1.5.20 在 worker 模式下分片载荷会变成
            //   0 字节，而且一个 ERROR 都不报 —— 表现就是"能播放但画面一直黑"。
            const hls = new window.Hls({ maxBufferLength: 30, enableWorker: false, lowLatencyMode: false });
            curHls = hls;
            let netFails = 0;
            hls.on(window.Hls.Events.ERROR, function (_, d) {
                if (!d) return;
                plog('PLAYER_HLS_ERR', (d.fatal ? 'fatal ' : '') + (d.type || '') + ' / ' + (d.details || '')
                    + (d.response && d.response.code ? ' HTTP' + d.response.code : '')
                    + ' url=' + String((d.frag && d.frag.url) || d.url || '').slice(0, 120));
                if (!d.fatal) return;
                if (d.type === window.Hls.ErrorTypes.NETWORK_ERROR && netFails < 3) {
                    netFails++;
                    setStatus('网络不稳，重试第 ' + netFails + ' 次…');
                    try { hls.startLoad(); } catch (e) {}
                } else if (d.type === window.Hls.ErrorTypes.MEDIA_ERROR) {
                    try { hls.recoverMediaError(); } catch (e) {}
                } else if (!tryNextSource('取不到分片（' + (d.details || d.type || '未知')
                        + (d.response && d.response.code ? ' HTTP' + d.response.code : '') + '）')) {
                    setStatus('播放出错：' + (d.details || d.type || '未知')
                        + (d.response && d.response.code ? '（HTTP ' + d.response.code + '）' : ''), 'err');
                }
            });
            hls.loadSource(url);
            hls.attachMedia(v);
            hls.on(window.Hls.Events.MANIFEST_PARSED, function () {
                const p = v.play();
                if (p && p.catch) p.catch(function (err) {
                    // 自动播放被拦 → 静音再试（静音播放一定允许）
                    v.muted = true;
                    const p2 = v.play();
                    if (p2 && p2.catch) p2.catch(function () {
                        setStatus('浏览器拦了自动播放（' + ((err && err.name) || '') + '），点一下播放键', 'err');
                        return;
                    });
                    setStatus('已静音自动播放，点右下角喇叭开声音');
                });
            });
            const token = {};
            curMediaToken = token;
            // ★ 12 秒太急：实测有的源要 25 秒才出第一帧，看门狗先报了错，
            //   画面后来正常播放，那条报错却一直挂在下面 —— 所以既延长时间，
            //   也在第一帧到达时把它撤掉（见下面的 clearStall）。
            setTimeout(function () {
                if (curMediaToken !== token) return;
                if (v.readyState === 0 && !(v.buffered && v.buffered.length)) {
                    // 画面一直不来 → 先自己换源，换不动了才提示用户
                    if (!tryNextSource('画面一直起不来')) {
                        stallMsgOn = true;
                        setStatus('画面起得比较慢：这个源的分片可能取不到，点「数据源」可以换一个', 'err');
                    }
                }
            }, 18000);
        } else if (isHls) {
            setStatus('hls.js 没加载出来，无法播放 m3u8', 'err');
        } else {
            v.src = url;
            v.load();
            const p = v.play();
            if (p && p.catch) p.catch(function () {});
        }
    }

    // 源站的剧集名经常就是「第01集」「1」这类序号本身，直接拼会变成
    // 「第 1 集 · 第1集」。抽出数字比对，是序号就只用我们自己的标签。
    function epLabel(i, name) {
        const n = i + 1;
        const plain = String(name == null ? '' : name).trim();
        const digits = plain.replace(/[^0-9]/g, '');
        if (!plain || plain === String(n) || (digits && Number(digits) === n)) return '第 ' + n + ' 集';
        return '第 ' + n + ' 集 · ' + plain;
    }

    async function playEpisode(idx) {
        if (idx < 0 || idx >= episodes.length) return;
        // ★ 换集之前先把**上一集**的进度落盘（此刻 epIdx/currentTime 还是上一集的）
        if (progressReady) saveWatchProgress();
        epIdx = idx;
        const ep = episodes[idx];
        const seq = ++playSeq;
        const p = payload || {};

        $('pl-win-title').textContent = (p.title || '')
            + (episodes.length > 1 ? ' · 第 ' + (idx + 1) + ' 集' : '');
        $('pl-ep-no').textContent = epLabel(idx, ep.name);
        $('pl-ep-desc').textContent = (p.epSummaries && p.epSummaries[idx]) || '（这一集没有简介）';
        $('pl-src-name').textContent = sourceName || '—';
        renderEpSelect();
        renderRelated();

        setStatus('正在解析播放地址…');
        try {
            const info = await invoke('anime_resolve', {
                sourceUrl: (candidates[candIdx] && candidates[candIdx].source_url) || p.sourceUrl || '',
                pageUrl: ep.url,
            });
            if (seq !== playSeq) return;
            if (!info || !info.url) throw new Error('没解析出播放地址');
            setStatus('');
            attachMedia(info.url);
            progressReady = true;
            // ★ 恢复上次看到的位置（attachMedia 之后，等元数据到了再 seek）
            if (resumeAt > 0) { pendingSeek = resumeAt; resumeAt = 0; applyPendingSeek(); }
            // 开播后再挂管线（要有画面才有意义）
            if (srMode !== 'off' || interpOn) applyPipeline();
            reportProgress(idx + 1);
        } catch (e) {
            if (seq !== playSeq) return;
            setStatus('播放失败：' + (e && e.message ? e.message : e)
                + '　（可点右侧「数据源」换一个线路）', 'err');
        }
    }

    function reportProgress(epNo) {
        if (!followId || epNo === lastReportedEp) return;
        lastReportedEp = epNo;
        emit('player-progress', { id: followId, progress: epNo }).catch(function () {});
    }

    function renderEpSelect() {
        const sel = $('pl-ep-select');
        if (!sel) return;
        sel.innerHTML = episodes.map(function (e, i) {
            return '<option value="' + i + '">' + escapeHtml(epLabel(i, e.name)) + '</option>';
        }).join('');
        sel.value = String(epIdx);
    }

    function escapeHtml(s) {
        return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
            return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
        });
    }

    function renderRelated() {
        const box = $('pl-rel-list');
        if (!box) return;
        const rel = (payload && payload.related) || [];
        const cnt = $('pl-rel-count');
        if (cnt) cnt.textContent = rel.length ? rel.length + ' 部' : '';
        if (!rel.length) { box.innerHTML = '<div class="pl-empty">没有推荐</div>'; return; }
        box.innerHTML = rel.map(function (r) {
            const img = r.image
                ? '<img loading="lazy" referrerpolicy="no-referrer" src="' + escapeHtml(r.image)
                  + '" onerror="this.style.visibility=\'hidden\'">'
                : '';
            return '<div class="pl-rel-item" data-sub="' + r.id + '" data-name="' + escapeHtml(r.name_cn || r.name || '') + '">'
                + '<div class="pl-rel-thumb">' + img + '</div>'
                + '<div class="pl-rel-info"><div class="pl-rel-name">'
                + escapeHtml(r.name_cn || r.name || '')
                + '</div>'
                + (r.desc ? '<div class="pl-rel-desc">' + escapeHtml(r.desc) + '</div>' : '')
                + '</div></div>';
        }).join('');
    }

    // 标题相似度：和主窗口同一套算法，用来在搜索结果里挑最像的那个条目
    const STRIP = String.fromCharCode(9, 10, 13, 32, 0x3000, 39, 34) + '·:：—_~～!！?？.。,，';
    function titleScore(a, b) {
        const norm = function (v) {
            const t = String(v || '').toLowerCase();
            let o = '';
            for (let i = 0; i < t.length; i++) if (STRIP.indexOf(t[i]) < 0) o += t[i];
            return o;
        };
        const x = norm(a), y = norm(b);
        if (!x || !y) return 0;
        if (x === y) return 1000;
        if (x.indexOf(y) >= 0 || y.indexOf(x) >= 0) return 700 - Math.abs(x.length - y.length);
        let best = 0;
        for (let i = 0; i < x.length; i++) {
            for (let j = 0; j < y.length; j++) {
                let run = 0;
                while (i + run < x.length && j + run < y.length && x[i + run] === y[j + run]) run++;
                if (run > best) best = run;
            }
        }
        return Math.round(best * 60 / Math.max(x.length, y.length));
    }

    /// 点相关推荐：**在本窗口直接换片播放**。
    ///
    /// ★ 用户明确要求「不要弹主窗口，直接播放」。流程和主窗口的 startWatch 一样：
    ///   拉详情 → 并发搜所有源 → 按标题相似度排序 → 逐个试到能拿到剧集 → 换 payload 重播。
    async function playSubject(id, knownName, knownCover) {
        if (!id) return;
        const my = ++switchSeq;          // 后一次点击直接取代前一次（别用布尔锁，搜源要十几秒）
        playSeq++;                       // 让还在飞的 playEpisode 结果作废
        const t0 = Date.now();
        const old = payload || {};
        const oldId = Number(old.followId) || 0;
        try {
            // ★ 详情和搜源**并行**：标题相关推荐里本来就带着，没必要先等 Bangumi
            //   再拿标题去搜源（那一次串行就白花 2~5 秒）。
            const detailP = invoke('anime_detail', { id: id }).catch(function () { return null; });
            let nm = knownName || '';
            if (!nm) {
                const d0 = await detailP;
                if (my !== switchSeq) return;
                const m0 = (d0 && d0.meta) || {};
                nm = m0.name_cn || m0.name || '';
            }
            if (!nm) throw new Error('拿不到这部番的标题');
            setStatus('正在并发搜索「' + nm + '」的可用线路…');
            // 换片只要一条能用的线路 → quick 模式（2 条 / 3 秒上限，不跑网页源）
            const rs = await invoke('anime_search_all', { keyword: nm, quick: true }) || [];
            const tSearch = Date.now();
            if (my !== switchSeq) return;
            if (!rs.length) throw new Error('所有源都没搜到这部番（当前网络下这些站可能不可达）');
            const cands = rs.map(function (r) {
                return { r: r, s: titleScore(nm, r.name_cn || r.name || r.title || '') };
            }).sort(function (a, b) { return b.s - a.s; }).slice(0, 6).map(function (x) { return x.r; });

            let ci = -1, eps = null, srcLabel = '';
            for (let i = 0; i < cands.length; i++) {
                if (my !== switchSeq) return;
                const c = cands[i];
                srcLabel = c.source_name || c.source || ('源' + (i + 1));
                setStatus('正在试 ' + srcLabel + ' …（' + (i + 1) + '/' + cands.length + '）');
                try {
                    const e2 = await invoke('anime_episodes', { sourceUrl: c.source_url, pageUrl: c.url }) || [];
                    if (e2.length) { ci = i; eps = e2; break; }
                } catch (e) { /* 试下一个 */ }
            }
            const tEps = Date.now();
            if (ci < 0) throw new Error(cands.length + ' 个候选源都拿不到剧集');
            // ★ 不再 await 详情：Bangumi 那次要 2.6 秒，而换片只要标题 + 封面
            //   （相关推荐里本来就带着），先换过去，简介等详情回来了再补。
            const meta = {};
            const bgmEps = [];

            // 相关推荐：把刚才在看的那部放回列表（方便切回）+ 补一批热门
            const rel = [];
            const push = function (o) {
                if (!o || !o.id || o.id === id) return;
                for (let k = 0; k < rel.length; k++) if (rel[k].id === o.id) return;
                rel.push(o);
            };
            if (oldId) push({ id: oldId, name_cn: old.title || '', image: old.cover || '', desc: '正在播放' });
            (old.related || []).forEach(push);
            // ★ 不再重新拉 anime_hot：现有列表已经够铺满面板，省一次往返就是省 1 秒
            if (my !== switchSeq) return;

            applyPayload({
                title: nm,
                cover: knownCover || '',
                subtitle: '',
                followId: id,
                epIndex: 0,
                sourceName: srcLabel,
                sourceUrl: cands[ci].source_url || '',
                candidates: cands.map(function (c) {
                    return { name: c.name, title: c.title, source_name: c.source_name, source: c.source,
                             source_url: c.source_url, url: c.url };
                }),
                candidateIdx: ci,
                episodes: eps.map(function (e) {
                    return { channel: e.channel, name: e.name, sort: e.sort, url: e.url };
                }),
                epSummaries: eps.map(function (e, i) {
                    const b = bgmEps[i] || {};
                    return b.summary || b.name || '';
                }),
                related: rel.slice(0, 12),
            });
            plog('PLAYER_SWITCH', nm + ' 用时 ' + ((Date.now() - t0) / 1000).toFixed(1)
                + ' 秒，源=' + srcLabel + '，候选 ' + cands.length + ' 个'
                + '　[搜源 ' + (tSearch - t0) + 'ms / 取剧集 ' + (tEps - tSearch)
                + 'ms / 换 payload ' + (Date.now() - tEps) + 'ms]');
            // 主窗口跟着换成这一部（★ 不抢焦点，别打扰正在看的画面）
            invoke('main_sync_subject', { subjectId: id }).catch(function () {});
            // 详情回来后**在后台补**封面/副标题/本集简介，不打断已经开始的播放
            detailP.then(function (d) {
                if (my !== switchSeq || !d) return;
                try {
                    const m = (d && d.meta) || {};
                    const be = (d && d.episodes) || [];
                    if (m.name && m.name !== nm) {
                        const subEl = $('pl-hero-sub');
                        if (subEl) { subEl.textContent = m.name; subEl.style.display = ''; }
                    }
                    if (!payload) return;
                    payload.epSummaries = (payload.episodes || []).map(function (e, i) {
                        const b = be[i] || {};
                        return b.summary || b.name || '';
                    });
                    const desc = $('pl-ep-desc');
                    if (desc) desc.textContent = payload.epSummaries[epIdx] || '（这一集没有简介）';
                } catch (e) {}
            });
        } catch (e) {
            if (my === switchSeq) {
                setStatus('换片失败：' + (e && e.message ? e.message : e)
                    + '　（可点右侧「数据源」换线路）', 'err');
            }
        }
    }

    async function cycleSource() {
        if (candidates.length <= 1) {
            setStatus('只有一条候选线路可换', 'err');
            return;
        }
        const start = candIdx;
        for (let k = 1; k <= candidates.length; k++) {
            const next = (start + k) % candidates.length;
            const c = candidates[next];
            setStatus('正在换到 ' + (c.source_name || c.source || c.name || ('源' + (next + 1))) + ' …');
            try {
                const eps = await invoke('anime_episodes', {
                    sourceUrl: c.source_url,
                    pageUrl: c.url,
                }) || [];
                if (!eps.length) continue;
                candIdx = next;
                sourceName = c.source_name || c.source || c.name || ('源' + (next + 1));
                episodes = eps;
                await playEpisode(0);
                return;
            } catch (e) { /* 试下一个 */ }
        }
        setStatus('所有候选线路都不可用', 'err');
    }

    // ============================================================
    // 会话载入
    // ============================================================

    function applyPayload(p) {
        autoSwitched = 0;   // 新片子/新一集，重新给自动换源 3 次机会
        // ★ 换片/换集之前先把**当前**的进度落盘（此刻 payload/epIdx/currentTime 还是旧的）
        if (progressReady) saveWatchProgress();
        progressReady = false;
        pendingSeek = 0;
        payload = p || {};
        candidates = p.candidates || [];
        candIdx = Number(p.candidateIdx) || 0;
        episodes = p.episodes || [];
        sourceName = p.sourceName
            || (candidates[candIdx] && (candidates[candIdx].source_name || candidates[candIdx].name)) || '—';
        followId = Number(p.followId) || 0;
        lastReportedEp = -1;
        // ★ 进度记忆：算出这条片子的键，并记下"要恢复到第几秒"
        payload.__subKey = p.subKey || progKeyOf(p, followId);
        const saved = loadWatchProgress(payload.__subKey);
        // 只在"就是同一集"时续播；用户主动点别的集数时不硬拉回旧位置
        const wantEp = Number(p.epIndex) || 0;
        resumeAt = (saved && !saved.done && saved.epIndex === wantEp && saved.pos > 5) ? saved.pos : 0;
        if (resumeAt > 0) plog('ANIME_RESUME', 'key=' + payload.__subKey + ' ep=' + wantEp + ' pos=' + resumeAt);

        const hero = $('pl-hero-img');
        const heroBox = $('pl-hero');
        const heroBg = $('pl-hero-bg');
        if (hero) {
            if (p.cover) {
                hero.src = p.cover;
                if (heroBg) heroBg.style.backgroundImage = 'url("' + String(p.cover).replace(/"/g, '%22') + '")';
                if (heroBox) heroBox.classList.remove('no-cover');
            } else {
                hero.removeAttribute('src');
                if (heroBg) heroBg.style.backgroundImage = '';
                if (heroBox) heroBox.classList.add('no-cover');
            }
        }
        $('pl-hero-title').textContent = p.title || '—';
        const subEl = $('pl-hero-sub');
        subEl.textContent = p.subtitle || '';
        subEl.style.display = p.subtitle ? '' : 'none';
        $('pl-src-name').textContent = sourceName;
        renderEpSelect();
        renderRelated();

        if (!episodes.length) {
            setStatus('这条线路没拿到剧集，点「数据源」换一个', 'err');
            return;
        }
        playEpisode(Number(p.epIndex) || 0);
    }

    // ============================================================
    // 界面
    // ============================================================

    function syncUi() {
        const v = $('pl-video');
        const icon = $('pl-toggle-icon');
        if (icon) {
            icon.innerHTML = v.paused
                ? '<polygon points="6 4 20 12 6 20"/>'
                : '<rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/>';
        }
        const seek = $('pl-seek');
        // ★ 拖动中、以及刚提交 seek 的一小段时间里，**不要**拿 currentTime 回写滑块：
        //   seek 是异步的，立刻回写会把滑块弹回旧位置（用户报的「拖了会返回去」）。
        if (seek && !seeking && Date.now() > seekGuardUntil) {
            const d = Number(v.duration) || 0;
            seek.value = d > 0 ? Math.round((v.currentTime / d) * 1000) : 0;
        }
        const t = $('pl-time');
        if (t && !seeking) t.textContent = fmtTime(v.currentTime) + ' / ' + fmtTime(v.duration);
        const mi = $('pl-mute-icon');
        if (mi) {
            mi.innerHTML = (v.muted || v.volume === 0)
                ? '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19"/><line x1="22" y1="9" x2="16" y2="15"/><line x1="16" y1="9" x2="22" y2="15"/>'
                : '<polygon points="11 5 6 9 2 9 2 15 6 15 11 19"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M18.5 5.5a9 9 0 0 1 0 13"/>';
        }
    }

    /// 浮层显隐：鼠标一动就显形，静止 2.6 秒淡出
    function setupChrome() {
        const root = $('pl-root');
        let timer = null;
        const wake = () => {
            root.classList.add('awake');
            if (timer) clearTimeout(timer);
            timer = setTimeout(() => root.classList.remove('awake'), 2600);
        };
        document.addEventListener('mousemove', wake);
        document.addEventListener('mouseenter', wake);
        document.addEventListener('mouseleave', () => {
            if (timer) clearTimeout(timer);
            root.classList.remove('awake');
        });
        // 暂停时别把控件藏起来（用户多半正要操作）
        $('pl-video').addEventListener('pause', () => {
            if (timer) clearTimeout(timer);
            root.classList.add('awake');
        });
        wake();
    }

    function setupWindowControls() {
        let win = null;
        try {
            const W = window.__TAURI__ && window.__TAURI__.window;
            if (W && W.getCurrentWindow) win = W.getCurrentWindow();
        } catch (e) {}
        const on = (id, fn) => { const el = $(id); if (el) el.addEventListener('click', fn); };
        // 回主界面：把主窗口抬到前台，并让它显示正在看的这一部（subjectId=0 时只抬窗口）
        on('pl-main-btn', () => {
            invoke('focus_main_and_open', { subjectId: followId || 0 }).catch(function () {});
        });
        on('pl-min', () => { try { win && win.minimize(); } catch (e) {} });
        on('pl-max', () => { try { win && win.toggleMaximize(); } catch (e) {} });
        on('pl-close', () => { try { win && win.close(); } catch (e) {} });
        on('pl-panel-btn', () => {
            const root = $('pl-root');
            root.classList.toggle('no-panel');
            try {
                localStorage.setItem('vx_pl_panel', root.classList.contains('no-panel') ? '0' : '1');
            } catch (e) {}
        });
        try {
            if (localStorage.getItem('vx_pl_panel') === '0') $('pl-root').classList.add('no-panel');
        } catch (e) {}
    }

    function bind() {
        const v = $('pl-video');
        v.controls = false;

        $('pl-toggle').addEventListener('click', function () {
            if (v.paused) { const p = v.play(); if (p && p.catch) p.catch(function () {}); }
            else { try { v.pause(); } catch (e) {} }
        });
        $('pl-mute').addEventListener('click', function () { v.muted = !v.muted; });
        // ★ 进度条：拖动过程中**只更新文字预览，不碰 currentTime**。
        //   以前每个 input 事件都写一次 currentTime，HLS 会为此重载分片 → 卡顿，
        //   而且 seek 还没落地就被 timeupdate 回写 → 滑块弹回去。
        //
        // ★★ 2026-10-08 用户报「只可以点一下、拖过去、再点一下才停，不能按住拖」：
        //   原生 range 的拖动在 WebView2 里不可靠（按下不一定进入拖动状态，
        //   松开也不一定收到 pointerup，于是要再点一下才"停下"）。
        //   改成**自己实现**：按下即跳转 + `setPointerCapture` 抓住指针 →
        //   移动全程跟随、**松手立刻提交**，指针移出控件也不丢事件。
        const seek = $('pl-seek');
        let seekDragging = false;

        const previewSeek = function () {
            const d = Number(v.duration) || 0;
            const t = $('pl-time');
            if (t && d > 0) {
                t.textContent = fmtTime((Number(seek.value) / 1000) * d) + ' / ' + fmtTime(d);
            }
        };
        const commitSeek = function () {
            if (!seeking) return;
            seeking = false;
            const d = Number(v.duration) || 0;
            if (d > 0) {
                // 保护窗口：等真的跳过去（seeked）再恢复回写，避免中间帧把滑块拉回旧位置
                seekGuardUntil = Date.now() + 1500;
                try { v.currentTime = (Number(seek.value) / 1000) * d; } catch (e) {}
            }
            syncUi();
        };
        // 把指针的屏幕 x 换算成进度条位置（0..1000）
        const seekFromX = function (clientX) {
            const r = seek.getBoundingClientRect();
            if (r.width <= 0) return;
            let ratio = (clientX - r.left) / r.width;
            if (ratio < 0) ratio = 0;
            if (ratio > 1) ratio = 1;
            seek.value = String(Math.round(ratio * 1000));
            previewSeek();
        };
        seek.addEventListener('pointerdown', function (e) {
            if (e.button !== undefined && e.button !== 0) return;   // 只认左键
            seekDragging = true;
            seeking = true;
            try { seek.setPointerCapture(e.pointerId); } catch (err) {}
            seekFromX(e.clientX);
            e.preventDefault();   // 别让浏览器再跑一遍原生拖动逻辑
        });
        seek.addEventListener('pointermove', function (e) {
            if (!seekDragging) return;
            seekFromX(e.clientX);
            e.preventDefault();
        });
        const endSeekDrag = function (e) {
            if (!seekDragging) return;
            seekDragging = false;
            try { if (e && e.pointerId !== undefined) seek.releasePointerCapture(e.pointerId); } catch (err) {}
            commitSeek();
        };
        seek.addEventListener('pointerup', endSeekDrag);
        seek.addEventListener('pointercancel', endSeekDrag);
        // 键盘操作（方向键）走原生 input/change
        seek.addEventListener('input', function () {
            if (seekDragging) return;   // 拖动中的 input 由我们自己处理
            seeking = true;
            previewSeek();
        });
        seek.addEventListener('change', function () { if (!seekDragging) commitSeek(); });

        v.addEventListener('timeupdate', syncUi);
        v.addEventListener('seeked', function () { seekGuardUntil = 0; syncUi(); });
        v.addEventListener('loadeddata', clearStall);
        v.addEventListener('playing', clearStall);
        v.addEventListener('canplay', clearStall);
        v.addEventListener('timeupdate', clearStall);
        v.addEventListener('durationchange', syncUi);
        v.addEventListener('play', syncUi);
        v.addEventListener('pause', syncUi);
        v.addEventListener('volumechange', syncUi);
        // ★ 观看进度记忆：换源/暂停/关窗/每 5 秒都落一次盘
        v.addEventListener('loadedmetadata', applyPendingSeek);
        v.addEventListener('canplay', applyPendingSeek);
        v.addEventListener('pause', saveWatchProgress);
        v.addEventListener('ended', saveWatchProgress);
        window.addEventListener('pagehide', saveWatchProgress);
        window.addEventListener('beforeunload', saveWatchProgress);
        setInterval(saveWatchProgress, 5000);

        $('pl-ep-select').addEventListener('change', function () {
            playEpisode(Number(this.value) || 0);
        });
        $('pl-src-btn').addEventListener('click', function () {
            autoSwitched = 0;   // 用户手动换源，重新给自动换源机会
            cycleSource();
        });

        // 超分：点一下循环 关 → 效率 → 质量
        $('pl-sr-btn').addEventListener('click', function () {
            const i = SR_CYCLE.indexOf(srMode);
            srMode = SR_CYCLE[(i + 1) % SR_CYCLE.length];
            savePrefs();
            applyPipeline();
        });
        // 超帧：点一下开关
        $('pl-interp-btn').addEventListener('click', function () {
            interpOn = !interpOn;
            savePrefs();
            applyPipeline();
        });

        // 相关推荐 → **本窗口直接换片播放**（用户要求：不要弹主窗口）
        $('pl-rel-list').addEventListener('click', function (e) {
            const it = e.target.closest('[data-sub]');
            if (!it) return;
            const id = Number(it.dataset.sub);
            if (!id) return;
            const img = it.querySelector('img');
            playSubject(id, it.dataset.name || '', (img && img.getAttribute('src')) || '');
        });

        syncUi();
    }

    // ============================================================
    // 启动
    // ============================================================

    async function boot() {
        // 主题跟主窗口一致（两个窗口同源）
        try {
            const t = localStorage.getItem('vortex_theme');
            if (t && t !== 'purple') document.documentElement.setAttribute('data-theme', t);
        } catch (e) {}

        loadPrefs();
        bind();
        setupChrome();
        setupWindowControls();
        // 窗口缩放会改变放大倍数 → 跨过档位线就按新倍数重建链（防抖 600ms）
        (function () {
            let t = null;
            window.addEventListener('resize', function () {
                if (t) clearTimeout(t);
                t = setTimeout(function () {
                    try {
                        const a = window.__vxAnime4K;
                        if (a && a.isOn() && a.needsRebuild()) {
                            applyPipeline();
                        }
                    } catch (e) {}
                }, 600);
            });
        })();
        setStatus('正在准备播放…');

        let p = null;
        try { p = await invoke('player_take_payload'); } catch (e) {}
        if (!p) {
            setStatus('没有待播放的内容（请从主窗口点「开始观看」）', 'err');
            return;
        }
        applyPayload(p);

        // 主窗口再次发起播放时后端会发 player-reload，这里重新取一份
        try {
            await listen('player-reload', async function () {
                try {
                    const np = await invoke('player_take_payload');
                    if (np) applyPayload(np);
                } catch (e) {}
            });
        } catch (e) {}
    }

    window.addEventListener('DOMContentLoaded', function () {
        boot().catch(function (e) {
            setStatus('播放器初始化失败: ' + (e && e.message ? e.message : e), 'err');
        });
    });
})();
