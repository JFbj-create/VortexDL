# VortexDL 模块开发教程

VortexDL 支持通过 **拖拽 ZIP 包** 安装工具模块。模块是自包含的 Web 工具页面，安装后会显示在「工具」页面。

## 模块包格式

模块是一个 `.zip` 压缩包，必须包含以下文件：

```
my_module.zip
├── manifest.json      ← 必需，模块清单
├── index.html          ← 必需（默认入口），工具页面内容
└── assets/             ← 可选，静态资源（图片/CSS/JS）
    ├── style.css
    ├── script.js
    └── icon.png
```

## manifest.json 字段说明

| 字段 | 必填 | 类型 | 说明 |
|------|------|------|------|
| `id` | 是 | string | 模块唯一标识，只能含小写字母/数字/连字符，如 `my-tool` |
| `name` | 是 | string | 显示名称（中文亦可），如 `我的工具` |
| `icon` | 是 | string | emoji 图标，或 `assets/icon.png` 图片路径 |
| `section` | 是 | string | 分区分类，如 `工具` / `游戏` / `首页` |
| `description` | 否 | string | 一句话描述 |
| `entry` | 否 | string | 入口文件名，默认 `index.html` |
| `version` | 否 | string | 版本号，如 `1.0.0` |
| `author` | 否 | string | 作者名 |

### manifest.json 模板

```json
{
  "id": "my-tool",
  "name": "我的工具",
  "icon": "🔧",
  "section": "工具",
  "description": "一个示例工具模块",
  "entry": "index.html",
  "version": "1.0.0",
  "author": "你的名字"
}
```

## index.html 编写

模块页面是纯 HTML/CSS/JS，会被注入到工具页面容器中渲染。

**重要：** 模块 HTML 运行在 WebView2 沙箱中，可直接调用 VortexDL 暴露的 Tauri 接口（见下方「与主程序交互」章节）。

```html
<!-- index.html 示例 -->
<style>
  .my-tool { padding: 24px; color: #e0e0e0; }
  .my-tool h2 { color: #a78bfa; margin-bottom: 16px; }
  .btn {
    background: #6d28d9; color: #fff; border: none;
    padding: 10px 20px; border-radius: 8px; cursor: pointer;
  }
</style>

<div class="my-tool">
  <h2>🔧 我的工具</h2>
  <p>这是一个示例模块。</p>
  <button class="btn" onclick="runAction()">点击执行</button>
  <div id="result"></div>
</div>

<script>
  async function runAction() {
    document.getElementById('result').textContent = '执行中...';
    // 可调用 Tauri 接口，例如读取设置
    try {
      const dir = await window.__TAURI_INTERNALS__.invoke('get_setting', { key: 'download_dir' });
      document.getElementById('result').textContent = '下载目录: ' + dir;
    } catch (e) {
      document.getElementById('result').textContent = '出错: ' + e;
    }
  }
</script>
```

## 与主程序交互（可选）

模块可通过 Tauri 的 `invoke` 调用 VortexDL 后端命令。在 `<script>` 中使用：

```javascript
// 标准 Tauri 调用方式
const { invoke } = window.__TAURI__.core;
const result = await invoke('command_name', { param1: 'value' });

// 兼容旧版 WebView 的写法
const result = await window.__TAURI_INTERNALS__.invoke('command_name', { param1: 'value' });
```

### 常用可用命令

| 命令 | 参数 | 返回 | 说明 |
|------|------|------|------|
| `get_setting` | `{ key }` | `String` | 读取设置项（如 `download_dir` / `extract_dir`） |
| `fetch_downloads` | — | `Vec<DownloadItem>` | 获取下载列表 |
| `browse_path` | — | `String` | 弹出目录选择对话框 |
| `open_external` | `{ url }` | — | 用系统默认浏览器打开链接 |
| `list_modules` | — | `Vec<ModuleInfo>` | 列出所有已安装模块 |

> 注意：仅能调用 VortexDL 在 `generate_handler!` 中注册的命令。

## 打包与安装

### 1. 准备目录结构

```
my_module_src/         ← 源码目录（非必需，仅组织用）
├── manifest.json
├── index.html
└── assets/
    └── icon.png
```

### 2. 打包成 ZIP

用 7-Zip / WinRAR / PowerShell 将内容（注意：是文件本身，不是包含它们的父文件夹）打包：

```powershell
# PowerShell 打包示例（在源码目录内执行）
Compress-Archive -Path manifest.json, index.html, assets -DestinationPath my_module.zip
```

**关键：** ZIP 内部根目录必须直接是 `manifest.json` 和 `index.html`，不能多套一层文件夹。正确与错误示例：

```
✅ 正确结构                ❌ 错误结构（多套一层）
my_module.zip              my_module.zip
├── manifest.json          └── my_module_src/
├── index.html                 ├── manifest.json
└── assets/                     ├── index.html
    └── ...                     └── assets/
```

> 说明：即便 ZIP 内有多层嵌套，安装程序也会递归查找 `manifest.json`，但规范做法是放在根目录。

### 3. 安装

启动 VortexDL → 进入「工具」页面 → 将 `.zip` 文件拖拽到「已安装模块」区域 → 自动解压安装。

安装后模块会持久化保存在 `{VortexDL.exe 同目录}/modules/{module_id}/`，重启软件后仍然存在。

### 4. 卸载

在工具页面点击模块卡片右上角的 `×` 按钮，确认后即可卸载。

## 调试技巧

- 打开 WebView2 开发者工具：在模块页面右键 → 检查（或按 F12，若已启用）
- 模块文件实际存放路径：`{exe_dir}/modules/{module_id}/`
- 修改已安装模块的 `index.html` 后，重新打开模块即可看到改动（无需重新安装）
- 模块加载失败时，检查 `manifest.json` 是否为合法 JSON，以及 `id` / `name` 是否非空

## 完整示例

本教程附带两个示例：

| 目录 | 说明 |
|------|------|
| `example_simple/` | 最小可用模块，纯静态 HTML |
| `example_advanced/` | 调用 Tauri 接口、带样式与脚本的高级模块 |

进入对应目录，按上面 PowerShell 命令打包成 ZIP 即可安装体验。
