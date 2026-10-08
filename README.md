<div align="center">

# 漩涡下载器 · VortexDL

**一个把「找资源 → 高速下载 → 自动解压 → 建快捷方式 → 追番看剧」全串起来的一体化工具**

Windows 桌面应用 · Tauri 2 + Rust · 单文件 exe

[![Platform](https://img.shields.io/badge/平台-Windows%2010%2F11-0078D6?style=flat-square&logo=windows)](https://github.com/JFbj-create/VortexDL/releases/latest)
[![Rust](https://img.shields.io/badge/Rust-1.80%2B-000000?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-24C8DB?style=flat-square&logo=tauri)](https://tauri.app/)
[![License](https://img.shields.io/badge/许可-免费使用-2ea44f?style=flat-square)](#-许可)

[📥 下载安装包](https://github.com/JFbj-create/VortexDL/releases/latest) ·
[🌐 官方网站](https://jfbj-create.github.io/VortexDL/) ·
[💬 QQ 交流群 1083613183](https://qm.qq.com/q/6yBSVcMYmS)

</div>

---

## 目录

- [这个软件解决什么问题](#-这个软件解决什么问题)
- [功能总览](#-功能总览)
- [各模块详解](#-各模块详解)
- [技术架构](#-技术架构)
- [构建与运行](#-构建与运行)
- [常见问题](#-常见问题)
- [许可](#-许可)
- [联系作者](#-联系作者)

---

## 🎯 这个软件解决什么问题

在 Windows 上找游戏资源，通常要经历这么一串折磨：

> 打开浏览器 → 在满屏广告里找下载按钮 → 被引导到网盘 → 开会员 → 下完发现要密码 →
> 解压出来一堆散文件不知道点哪个 → 手动找主程序 → 每次玩游戏都要翻目录

**VortexDL 把这些全部做进一个软件里**，并且针对每一环都做了实测优化：

| 环节 | 传统做法 | VortexDL |
|---|---|---|
| 找资源 | 遍地广告站、逐个翻 | 内置多个资源源的本地索引，离线秒搜 |
| 下载 | 浏览器单线程，几十 KB/s | 自研多线程引擎，实测 **110 MB/s** |
| 断点续传 | 断了从头来 | 分块级别续传，关软件也保留进度 |
| 解压 | 手动开压缩软件、找密码 | 自动解压 + 自动破解压密码 + 失败自动重试 |
| 启动游戏 | 翻目录找 exe | 自动识别主程序 + 建桌面快捷方式（名字可改） |
| 追番看番 | 满屏广告播放器 | 独立播放窗口 + **Anime4K 实时超分** + 记忆播放进度 |
| 看书 | 各种 App 要登录要会员 | 书库：Gutenberg / 国学经典 / 古籍，在线阅读 + 下载 |

---

## ✨ 功能总览

<table>
<tr><td width="50%" valign="top">

### 🎮 游戏资源
- 多个资源源**本地索引**，离线可搜（5 万+ 条）
- 分类浏览 / 关键词搜索 / 卡片式封面墙
- 一键下载 → 自动解压 → 自动建快捷方式
- 成人资源独立页面（三段配比推送）

</td><td width="50%" valign="top">

### ⚡ 下载引擎（SwiftFetch）
- 自研多线程分块下载，**动态块大小 + AIMD 自适应并发**
- 实测 **110 MB/s**（视带宽与源站而定）
- 分块级断点续传 + `.swiftfetch-resume` 边车文件
- BitTorrent 支持（DHT / PEX / WebSeed）
- 429 智能退让、卡死块自动回收

</td></tr>
<tr><td valign="top">

### 📺 动漫追番
- 多源并发搜索 + 按标题相似度自动选源
- **独立播放窗口**（无边框 + 悬停显形浮层）
- **Anime4K 实时超分**（WebGL2 自定义管线）
- **实时超帧**（24fps → 60fps）
- **观看进度记忆**：「继续观看」接着上次那一集那一秒
- 失败自动换源（最多 3 次）

</td><td valign="top">

### 📚 书库
- **公版名著**：Project Gutenberg 7.9 万本（含 **444 本中文经典**：西遊記 / 紅樓夢 / 三国演义 / 唐诗三百首）
- **国学经典**：道德经 / 论语 / 诗经 全文 + 注释
- **古籍善本**：书格古籍影印本，正文可读、可下载
- **本地导入**：自己的 txt / epub 丢进去就能分章阅读
- 全部**免登录免付费**，在线阅读 + 下载到本地
- 收藏 + 阅读进度记忆

</td></tr>
<tr><td valign="top">

### 🛠 修改器商城
- 全量索引 + 封面墙 + **中文名自动翻译**
- 一键下载 + 自动加 Windows Defender 白名单
- 本地库管理：启动 / 卸载 / 打开目录

</td><td valign="top">

### 🎨 界面与体验
- **64 套配色主题**（纯白 / 灰 / 黑系列）
- 全局动效 + 液态玻璃质感
- **性能模式**：低配机器（≤4 核 / ≤4GB）自动关闭 156 处模糊特效
- 应用内更新 + 系统监控 + 资源导航

</td></tr>
</table>

---

## 📦 各模块详解

<details>
<summary><b>游戏资源 & 下载</b>（点击展开）</summary>

- 启动时后台预热索引，首屏秒开
- 卡片墙支持分类筛选、关键词搜索
- 下载弹窗可配置：下载目录 / 解压目录 / 解压密码 / 是否删除压缩包 / 是否建快捷方式
- **下载完成后自动**：解压 → 识别主程序 → 建桌面快捷方式
- **快捷方式名字可改**：解压完成会弹窗，自动推断游戏名并让用户确认或修改；
  名字里的非法字符自动替换成下划线
- 未下载完的任务关软件会**保留为「已暂停」**，下次打开点「继续」按分块续传

**实测速度**（同一个源站、同一台机器）：

| 场景 | 速度 |
|---|---|
| 浏览器直接下载 | ~7 MB/s |
| 早期单连接模式 | ~7 MB/s |
| 现在的动态分块 | **110 MB/s** |

</details>

<details>
<summary><b>动漫追番 & 播放器</b>（点击展开）</summary>

- 多源并发搜索，按标题相似度排序自动选源，**播放失败自动换源**
- 播放独立成窗口：无边框、悬停才显形控件、右侧可收起详情面板
- **Anime4K 超分**：把 mpv 的 Anime4K 着色器移植到 WebGL2，实时对每个视频帧做超分
  - 画布按源宽高比留黑边，绝不拉伸画面
  - 按放大倍数自动选着色器链
  - 链只在新源帧上跑（不跟着显示器刷新率空转），上传按需
- **实时超帧**：光流补帧，把 24fps 的番剧补到 60fps
- **观看进度记忆**：
  - 播放窗口每 5 秒 / 暂停 / 关窗时记录「看到第几集第几秒」
  - 动漫页顶部出现「继续观看」一排卡片（带进度条）
  - 点一下就**回到上次那一集的同一秒**继续看
- 进度条支持**按住拖动**（自定义指针捕获实现，不依赖浏览器原生 range 拖动）

</details>

<details>
<summary><b>书库</b>（点击展开）</summary>

两页：**主页**（推书 + 搜索）、**收藏**。

| 源 | 内容 | 在线阅读 | 下载 |
|---|---|---|---|
| **Gutenberg**（Gutendex API） | 外文名著 7.9 万本 + **中文公版书 444 本** | ✅ | ✅ TXT / EPUB |
| **国学经典**（5000yan） | 道德经 / 论语 / 诗经 等全文 + 译文 + 解析 | ✅ | — |
| **古籍善本**（书格） | 古籍影印本，正文说明 + 官方下载入口 | ✅ | ✅ 入口 |
| **本地导入** | 你自己的 txt / epub | ✅ 自动分章 | — |

- 全部**免登录、免付费、免密钥**
- 阅读器：字号调节、章节下拉跳转、上/下一章、阅读进度记忆
- 收藏 + 「源自检」按钮（一键检测每个源在当前网络下是否可用）

</details>

<details>
<summary><b>修改器商城</b>（点击展开）</summary>

- 索引来自允许抓取的公开 sitemap，封面自动提取
- **中文名自动翻译**（走 Steam 官方商店搜索接口，无需密钥，命中率 93%）
- 下载按游戏分文件夹、自动防重复
- 下载后自动加入 Windows Defender 白名单（不用手动点）
- 本地库：一键启动（自动提权）/ 卸载 / 打开目录

</details>

<details>
<summary><b>界面 · 主题 · 性能</b>（点击展开）</summary>

- **64 套主题**：紫 / 薰衣草 / 品红 / 霓虹 / 纯白 / 灰 / 黑 等系列，一键切换
- 全局动效：卡片悬浮、按钮按压、页面淡入、进度条流光
- 侧边栏 / 卡片 / 弹窗统一圆角与阴影
- **性能模式**：
  - 自动检测 CPU 核心数与内存
  - ≤4 核或 ≤4GB 内存 → 自动开启，关闭全部 156 处 `backdrop-filter` 与动效
  - 也可在设置里手动开关
- 应用内检查更新 + 系统监控（CPU / GPU / 内存 / 磁盘）

</details>

---

## 🏗 技术架构

### 目录结构

```
VortexDL/
├── src/                          前端（编译期内嵌进 exe，brotli 压缩）
│   ├── index.html                主界面（单页 + 多视图）
│   ├── app.js                    主逻辑（约 1.1 万行）
│   ├── styles.css                样式 + 64 套主题变量
│   ├── player.html / player.js   独立播放窗口
│   ├── anime4k.js                Anime4K WebGL2 超分管线
│   ├── shaders/                  GLSL 着色器（Anime4K v4 移植）
│   └── vendor/hls.min.js         HLS 播放
│
├── src-tauri/
│   ├── src/
│   │   ├── main.rs               入口 + 命令注册
│   │   ├── commands.rs           Tauri 命令（下载 / 解压 / 快捷方式 …）
│   │   ├── search_engine.rs      资源搜索 + 多源聚合（本地索引）
│   │   ├── books.rs              书库（多源 + 阅读 + 下载）
│   │   ├── anime.rs              动漫（元数据 / 搜索 / 剧集 / 播放地址）
│   │   ├── kazumi.rs             动漫源规则引擎（XPath 子集）
│   │   ├── gx.rs                 某来源游戏库
│   │   ├── trainer.rs            修改器
│   │   ├── downloader.rs         下载引擎对接（SwiftFetch）
│   │   ├── extractor.rs          解压（7z / zip 纯 Rust）
│   │   ├── licensing.rs          离线授权校验
│   │   └── ...
│   ├── kazumi_rules/             动漫源规则（17 条 JSON）
│   └── tauri.conf.json
│
└── SwiftFetch/                   下载内核（本仓库同级目录，path 依赖）
    └── src/
        ├── dynamic_engine.rs     动态分块 HTTP 引擎
        ├── bt_engine.rs          BitTorrent 引擎
        ├── smart_sched.rs        带宽自适应调度
        └── ...
```

### 技术要点

| 点 | 做法 |
|---|---|
| **前端分发** | 前端在**编译期**内嵌进 exe（`frontendDist: "../src"` + brotli），单文件发布，改前端必须重新编译 |
| **下载引擎** | 无静态预切分，按实测速度**动态派生块**；块数上限随文件大小动态放大（最大 512），尾部不再并发塌陷 |
| **Range 探测** | 先探 `bytes=0-0`，若返回 200 再补一次**中段区间**探测 —— 有的 CDN 首页响应不带 `Content-Range`，只探首字节会把支持分段的服务器误判成不支持（会退化成单连接 7MB/s） |
| **429 处理** | 识别 `Retry-After` + 全局静默 + 指数退让 ceiling，避免"限速反而更慢" |
| **卡死块** | 5 秒无进度的块自动回收重新入队 |
| **超分** | Anime4K v4 GLSL 移植到 WebGL2（RGBA16F 中间纹理），画布 ≥ 1.25× 源尺寸才能过着色器的 `//!WHEN` 守卫 |
| **超帧** | 光流估计 + 运动补偿插值，输出封顶 60fps，链只在新源帧上跑 |
| **书源免登录** | 只用开放 API / 无需鉴权的公开页面；GBK 页面用 Win32 `MultiByteToWideChar` 解码（不引入额外依赖） |
| **授权** | 纯离线哈希校验，不联网、不绑机器码；免费档哈希明文，付费档哈希拆 4 段 + 位移加密分散在 4 个编译单元 |

---

## 🔨 构建与运行

### 环境要求

- **Rust 1.80+**（MSVC 工具链，不是 GNU）
- **Visual Studio 2022 Build Tools**（含 C++ 桌面开发）
- **Node.js**（可选，仅用于语法检查）

> ⚠️ **必须用 MSVC 目标**。GNU 目标在本项目上会产生不兼容资源文件导致崩溃。

### 构建步骤

```bat
:: 1) 初始化 MSVC 环境
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"

:: 2) 编译
cd VortexDL\src-tauri
cargo build --release --target x86_64-pc-windows-msvc

:: 产物：src-tauri\target\x86_64-pc-windows-msvc\release\vortex-dl.exe
```

### 运行测试

```bat
cd VortexDL\src-tauri
cargo test --release --target x86_64-pc-windows-msvc

:: 跑真实网络冒烟测试（会真的访问书源 / 番剧源）
cargo test --release --target x86_64-pc-windows-msvc -- --ignored
```

> 📦 **关于 `resources_main/`**：本仓库**不含二进制与运行时数据**
> （`7z.exe`/`7z.dll`、`swiftfetch.exe`、`WebView2Loader.dll`、`game_index.json` 等），
> 一是体积，二是这些都是第三方或构建产物。要完整打包请自备：
>
> | 文件 | 来源 |
> |---|---|
> | `7z.exe` / `7z.dll` | [7-Zip](https://www.7-zip.org/)（LGPL） |
> | `WebView2Loader.dll` | WebView2 SDK |
> | `swiftfetch.exe` | 在本仓库 `SwiftFetch/` 里 `cargo build --release` 得到 |
> | `game_index.json` / `translations.json` | 运行时自动生成 |
>
> 只跑 `cargo build` 编译主程序不需要这些（只有 NSIS 打包会用到）。

### 打包安装包

```bat
:: 需要 NSIS
makensis installer.nsi
:: 产物：VortexDL-Setup.exe
```

> 📌 NSIS 脚本里的 `LICENSE.txt` 和 `installer.nsi` 自己**都必须带 UTF-8 BOM**，
> 否则中文会变成乱码（NSIS 靠 BOM 判断编码）。

---

## ❓ 常见问题

<details>
<summary><b>下载速度慢 / 卡在「连接中」</b></summary>

1. **GX 类来源的下载链接是预签名 URL，1 小时过期**。放置太久或暂停很久再继续会失效 ——
   点「↻ 重试」会自动重新获取链接（新版本已自动处理）。
2. 某些 CDN 对**新建连接数**限流。软件内置 429 智能退让，遇到限速会自动降并发恢复。
3. 如果长时间 0 字节，界面会提示具体原因，不会一直只显示"连接中"。

</details>

<details>
<summary><b>动漫搜不到 / 播放不了</b></summary>

- 源站本身可能挂了或被墙。软件会**并发搜多个源并自动换源**（最多 3 次）。
- 纯 HTTP 只能覆盖一部分源；需要 JS 渲染的源无法支持（这是源的实现方式决定的，不是软件问题）。
- 可以点动漫页的「数据源」手动换一条线路。

</details>

<details>
<summary><b>书库某个源打不开</b></summary>

点书库页右上角的 **「源自检」**，会依次检测每个源在当前网络下是否可用并打印耗时。
公版书走 Gutenberg（国外站点），国内访问偶发超时，重试通常就好。

</details>

<details>
<summary><b>快捷方式指向了错误的游戏</b></summary>

平铺解压（所有游戏都解到同一个目录）时，自动识别可能挑错。
软件的处理方式：**共享目录下名字对不上就明确报错，宁可不建也不建错的**。
另外解压完成会弹窗让你确认，可以自己从候选列表里挑正确的 exe。

</details>

<details>
<summary><b>老电脑卡顿</b></summary>

设置里有**性能模式**（≤4 核或 ≤4GB 内存会自动开启），会关闭全部模糊特效与动效。
也可以手动开关试试哪个更顺。

</details>

---

## 📄 许可

本项目**免费提供使用**。任何人都可以自由使用、复制和分发本软件。

本软件按"现状"提供，不附带任何明示或暗示的担保。在适用法律允许的最大范围内，
作者不对因使用本软件而产生的任何直接、间接、偶然、特殊或后果性损害承担责任。

使用本软件即表示您已阅读并同意本许可协议的条款。

> 📌 **关于本仓库**：这里发布的是**公开源码版**。
> 为保护付费档位的安全性，**豪华版密钥校验相关的常量已移除**（免费 / 测试 / 开发者三档完好）。
> 详见 `src-tauri/src/licensing.rs` 顶部说明。

---

## 💬 联系作者

<div align="center">

| | |
|---|---|
| **作者 QQ** | [1523373515](https://wpa.qq.com/msgrd?v=3&uin=1523373515&site=qq&menu=yes) |
| **QQ 交流群** | [1083613183](https://qm.qq.com/q/6yBSVcMYmS) |
| **问题反馈** | [GitHub Issues](https://github.com/JFbj-create/VortexDL/issues) |

遇到问题先看上面的[常见问题](#-常见问题)，还不行就加群或提 issue。

</div>

---

<div align="center">

**如果这个软件帮到了你，欢迎给个 ⭐ Star**

</div>
