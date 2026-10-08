// ============================================================================
// Anime4K 实时超分（WebGL2）
// ============================================================================
//
// 着色器来自 bloc97/Anime4K v4（**MIT**，见 shaders/LICENSE），是 Kazumi
// （Predidit/Kazumi，MIT）在 mpv 上用的同一套。区别是 mpv 原生支持多趟 GLSL 管线，
// 而我们的播放器是 WebView2 里的 <video>，所以这里**自己实现了一个小型的
// mpv 着色器管线**：解析 `//!` 指令 → 每趟编译成一个 WebGL2 片元着色器 → 依次
// 渲染进 FBO → 最后画到 canvas。
//
// ## mpv 指令 → WebGL2 的映射
//
// | mpv                      | 这里                                   |
// |--------------------------|----------------------------------------|
// | `//!HOOK MAIN`           | 钩住当前 MAIN（每趟的输出尺寸由 WIDTH/HEIGHT 定） |
// | `//!BIND X`              | 把 X 绑成采样器，并生成 X_tex / X_texOff / X_pos / X_size / X_pt |
// | `//!SAVE X`              | 这一趟的输出写进 X（不写就写回 HOOK 的那个） |
// | `//!WIDTH / //!HEIGHT`   | RPN 表达式，支持 `MAIN.w 2 *` 这种       |
// | `//!COMPONENTS n`        | 忽略（统一用 RGBA16F，取 .x 也对）       |
// | `//!WHEN <rpn>`          | RPN 求值，false 就跳过这一趟             |
//
// ★ 坐标模型：mpv 的 `<X>_pos` 是**归一化坐标**（与分辨率无关），
//   `<X>_pt` 是 1/尺寸，`<X>_texOff(o)` 是"当前点 + o 个该纹理的像素"。
//   所以统一写成：
//     X_pos  = v_uv
//     X_pt   = 1.0 / X_size
//     X_texOff(o) = texture(X_s, v_uv + o * X_pt)
//   实测这套映射对 2 倍放大的 depth-to-space 那趟也对得上（那趟靠
//   `fract(pos*size)` 取相位，归一化坐标 × 尺寸正好还原像素坐标）。
//
// ★ 中间结果有负数（CNN 的权重），所以必须用**浮点**渲染目标（RGBA16F）。
//   拿不到 `EXT_color_buffer_float` 就直接禁用超分并说明原因 —— 退回 8 位会
//   clamp 掉负数，画质反而更差。
//
// ★ 去掉了 Kazumi 链里的 `AutoDownscalePre_x2/x4`：那两趟只在"视频比显示区域大
//   很多"时先缩小以省 GPU，网页里画布尺寸就是显示尺寸，用不上。

(function () {
    'use strict';

    const SHADER_DIR = 'shaders/';

    // 档位 → 着色器链（与 Kazumi 的 utils/constants.dart 一致，去掉 AutoDownscalePre）
    // 输出像素上限：窗口最大化时不让每条 CNN 都在三四百万像素上跑
    const MAX_PIXELS = 2600000;
    // 超分专用：把画布抬到源的 1.25 倍（越过 Anime4K 放大趟的 1.2 守卫）。
    // 1.25 是刚好够用的最小值 —— 抬得越高越吃填充率，1.25 时 1080p 源 → 2400x1350。
    const SR_BOOST = 1.25;
    // 抬升后的画布像素上限（3.3M 刚好容下 1080p 源的 1.25 倍，1440p 源就不抬了）
    const SR_MAX_PIXELS = 3300000;

    function pickChain(mode, scale) {
        if (!mode || mode === 'off') return null;
        // ★ 就按用户选的档位来，**不要按倍数降级**。
        //   降级的本意是省 GPU，但代价是把效果降没了：倍数≈1 时降到「轻量修复」
        //   （只跑小模型 Restore_S），用户看到的就是「超分压根没用」。
        //   而真正该省的地方着色器自己已经管了 —— 放大趟带 `//!WHEN OUTPUT.w MAIN.w / 1.2 >`
        //   守卫，源已经够大时它自己会跳过；留下的 Restore 趟正是**可见效果**的来源
        //   （线稿加深 + 边缘锐化），实测整条链也就 1ms/帧，不差这点。
        void scale;
        return CHAINS[mode] || CHAINS.efficiency;
    }

    const CHAINS = {
        // 不放大、只修线稿（倍数≈1 时用）
        restore: [
            'Anime4K_Clamp_Highlights',
            'Anime4K_Restore_CNN_S',
        ],
        efficiency: [
            'Anime4K_Clamp_Highlights',
            'Anime4K_Restore_CNN_M',
            'Anime4K_Restore_CNN_S',
            'Anime4K_Upscale_CNN_x2_M',
            'Anime4K_Upscale_CNN_x2_S',
        ],
        quality: [
            'Anime4K_Clamp_Highlights',
            'Anime4K_Restore_CNN_VL',
            'Anime4K_Upscale_CNN_x2_VL',
            'Anime4K_Upscale_CNN_x2_M',
        ],
    };

    const VERT = `#version 300 es
in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}`;

    const shaderCache = new Map();

    async function loadShaderText(name) {
        if (shaderCache.has(name)) return shaderCache.get(name);
        const r = await fetch(SHADER_DIR + name + '.glsl');
        if (!r.ok) throw new Error('着色器缺失: ' + name + ' (' + r.status + ')');
        const t = await r.text();
        shaderCache.set(name, t);
        return t;
    }

    // ---------------------------------------------------------------- 解析

    /// 把一个 .glsl 文件拆成多趟。每趟 = `//!` 指令 + 一个 `vec4 hook() {...}`
    function parsePasses(src) {
        const lines = src.split(/\r?\n/);
        const passes = [];
        let cur = null;
        let body = [];
        const flush = () => {
            if (!cur) return;
            cur.body = body.join('\n');
            passes.push(cur);
            cur = null;
            body = [];
        };
        for (const line of lines) {
            const t = line.trim();
            if (t.startsWith('//!')) {
                // 指令行：新的一趟以 DESC 开头
                const sp = t.indexOf(' ');
                const key = sp < 0 ? t.slice(3) : t.slice(3, sp);
                const val = sp < 0 ? '' : t.slice(sp + 1).trim();
                if (key === 'DESC') {
                    flush();
                    cur = { desc: val, binds: [], when: '', width: '', height: '', save: '', hook: '' };
                }
                if (!cur) continue; // 文件头部的杂项指令
                switch (key) {
                    case 'HOOK': cur.hook = val; break;
                    case 'BIND': cur.binds.push(val); break;
                    case 'SAVE': cur.save = val; break;
                    case 'WIDTH': cur.width = val; break;
                    case 'HEIGHT': cur.height = val; break;
                    case 'WHEN': cur.when = val; break;
                    case 'COMPONENTS': break; // 统一 RGBA16F
                    default: break;
                }
            } else if (cur) {
                body.push(line);
            }
        }
        flush();
        return passes;
    }

    // ------------------------------------------------------- RPN 表达式求值

    /// 求尺寸表达式，如 `MAIN.w` / `MAIN.w 2 *` / `conv2d_tf.h 2 *`
    function evalRpn(expr, vars) {
        const toks = String(expr).trim().split(/\s+/).filter(Boolean);
        const st = [];
        for (const tk of toks) {
            if (tk === '*' || tk === '/' || tk === '+' || tk === '-') {
                const b = st.pop();
                const a = st.pop();
                if (a === undefined || b === undefined) return NaN;
                st.push(tk === '*' ? a * b : tk === '/' ? a / b : tk === '+' ? a + b : a - b);
            } else if (tk === '>') {
                const b = st.pop(), a = st.pop();
                st.push(a > b ? 1 : 0);
            } else if (tk === '<') {
                const b = st.pop(), a = st.pop();
                st.push(a < b ? 1 : 0);
            } else if (tk === '>=') {
                const b = st.pop(), a = st.pop();
                st.push(a >= b ? 1 : 0);
            } else if (tk === '<=') {
                const b = st.pop(), a = st.pop();
                st.push(a <= b ? 1 : 0);
            } else if (tk === '=' || tk === '==') {
                const b = st.pop(), a = st.pop();
                st.push(a === b ? 1 : 0);
            } else if (vars[tk] !== undefined) {
                st.push(vars[tk]);
            } else {
                const n = Number(tk);
                st.push(Number.isFinite(n) ? n : NaN);
            }
        }
        return st.length ? st[st.length - 1] : NaN;
    }

    /// `//!WHEN` 的变量表（OUTPUT=画布，MAIN=当前 MAIN，NATIVE=原始视频）
    function whenVars(mainSize, nativeSize, outSize) {
        return {
            'OUTPUT.w': outSize[0], 'OUTPUT.h': outSize[1],
            'MAIN.w': mainSize[0], 'MAIN.h': mainSize[1],
            'NATIVE.w': nativeSize[0], 'NATIVE.h': nativeSize[1],
        };
    }

    // ------------------------------------------------------------ 着色器生成

    function buildFragment(pass, hookName, boundNames, sizeOf) {
        const L = [];
        L.push('#version 300 es');
        L.push('precision highp float;');
        L.push('precision highp int;');
        L.push('in vec2 v_uv;');
        L.push('out vec4 outColor;');
        L.push('');

        const emitBinding = (n) => {
            L.push(`uniform sampler2D ${n}_s;`);
            L.push(`uniform vec2 ${n}_size;`);
            L.push(`#define ${n}_pos v_uv`);
            L.push(`#define ${n}_pt (vec2(1.0) / ${n}_size)`);
            L.push(`#define ${n}_tex(p) texture(${n}_s, p)`);
            L.push(`#define ${n}_texOff(o) texture(${n}_s, ${n}_pos + (o) * ${n}_pt)`);
        };
        for (const n of boundNames) emitBinding(n);
        // HOOKED 指向被钩住的那张
        L.push(`#define HOOKED_pos ${hookName}_pos`);
        L.push(`#define HOOKED_pt ${hookName}_pt`);
        L.push(`#define HOOKED_size ${hookName}_size`);
        L.push(`#define HOOKED_tex(p) ${hookName}_tex(p)`);
        L.push(`#define HOOKED_texOff(o) ${hookName}_texOff(o)`);
        L.push('');
        L.push(pass.body);
        L.push('');
        L.push('void main() { outColor = hook(); }');
        return L.join('\n');
    }

    // ------------------------------------------------------------- 渲染器

    class Anime4K {
        constructor(video, canvas) {
            this.video = video;
            this.canvas = canvas;
            this.mode = 'off';
            this.passes = null;      // 已编译的趟
            this.programs = [];
            this.slots = new Map();  // name -> {a:{tex,fbo,w,h}, b:{...}, idx}
            this.nativeSize = [0, 0];
            this.videoTex = null;
            this.running = false;
            this.frames = 0;
            this.lastFpsT = 0;
            this.fps = 0;
            this.error = '';
            this.vfcHandle = null;

            const gl = canvas.getContext('webgl2', {
                alpha: false, antialias: false, depth: false, stencil: false,
                premultipliedAlpha: false, preserveDrawingBuffer: false,
                powerPreference: 'high-performance',
            });
            if (!gl) throw new Error('WebGL2 不可用');
            this.gl = gl;

            const cbf = gl.getExtension('EXT_color_buffer_float') || gl.getExtension('EXT_color_buffer_half_float');
            if (!cbf) throw new Error('显卡/驱动不支持浮点渲染目标（EXT_color_buffer_float）');
            this.floatOk = true;

            // ---- 超帧（帧插值）状态 ----
            // ★ 这是**运动自适应帧混合**，不是运动补偿光流插帧（RIFE 那种要跑神经网络，
            //   在 WebView2 里延迟和开销都太大）。好处是零额外解码延迟 —— 只用上一帧，
            //   不需要"未来帧"，所以不会为了插帧而缓冲。
            this.interp = false;
            // ★ 超帧缓存的是**超分之后**的两张帧（不是原始视频帧）：
            //   链每源帧只跑一次，中间帧拿这两张混 —— 这是性能的关键（见 render）。
            this.srSlots = [null, null];   // [上一源帧, 当前源帧] 的超分结果
            this.srIdx = 0;
            this.srReady = false;
            this.outSize = [0, 0];         // 最近一帧的输出尺寸
            this.lastScale = 1;            // 最近一帧的放大倍数
            this.lastFrameAt = 0;      // 最近一次"新画面"的时间
            this.frameInterval = 1 / 24;
            this.lastStamp = -1;       // 用来判断"这一帧是不是新的"
            this.phase = 1;
            this.passRuns = 0;   // 诊断：这一帧里真正执行了多少趟着色器
            // ★ 链每秒跑几次：超帧开着时这才是 GPU 真实负载（以前 = 显示刷新率）
            this.chainRuns = 0;
            this.chainFps = 0;
            // ★ 延迟回收：`_slot` 在尺寸变化时会重建缓冲，但**这一帧的输入可能还引用着
            //   旧纹理**（输入是先解析好的）。当场 deleteTexture 会让本帧的 drawArrays
            //   去采样一张已删除的纹理 → GL_INVALID_OPERATION + 画面局部变黑。
            //   所以先扔进 trash，等这一帧画完再真正释放。
            this.trash = [];

            // 全屏四边形
            this.quad = gl.createBuffer();
            gl.bindBuffer(gl.ARRAY_BUFFER, this.quad);
            gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);

            this.blit = this._program(
                `#version 300 es
precision highp float;
in vec2 v_uv; out vec4 outColor;
uniform sampler2D u_s;
void main() { outColor = vec4(texture(u_s, v_uv).rgb, 1.0); }`
            );
        }

        /// 超帧的混合趟：prev → cur 按 phase 混，运动大的地方少混（避免拖影）
        _blendProgram() {
            if (this._blend) return this._blend;
            this._blend = this._program(
                `#version 300 es
precision highp float;
in vec2 v_uv; out vec4 outColor;
uniform sampler2D u_prev;
uniform sampler2D u_cur;
uniform float u_phase;
void main() {
    vec3 a = texture(u_prev, v_uv).rgb;
    vec3 b = texture(u_cur, v_uv).rgb;
    float d = length(b - a);
    // ★ 运动大的地方**直接显示最新帧**（w→1）。以前是 w→0（显示上一帧），
    //   等于在动的画面上平白多一帧延迟 —— 观感就是「糊 + 迟钝」。
    //   静止/慢速区域才按相位平滑混合（那里才真的需要插中间帧）。
    float w = mix(u_phase, 1.0, smoothstep(0.05, 0.22, d));
    outColor = vec4(mix(a, b, w), 1.0);
}`
            );
            return this._blend;
        }

        _program(fragSrc) {
            const gl = this.gl;
            const vs = gl.createShader(gl.VERTEX_SHADER);
            gl.shaderSource(vs, VERT);
            gl.compileShader(vs);
            if (!gl.getShaderParameter(vs, gl.COMPILE_STATUS)) {
                throw new Error('顶点着色器编译失败: ' + gl.getShaderInfoLog(vs));
            }
            const fs = gl.createShader(gl.FRAGMENT_SHADER);
            gl.shaderSource(fs, fragSrc);
            gl.compileShader(fs);
            if (!gl.getShaderParameter(fs, gl.COMPILE_STATUS)) {
                const log = gl.getShaderInfoLog(fs) || '';
                throw new Error('片元着色器编译失败: ' + log.slice(0, 400));
            }
            const p = gl.createProgram();
            gl.attachShader(p, vs);
            gl.attachShader(p, fs);
            gl.linkProgram(p);
            gl.deleteShader(vs);
            gl.deleteShader(fs);
            if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
                throw new Error('着色器链接失败: ' + gl.getProgramInfoLog(p));
            }
            p._aPos = gl.getAttribLocation(p, 'a_pos');
            return p;
        }

        _slot(name, w, h) {
            let s = this.slots.get(name);
            if (!s || s.w !== w || s.h !== h) {
                if (s) { this._freeSlot(s); }
                s = { w, h, idx: 0, a: this._makeTarget(w, h), b: this._makeTarget(w, h) };
                this.slots.set(name, s);
            }
            return s;
        }

        _makeTarget(w, h) {
            const gl = this.gl;
            const tex = gl.createTexture();
            gl.bindTexture(gl.TEXTURE_2D, tex);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA16F, w, h, 0, gl.RGBA, gl.HALF_FLOAT, null);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
            gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
            const fbo = gl.createFramebuffer();
            gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
            gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
            gl.bindFramebuffer(gl.FRAMEBUFFER, null);
            return { tex, fbo };
        }

        _freeSlot(s) {
            // 不立刻删，见 this.trash 的注释
            for (const k of ['a', 'b']) {
                if (s[k]) this.trash.push(s[k]);
            }
        }

        /// 这一帧画完了，真正释放延迟回收的缓冲
        _flushTrash() {
            const gl = this.gl;
            for (const t of this.trash) {
                try { gl.deleteTexture(t.tex); gl.deleteFramebuffer(t.fbo); } catch (e) {}
            }
            this.trash.length = 0;
        }

        _ensureVideoTex() {
            const gl = this.gl;
            if (!this.videoTex) {
                this.videoTex = gl.createTexture();
                gl.bindTexture(gl.TEXTURE_2D, this.videoTex);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
                gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
            }
            return this.videoTex;
        }

        /// 编译整条链（异步加载着色器文本）
        ///
        /// ★ `mode === 'off'` 时**不能**把管线整个拆掉：只开超帧（不超分）时
        ///   仍然需要这条 WebGL 管线来输出画面。所以这里只清空 passes。
        async setMode(mode) {
            this.mode = mode;
            this.error = '';
            this._destroyPrograms();
            if (mode === 'off') { this.passes = []; this.chainName = ''; return 0; }
            // ★★ 必须用**当前**画布/源尺寸算倍数。以前用 `this.lastScale`（上一次
            //    render 留下的值）→ 第一次挂载时它是 1 → 永远选中轻量链 → 低分辨率源
            //    只做了双线性拉伸、没跑 CNN 放大，画面就是「糊」的。
            let sc = 1;
            try {
                const csz = this._canvasSize();
                const ssz = this._srcSize();
                if (ssz[0] && ssz[1]) sc = Math.max(csz[0] / ssz[0], csz[1] / ssz[1]);
            } catch (e) {}
            this.lastScale = sc;
            const chain = pickChain(mode, sc);
            if (!chain) throw new Error('未知档位: ' + mode);
            this.chainName = chain === CHAINS.quality ? 'quality' : 'efficiency';

            const all = [];
            for (const name of chain) {
                const txt = await loadShaderText(name);
                for (const p of parsePasses(txt)) {
                    p.__file = name;
                    all.push(p);
                }
            }
            this.passes = all;
            this.programs = [];
            this.slots.clear();
            this.srReady = false;   // 换了链，超帧缓存要重建

            // 逐趟编译（此时还不知道尺寸，先只编译骨架：绑定名与输出名是静态的）
            for (const p of this.passes) {
                // ★★ 两个必须踩对的点，踩错任何一个都会让**整条链一趟都不执行**
                //   （症状：超分看着完全没效果，而且不报任何错）：
                //
                // 1) HOOK 名要归一到我们实际有的纹理上。mpv 有 MAIN/LUMA/CHROMA/
                //    PREKERNEL/POSTKERNEL 这些阶段，我们只有一张 RGB 图，
                //    所以除了 NATIVE/OUTPUT 一律映射到 MAIN。
                // 2) **绝对不要把 `//!SAVE` 的名字加进"要读的"列表** —— SAVE 是"写"。
                //    加进去的话每一趟都会去读一张还不存在的纹理 → bind 失败 → continue，
                //    于是 29 趟全部跳过、画面毫无变化。
                const hook = (p.hook === 'NATIVE' || p.hook === 'OUTPUT') ? p.hook : 'MAIN';
                const binds = new Set(p.binds);
                binds.delete('HOOKED');
                binds.add(hook);
                p.__hook = hook;
                p.__bound = Array.from(binds);
                p.__out = (p.save && p.save !== 'HOOKED') ? p.save : hook;
                p.__frag = buildFragment(p, hook, p.__bound);
            }
            return this.passes.length;
        }

        _destroyPrograms() {
            const gl = this.gl;
            for (const p of this.programs) gl.deleteProgram(p.prog);
            this.programs = [];
            for (const s of this.slots.values()) this._freeSlot(s);
            this.slots.clear();
            this._flushTrash();
        }

        _progFor(pass) {
            const key = pass.__file + '#' + pass.desc;
            let hit = this.programs.find((x) => x.key === key);
            if (hit) return hit.prog;
            const prog = this._program(pass.__frag);
            this.programs.push({ key, prog });
            return prog;
        }

        /// 渲染一帧
        render() {
            const gl = this.gl;
            const v = this.video;
            const vw = v.videoWidth || v.width || 0;
            const vh = v.videoHeight || v.height || 0;
            this.dbg = { noSize: 0, when: 0, size: 0, bind: 0, drew: 0 };
            if (!vw || !vh) { this.dbg.noSize = 1; return; }
            this.nativeSize = [vw, vh];

            // 画布跟随显示尺寸，但加两道上限：DPR 1.25 + 总像素 260 万。
            // ★ 不设上限的话，窗口一最大化（比如 2240x1260 CSS）每条 CNN 都要在
            //   三四百万像素上跑，GPU 直接被拖死 —— 用户报的「放大后很慢很糊」。
            //   超出的部分交给 CSS 放大，肉眼几乎看不出。
            // ★ 先把画布的 CSS 尺寸对齐到 <video> 的实际画面框（留黑边），
            //   否则 _outSize() 拿到的是整个盒子，画面会被拉伸。
            this._syncCanvasCss();
            const csz = this._canvasSize();
            const cw = csz[0], ch = csz[1];
            if (this.canvas.width !== cw || this.canvas.height !== ch) {
                this.canvas.width = cw;
                this.canvas.height = ch;
            }
            this.outSize = [cw, ch];
            this.lastScale = Math.max(cw / vw, ch / vh);

            const tex = this._ensureVideoTex();
            const now = performance.now();
            // 这一帧的源画面是不是新的（超帧要靠它决定跑不跑链）
            const stamp = (v.getVideoPlaybackQuality && v.getVideoPlaybackQuality().totalVideoFrames)
                || v.currentTime;
            const isNew = stamp !== this.lastStamp;

            // ★ 上传视频帧很贵：1080p 一帧 8MB，超帧开着时若每个 rAF 都传，
            //   144Hz 就是 1.2 GB/s 的 PCIe 带宽，全被吃光。中间帧用的是缓存好的
            //   超分结果，不需要重传 —— 所以只在**新源帧**上上传。
            if (isNew || !this.interp) {
                gl.bindTexture(gl.TEXTURE_2D, tex);
                gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, true);
                gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, v);
                gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
            }

            if (this.interp) {
                if (isNew) {
                    if (this.lastStamp >= 0) {
                        const dt = (now - this.lastFrameAt) / 1000;
                        // 源帧间隔取滑动平均，抗抖动
                        if (dt > 0.004 && dt < 0.4) {
                            this.frameInterval = this.frameInterval * 0.7 + dt * 0.3;
                        }
                    }
                    this.lastStamp = stamp;
                    this.lastFrameAt = now;
                }
                const span = Math.max(this.frameInterval, 1 / 120);
                this.phase = Math.min(1, Math.max(0, (now - this.lastFrameAt) / 1000 / span));

                // ★★ 性能关键：整条着色器链**只在新源帧上跑一次**（源 24fps），
                //    中间帧只把「上一源帧的超分结果」和「当前源帧的超分结果」混一下。
                //    以前每个 rAF（144Hz）都跑整条链 → 6 倍开销，这就是「很慢」的主因。
                if (isNew || !this.srReady) {
                    const fin = this._runChain(tex, vw, vh, cw, ch);
                    // 拷进固定缓存（链的输出走的是双缓冲槽，下一趟会覆盖它，
                    // 而混合要在下一帧才用 —— 必须自己留一份）
                    const dst = this._srSlot(this.srIdx, fin.w, fin.h);
                    this._blitTo(dst.fbo, fin.tex, fin.w, fin.h);
                    this.srPrev = this.srSlots[this.srIdx ^ 1];
                    this.srCur = dst;
                    this.srIdx ^= 1;
                    this.srReady = true;
                }
                this._blitInterp(this.srPrev, this.srCur, this.phase, cw, ch);
            } else {
                const fin = this._runChain(tex, vw, vh, cw, ch);
                this._blitTo(null, fin.tex, cw, ch);
            }

            this._flushTrash();
            this.frames++;
            if (!this.lastFpsT) this.lastFpsT = now;
            if (now - this.lastFpsT >= 1000) {
                this.fps = Math.round((this.frames * 1000) / (now - this.lastFpsT));
                this.chainFps = Math.round((this.chainRuns * 1000) / (now - this.lastFpsT));
                this.frames = 0;
                this.chainRuns = 0;
                this.lastFpsT = now;
            }
        }

        /// 跑一遍着色器链：srcTex（源 vw×vh）→ 输出 cw×ch。返回链尾 MAIN 的 {tex,w,h}。
        _runChain(srcTex, vw, vh, cw, ch) {
            this.chainRuns++;
            const gl = this.gl;
            const outSize = [cw, ch];
            const virtual = new Map();
            virtual.set('MAIN', { tex: srcTex, w: vw, h: vh });
            virtual.set('NATIVE', { tex: srcTex, w: vw, h: vh });
            virtual.set('OUTPUT', { tex: null, w: cw, h: ch });

            const resolve = (name) => virtual.get(name) || (this.slots.get(name) ? this._current(this.slots.get(name)) : null);
            const sizeVars = (main) => {
                const vars = whenVars(main, [vw, vh], outSize);
                for (const [n, sl] of this.slots) { vars[n + '.w'] = sl.w; vars[n + '.h'] = sl.h; }
                return vars;
            };

            let mainSize = [vw, vh];
            this.passRuns = 0;
            gl.bindBuffer(gl.ARRAY_BUFFER, this.quad);

            for (const p of this.passes) {
                const vars = sizeVars(mainSize);
                if (p.when) {
                    const ok = evalRpn(p.when, vars);
                    if (!(ok > 0)) { this.dbg.when++; continue; }
                }
                const ow = Math.max(1, Math.round(evalRpn(p.width || (p.__hook + '.w'), vars)));
                const oh = Math.max(1, Math.round(evalRpn(p.height || (p.__hook + '.h'), vars)));
                if (!Number.isFinite(ow) || !Number.isFinite(oh) || ow > 8192 || oh > 8192) { this.dbg.size++; continue; }

                const ins = [];
                for (const n of p.__bound) {
                    const t = resolve(n);
                    if (!t) { ins.length = 0; break; }
                    ins.push({ name: n, tex: t.tex, w: t.w, h: t.h });
                }
                if (!ins.length) { this.dbg.bind++; continue; }

                let outTex = null;
                if (p.__out === 'MAIN') {
                    const sl = this._slot('__main', ow, oh);
                    outTex = sl.idx ? sl.a : sl.b;
                    sl.idx ^= 1;
                    mainSize = [ow, oh];
                    virtual.set('MAIN', { tex: outTex.tex, w: ow, h: oh });
                } else {
                    const sl = this._slot(p.__out, ow, oh);
                    outTex = sl.idx ? sl.a : sl.b;
                    sl.idx ^= 1;
                    virtual.set(p.__out, { tex: outTex.tex, w: ow, h: oh });
                }

                gl.bindFramebuffer(gl.FRAMEBUFFER, outTex.fbo);
                gl.viewport(0, 0, ow, oh);
                const prog = this._progFor(p);
                gl.useProgram(prog);
                gl.bindBuffer(gl.ARRAY_BUFFER, this.quad);
                gl.enableVertexAttribArray(prog._aPos);
                gl.vertexAttribPointer(prog._aPos, 2, gl.FLOAT, false, 0, 0);
                for (let i = 0; i < ins.length; i++) {
                    gl.activeTexture(gl.TEXTURE0 + i);
                    gl.bindTexture(gl.TEXTURE_2D, ins[i].tex);
                    gl.uniform1i(gl.getUniformLocation(prog, ins[i].name + '_s'), i);
                    gl.uniform2f(gl.getUniformLocation(prog, ins[i].name + '_size'), ins[i].w, ins[i].h);
                }
                gl.drawArrays(gl.TRIANGLES, 0, 3);
                this.passRuns++;
                this.dbg.drew++;
            }

            return resolve('MAIN') || { tex: srcTex, w: vw, h: vh };
        }

        /// 固定尺寸的缓存槽（超帧要两张：上一源帧/当前源帧的超分结果）
        _srSlot(i, w, h) {
            let sl = this.srSlots[i];
            if (!sl || sl.w !== w || sl.h !== h) {
                if (sl) this.trash.push({ tex: sl.tex, fbo: sl.fbo });
                const t = this._makeTarget(w, h);
                sl = { tex: t.tex, fbo: t.fbo, w: w, h: h };
                this.srSlots[i] = sl;
            }
            return sl;
        }

        /// 原样把一张纹理画到目标（fbo 传 null 就是画布）
        _blitTo(fbo, tex, w, h) {
            const gl = this.gl;
            gl.bindFramebuffer(gl.FRAMEBUFFER, fbo);
            gl.viewport(0, 0, w, h);
            gl.useProgram(this.blit);
            gl.bindBuffer(gl.ARRAY_BUFFER, this.quad);
            gl.enableVertexAttribArray(this.blit._aPos);
            gl.vertexAttribPointer(this.blit._aPos, 2, gl.FLOAT, false, 0, 0);
            gl.activeTexture(gl.TEXTURE0);
            gl.bindTexture(gl.TEXTURE_2D, tex);
            gl.uniform1i(gl.getUniformLocation(this.blit, 'u_s'), 0);
            gl.drawArrays(gl.TRIANGLES, 0, 3);
        }

        /// 超帧出帧：把两张超分结果按相位混一帧再铺到画布
        _blitInterp(prev, cur, phase, cw, ch) {
            if (!prev || !cur || prev.w !== cur.w || prev.h !== cur.h) {
                this._blitTo(null, cur.tex, cw, ch);
                return;
            }
            const gl = this.gl;
            const sl = this._slot('__interp', cur.w, cur.h);
            const outTex = sl.idx ? sl.a : sl.b;
            sl.idx ^= 1;
            const prog = this._blendProgram();
            gl.bindFramebuffer(gl.FRAMEBUFFER, outTex.fbo);
            gl.viewport(0, 0, cur.w, cur.h);
            gl.useProgram(prog);
            gl.bindBuffer(gl.ARRAY_BUFFER, this.quad);
            gl.enableVertexAttribArray(prog._aPos);
            gl.vertexAttribPointer(prog._aPos, 2, gl.FLOAT, false, 0, 0);
            gl.activeTexture(gl.TEXTURE0);
            gl.bindTexture(gl.TEXTURE_2D, prev.tex);
            gl.uniform1i(gl.getUniformLocation(prog, 'u_prev'), 0);
            gl.activeTexture(gl.TEXTURE1);
            gl.bindTexture(gl.TEXTURE_2D, cur.tex);
            gl.uniform1i(gl.getUniformLocation(prog, 'u_cur'), 1);
            gl.uniform1f(gl.getUniformLocation(prog, 'u_phase'), phase);
            gl.drawArrays(gl.TRIANGLES, 0, 3);
            this._blitTo(null, outTex.tex, cw, ch);
        }

        /// 当前该用的输出尺寸：画布 CSS 尺寸 × DPR，再压到像素上限
        _outSize() {
            const r = this.canvas.getBoundingClientRect();
            const dpr = Math.min(window.devicePixelRatio || 1, 1.25);
            let cw = Math.max(2, Math.round(r.width * dpr));
            let ch = Math.max(2, Math.round(r.height * dpr));
            if (cw * ch > MAX_PIXELS) {
                const k = Math.sqrt(MAX_PIXELS / (cw * ch));
                cw = Math.max(2, Math.round(cw * k));
                ch = Math.max(2, Math.round(ch * k));
            }
            return [cw, ch];
        }

        _srcSize() {
            const v = this.video;
            return [v.videoWidth || v.width || 0, v.videoHeight || v.height || 0];
        }

        /// 画布实际尺寸 = max(显示尺寸, 源尺寸封顶 2560x1440)。
        ///
        /// ★ 不能比源小：源 1080p、窗口只有 1278 宽时，把 1080p 缩进 1278 宽的画布
        ///   是一次 WebGL 双线性缩小，比浏览器自己缩放 <video> 还糊（用户实测「非常糊」）。
        ///   缩小这一步交给浏览器做，WebGL 只做 1:1 或放大。
        ///
        /// ★★ 超分开着时再抬到 1.25× 源。原因：Anime4K 的放大趟全带
        ///   `//!WHEN OUTPUT.w MAIN.w / 1.200 > OUTPUT.h MAIN.h / 1.200 > *` 守卫 ——
        ///   画布不到源的 1.2 倍时**整条放大链被跳过**，只剩 Restore 修复，
        ///   实测 1080p 源在 1080p 画布上 29 趟只跑了 15 趟，用户看到的就是
        ///   「超分压根没用」。抬到 1.25× 后 CNN 会真做一次 x2 放大，
        ///   再降到画布尺寸（超采样）—— 细节是真恢复的，不是插值糊出来的。
        ///   只在源本身不大时抬（放大后总像素要留在 SR_MAX_PIXELS 以内），
        ///   源已经比显示大时本来就该走缩小路径，抬了只会白烧 GPU。
        _canvasSize() {
            const o = this._outSize();
            const sz = this._srcSize();
            let w = Math.max(o[0], Math.min(sz[0] || 0, 2560));
            let h = Math.max(o[1], Math.min(sz[1] || 0, 1440));
            if (this.mode && this.mode !== 'off' && sz[0] > 0 && sz[1] > 0) {
                const bw = Math.ceil(sz[0] * SR_BOOST);
                const bh = Math.ceil(sz[1] * SR_BOOST);
                if (bw * bh <= SR_MAX_PIXELS) {
                    w = Math.max(w, bw);
                    h = Math.max(h, bh);
                }
            }
            return [w, h];
        }

        /// ★ 让画布和 <video>（object-fit: contain）**几何完全一致**：在视频盒子里
        ///   按源宽高比摆放，多出来的部分留黑边。
        ///
        ///   不这么做的话画布会把自己拉满整个盒子 —— 盒子比源「方」时画面就被纵向
        ///   拉伸（实测 16:9 的源被拉进 1.37 的盒子，一开超分画面就变形）。
        ///   盒子尺寸由父元素给，这里只设 CSS 尺寸；缓冲区尺寸走 _outSize()。
        _syncCanvasCss() {
            const c = this.canvas, v = this.video;
            const box = c.parentElement ? c.parentElement.getBoundingClientRect() : null;
            const vw = v.videoWidth, vh = v.videoHeight;
            if (!box || !vw || !vh || !box.width || !box.height) return;
            const k = Math.min(box.width / vw, box.height / vh);
            const w = Math.max(2, Math.floor(vw * k));
            const h = Math.max(2, Math.floor(vh * k));
            const sw = w + 'px', sh = h + 'px';
            if (c.style.width !== sw || c.style.height !== sh) {
                c.style.width = sw;
                c.style.height = sh;
            }
        }

        _current(s) { return s.idx ? s.b : s.a; }

    }

    // ------------------------------------------------------------ 对外接口

    let inst = null;
    let loopId = null;
    let loopKind = '';   // 'vfc' | 'raf'

    function supported() {
        try {
            const c = document.createElement('canvas');
            const gl = c.getContext('webgl2');
            if (!gl) return { ok: false, why: 'WebGL2 不可用' };
            const e = gl.getExtension('EXT_color_buffer_float') || gl.getExtension('EXT_color_buffer_half_float');
            if (!e) return { ok: false, why: '显卡/驱动不支持浮点渲染目标' };
            return { ok: true };
        } catch (e) {
            return { ok: false, why: String(e && e.message || e) };
        }
    }

    /// 驱动循环。
    /// ★ 超帧开启时必须用 requestAnimationFrame（按**显示刷新率**跑），
    ///   不能用 requestVideoFrameCallback —— 后者只在有新视频帧时触发（约 24Hz），
    ///   那样根本没机会插出中间帧。
    function stopLoop() {
        if (loopId == null) return;
        try {
            if (loopKind === 'vfc' && inst && typeof inst.video.cancelVideoFrameCallback === 'function') {
                inst.video.cancelVideoFrameCallback(loopId);
            } else {
                cancelAnimationFrame(loopId);
            }
        } catch (e) {}
        loopId = null;
        loopKind = '';
    }

    function startLoop() {
        stopLoop();
        const a = inst;
        if (!a) return;
        // ★ 超帧输出封顶 60fps。源才 24fps，插一帧到 48~60 就够了；
        //   以前跟着显示刷新率跑（这台机是 144Hz），每帧都要混合 + 铺满画布
        //   （放大后画布 2000x1200 = 2.6MP），填充率把视频解码器都饿死了 ——
        //   实测源帧率掉到 10fps，用户看到的就是「放大后很慢」。
        const MIN_DT = 1000 / 60;
        let lastDraw = 0;
        const tick = () => {
            if (!inst || inst !== a) return;
            try {
                // canvas/image 源没有 readyState；video 源要等有帧
                if (a.video.readyState === undefined || a.video.readyState >= 2) {
                    const t = performance.now();
                    // 非超帧走 rVFC（本来就按源帧率触发），不用节流
                    if (!a.interp || t - lastDraw >= MIN_DT - 1) {
                        lastDraw = t;
                        a.render();
                    }
                }
            } catch (e) {
                a.error = String(e && e.message || e);
                a.running = false;
                return;
            }
            schedule();
        };
        const schedule = () => {
            if (!inst || inst !== a) return;
            if (!a.interp && typeof a.video.requestVideoFrameCallback === 'function') {
                loopKind = 'vfc';
                loopId = a.video.requestVideoFrameCallback(tick);
            } else {
                loopKind = 'raf';
                loopId = requestAnimationFrame(tick);
            }
        };
        a.running = true;
        schedule();
    }

    const api = {
        supported,
        /// 管线是否在跑（超分或超帧任一开着）
        isOn() { return !!inst; },
        mode() { return inst ? inst.mode : 'off'; },
        interp() { return !!(inst && inst.interp); },
        fps() { return inst ? inst.fps : 0; },
        error() { return inst ? inst.error : ''; },

        /// 挂到 video 上。
        ///   mode   : 'off' | 'efficiency' | 'quality'
        ///   interp : 是否开超帧
        /// ★ 两者都为 off 时调用方应该直接 detach()，别挂着空管线白跑 GPU。
        async attach(video, canvas, mode, interp) {
            api.detach();
            const ok = supported();
            if (!ok.ok) throw new Error(ok.why);
            const a = new Anime4K(video, canvas);
            a.interp = !!interp;
            const n = await a.setMode(mode || 'off');
            inst = a;
            startLoop();
            return n;
        },

        /// 运行时切换超帧（不用重新加载着色器，只是换个驱动循环 + 开关混合趟）
        setInterp(on) {
            if (!inst) return;
            const wasInterp = inst.interp;
            inst.interp = !!on;
            inst.lastStamp = -1;
            inst.phase = 1;
            inst.srReady = false;   // 两条路径不同，缓存作废重建
            // 驱动方式要从 rVFC 换成 rAF（或反过来）
            if (wasInterp !== inst.interp) startLoop();
        },

        /// 运行时切换超分档位
        async setSr(mode) {
            if (!inst) return 0;
            return await inst.setMode(mode || 'off');
        },

        /// 诊断用：同步渲染一帧（渲染完立刻 readPixels 能拿到内容；
        /// 默认帧缓冲没保留，跨帧再读就是空的）
        renderOnce() { if (inst) inst.render(); },
        frames() { return inst ? inst.frames : 0; },
        running() { return !!(inst && inst.running); },
        passCount() { return inst && inst.passes ? inst.passes.length : 0; },
        /// 诊断：最近一帧真正执行了几趟（跟 passCount 比能看出 WHEN 守卫跳过了多少）
        passRuns() { return inst ? inst.passRuns : 0; },
        /// 状态栏用：输出尺寸 / 源尺寸 / 实际放大倍数 / 当前真正在用的链
        outSize() { return inst ? inst.outSize : [0, 0]; },
        nativeSize() { return inst ? inst.nativeSize : [0, 0]; },
        lastScale() { return inst ? inst.lastScale : 1; },
        chainName() { return inst ? (inst.chainName || '') : ''; },
        /// 着色器链每秒实际执行次数（= GPU 真实负载；超帧开着时远小于显示刷新率才对）
        chainFps() { return inst ? inst.chainFps : 0; },
        /// 探针用：拿到 gl 好调 finish() 等 GPU 干完，测出真实耗时
        __glForTiming() { return inst ? inst.gl : null; },
        /// 窗口尺寸变了 → 放大倍数跨过档位线 → 需要按新倍数重建链
        needsRebuild() {
            if (!inst || !inst.mode || inst.mode === 'off') return false;
            const want = pickChain(inst.mode, inst.lastScale || 1);
            const wantName = want === CHAINS.quality ? 'quality' : 'efficiency';
            return inst.chainName !== wantName;
        },
        dbg() { return inst ? inst.dbg : null; },

        detach() {
            stopLoop();
            if (inst) {
                try { inst._destroyPrograms(); } catch (e) {}
            }
            inst = null;
        },
    };

    window.__vxAnime4K = api;
})();
