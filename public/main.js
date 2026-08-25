/* ============================================================
   DSH Launcher 官网脚本
   - 中英双语切换（data-i18n / data-i18n-html）
   - 按语言切换主截图
   - 平台检测，高亮推荐下载
   - 导航滚动态、入场动画
   ============================================================ */

(function () {
  "use strict";

  /* ---------------- 文案字典 ---------------- */

  var I18N = {
    zh: {
      "nav.how": "运行原理",
      "nav.features": "特性",
      "nav.download": "下载",
      "nav.faq": "FAQ",

      "hero.badge": "v0.3.6 · React + Tauri · MIT 开源",
      "hero.badge2": "非官方启动器 · 不改动官方 Harness",
      "hero.title": "双击启动，打开即用。<br /><span class=\"grad\">浏览器里，就是官方 Web UI。</span>",
      "hero.sub": "DSH Launcher 是 DeepSeek Harness 的非官方桌面启动器。它不做二次包装、不改交互逻辑——只负责把官方服务稳稳地跑起来，然后把服务发布的 Web UI 地址交给你，在你喜欢的浏览器里打开。",
      "hero.cta1": "立即下载",
      "hero.cta2": "查看 GitHub",
      "hero.note": "macOS（Apple Silicon / Intel）与 Windows x64 · 界面支持中文与 English",

      "thin.kicker": "设计取向",
      "thin.title": "薄，是故意的。<span class=\"grad\">启动器只做启动。</span>",
      "thin.sub": "不内置账号体系，不 fork 官方界面，不捆绑多余运行时。DSH Launcher 是一个「薄壳」：准备好官方依赖、拉起官方服务、把官方地址交出去——你在浏览器里用到的，始终是 DeepSeek Harness 原版 Web UI。",
      "thin.no1t": "不重新包装",
      "thin.no1d": "没有自绘的聊天界面、没有中转层。界面、会话、模型能力全部来自官方 @deepseek-ai/dsh。",
      "thin.no2t": "不抢占地址",
      "thin.no2d": "显示和打开的永远是服务自己发布的 dsh web: <URL>；官方默认端口被占用时，用官方 --port 0 自动换空闲端口。",
      "thin.no3t": "不碰你的数据",
      "thin.no3d": "数据独立存放在 ~/.dsh-desktop，升级不丢配置与会话；密钥类信息永远不会进入安装目录。",

      "how.kicker": "运行原理",
      "how.title": "三步，从安装到<span class=\"grad\">官方工作台</span>",
      "how.s1t": "下载安装",
      "how.s1d": "macOS 使用 DMG，Windows 使用按用户安装的 NSIS 安装包。不需要自己装 Node，不需要敲任何命令。",
      "how.s2t": "双击启动",
      "how.s2d": "首次启动自动下载并校验固定版本的 Node 与官方 @deepseek-ai/dsh：SHA-256 校验、staging 安装、原子替换，失败自动回滚。",
      "how.s3t": "打开 Web UI",
      "how.s3d": "服务就绪后窗口会显示实际地址与运行时长，点一下「打开 Web UI」，在你选择的浏览器里进入官方界面。",
      "how.term": "启动过程 · 启动器日志",
      "how.l1": "校验版本标记、manifest、Node 与 dsh 入口",
      "how.l2": "本地服务已启动（官方默认端口）",
      "how.l3": "检测到官方新版本 → 提示「立即更新」",
      "how.l4": "→ 窗口显示实际地址 + 运行时长 +「打开 Web UI」按钮",

      "shots.kicker": "界面",
      "shots.title": "桌面只管启动，<span class=\"grad\">工作发生在浏览器</span>",
      "shots.main": "启动器",
      "shots.mainAlt": "DSH Launcher 主界面截图",
      "shots.plugin": "插件市场",
      "shots.pluginAlt": "DSH Launcher 插件市场页面截图",
      "shots.c1": "启动器窗口 · 中文界面（可在侧栏随时切换 English）",
      "shots.c2": "浏览器里的官方 Web UI · 完整模型与会话能力",

      "feat.kicker": "特性",
      "feat.title": "小事做稳，<span class=\"grad\">剩下的交给官方</span>",
      "feat.f1t": "双击即用",
      "feat.f1d": "运行时（Node 与 dsh）首次启动自动下载安装，之后每次启动先校验再拉起服务，全程无需命令行。",
      "feat.f2t": "官方 Web UI",
      "feat.f2d": "地址由服务自己发布，点击即用所选浏览器打开。启动器永不替换为自建地址或端口。",
      "feat.f3t": "多浏览器选择",
      "feat.f3d": "自动检测已安装的浏览器：只有一款就直接打开，多款则下拉选择；找不到时用系统默认浏览器兜底。",
      "feat.f4t": "托盘常驻",
      "feat.f4d": "关闭窗口只是隐藏到系统托盘，本地服务继续运行；托盘里的「退出」才会完整停止服务进程树。",
      "feat.f5t": "校验与回滚",
      "feat.f5d": "Node 归档必须匹配固定的 SHA-256；安装走 staging + 原子替换，失败保留旧版本，可回滚目录始终在手。",
      "feat.f6t": "中英双语",
      "feat.f6d": "首次按系统语言自动选择中文或 English，侧栏菜单可随时切换，无需重启服务。",
      "feat.f7t": "更新提醒",
      "feat.f7d": "启动时检查 npm 上 @deepseek-ai/dsh 是否有更新的语义化版本，有则一键「立即更新」，失败自动回退。",
      "feat.f8t": "数据隔离",
      "feat.f8d": "服务数据独立存放于 ~/.dsh-desktop/dsh-home。首次发现兼容的 ~/.dsh 或 CC Switch 数据时，由你明确选择；确认后会先校验备份和恢复能力，再原子导入且不覆盖已有值。",

      "dl.kicker": "下载",
      "dl.title": "选择你的平台<span class=\"grad\">开始</span>",
      "dl.sub": "当前版本 v0.3.6。下载页提供 macOS 双架构 DMG 与 Windows x64 安装程序。",
      "dl.rec": "推荐",
      "dl.macArm": "Apple Silicon（M 系列芯片）",
      "dl.macIntel": "Intel 芯片",
      "dl.btn": "下载 DMG",
      "dl.btnInstaller": "下载安装程序",
      "dl.note1": "Windows 现在使用按用户安装的 NSIS 安装包，不再发布便携 ZIP。首个 Tauri 版本需手动安装，此后桌面更新会在后台下载并等待确认重启。",
      "dl.all": "查看全部历史版本 →",

      "faq.kicker": "常见问题",
      "faq.title": "FAQ",
      "faq.q1": "它和 DeepSeek 官方是什么关系？",
      "faq.a1": "DSH Launcher 是非官方启动器。它运行的是未修改的官方 @deepseek-ai/dsh 包，该包仍受其自身许可条款约束；本仓库的 MIT 协议仅覆盖启动器自身代码。",
      "faq.q2": "需要我自己安装 Node.js 吗？",
      "faq.a2": "不需要。首次启动时启动器会自动下载固定版本的 Node 归档（经 SHA-256 校验）并安装精确版本的 @deepseek-ai/dsh，全程在桌面窗口内完成。",
      "faq.q3": "默认端口被占用了怎么办？",
      "faq.a3": "不用处理。服务报 EADDRINUSE 时，启动器会自动用官方 --port 0 让系统分配空闲端口重试，并始终显示服务发布的实际地址。",
      "faq.q4": "我的会话和配置存在哪里？",
      "faq.a4": "全部在 ~/.dsh-desktop/ 下，与安装目录和已有的 ~/.dsh 相互隔离；升级启动器或更新 Harness 都不会丢失配置与会话。",
      "faq.q5": "关闭窗口后服务还在吗？",
      "faq.a5": "在。关闭窗口只会把它隐藏到系统托盘，服务继续运行；想完全停止，请使用托盘菜单里的「退出」。",

      "footer.note": "© 2026 DSH Launcher · MIT License"
    },

    en: {
      "nav.how": "How it works",
      "nav.features": "Features",
      "nav.download": "Download",
      "nav.faq": "FAQ",

      "hero.badge": "v0.3.6 · React + Tauri · MIT open source",
      "hero.badge2": "Unofficial launcher · Harness stays untouched",
      "hero.title": "Double-click. It just runs.<br /><span class=\"grad\">The official Web UI, in your browser.</span>",
      "hero.sub": "DSH Launcher is an unofficial desktop launcher for DeepSeek Harness. No repackaging, no altered interaction — it simply gets the official service running reliably, then hands you the Web UI address the service publishes, opened in the browser you prefer.",
      "hero.cta1": "Download now",
      "hero.cta2": "View on GitHub",
      "hero.note": "macOS (Apple Silicon / Intel) and Windows x64 · UI in 中文 and English",

      "thin.kicker": "Design philosophy",
      "thin.title": "Thin on purpose. <span class=\"grad\">A launcher only launches.</span>",
      "thin.sub": "No account system, no forked interface, no bundled extras. DSH Launcher is a thin shell: it prepares the official dependencies, starts the official service, and hands over the official address — what you use in the browser is always the original DeepSeek Harness Web UI.",
      "thin.no1t": "No repackaging",
      "thin.no1d": "No self-drawn chat UI, no middle layer. The interface, sessions, and model capabilities all come from the official @deepseek-ai/dsh.",
      "thin.no2t": "No address hijacking",
      "thin.no2d": "What it shows and opens is always the service's own dsh web: <URL>; when the official default port is taken, it retries with the official --port 0 on a free port.",
      "thin.no3t": "Hands off your data",
      "thin.no3d": "Data lives independently in ~/.dsh-desktop, so upgrades never lose settings or sessions; secrets never enter the install directory.",

      "how.kicker": "How it works",
      "how.title": "Three steps to the <span class=\"grad\">official workspace</span>",
      "how.s1t": "Download & install",
      "how.s1d": "Use a DMG on macOS or the per-user NSIS installer on Windows. No Node to install, no commands to type.",
      "how.s2t": "Double-click to launch",
      "how.s2d": "On first launch it downloads and verifies the pinned Node build and the official @deepseek-ai/dsh: SHA-256 checks, staged install, atomic swap, automatic rollback on failure.",
      "how.s3t": "Open the Web UI",
      "how.s3d": "Once the service is ready, the window shows the actual address and uptime. Click \"Open Web UI\" and land in the official interface, in the browser you chose.",
      "how.term": "Launch sequence · launcher log",
      "how.l1": "Validate version marker, manifests, Node & dsh entry points",
      "how.l2": "Local service started (official default port)",
      "how.l3": "Newer official release found → offers \"Update now\"",
      "how.l4": "→ Window shows the actual address + uptime + \"Open Web UI\" button",

      "shots.kicker": "Interface",
      "shots.title": "The desktop launches; <span class=\"grad\">work happens in the browser</span>",
      "shots.main": "Launcher",
      "shots.mainAlt": "DSH Launcher main window screenshot",
      "shots.plugin": "Plugin marketplace",
      "shots.pluginAlt": "DSH Launcher plugin marketplace screenshot",
      "shots.c1": "Launcher window · English UI (switch to 中文 anytime in the sidebar)",
      "shots.c2": "The official Web UI in the browser · full model and session capabilities",

      "feat.kicker": "Features",
      "feat.title": "The small things, done right — <span class=\"grad\">the rest stays official</span>",
      "feat.f1t": "Double-click ready",
      "feat.f1d": "The runtime (Node and dsh) downloads automatically on first launch; every later launch validates before starting the service. No command line, ever.",
      "feat.f2t": "Official Web UI",
      "feat.f2d": "The address is published by the service itself and opened in your chosen browser with one click. The launcher never substitutes its own host or port.",
      "feat.f3t": "Browser picker",
      "feat.f3d": "Detects installed browsers automatically: one browser means a single open button, several add a picker menu; the system default is the fallback.",
      "feat.f4t": "Lives in the tray",
      "feat.f4d": "Closing the window just hides it to the system tray while the service keeps running; only the tray Quit command stops the full process tree.",
      "feat.f5t": "Verified & reversible",
      "feat.f5d": "Node archives must match the pinned SHA-256; installs go through staging with an atomic swap, failures keep the old version, and a rollback copy is always kept.",
      "feat.f6t": "中文 & English",
      "feat.f6d": "Follows your system language on first launch; switch anytime from the sidebar menu without restarting the service.",
      "feat.f7t": "Update prompts",
      "feat.f7d": "Checks npm for a strictly newer @deepseek-ai/dsh at launch and offers a one-click \"Update now\", with automatic rollback on failure.",
      "feat.f8t": "Data isolation",
      "feat.f8d": "Service data lives in ~/.dsh-desktop/dsh-home. When compatible ~/.dsh or CC Switch data is found, you choose explicitly; approval verifies backup and recovery before an atomic, missing-only import.",

      "dl.kicker": "Download",
      "dl.title": "Pick your platform <span class=\"grad\">and start</span>",
      "dl.sub": "Current version v0.3.6. The download page provides dual-architecture macOS DMGs and a Windows x64 installer.",
      "dl.rec": "Recommended",
      "dl.macArm": "Apple Silicon (M-series)",
      "dl.macIntel": "Intel",
      "dl.btn": "Download DMG",
      "dl.btnInstaller": "Download installer",
      "dl.note1": "Windows now uses a per-user NSIS installer; there is no portable ZIP. Install the first Tauri release manually, then desktop updates download in the background and wait for restart confirmation.",
      "dl.all": "Browse all past releases →",

      "faq.kicker": "FAQ",
      "faq.title": "FAQ",
      "faq.q1": "How is it related to DeepSeek officially?",
      "faq.a1": "DSH Launcher is unofficial. It runs the unmodified official @deepseek-ai/dsh package, which remains under its own license and terms; this repository's MIT license covers only the launcher code.",
      "faq.q2": "Do I need to install Node.js myself?",
      "faq.a2": "No. On first launch the launcher downloads a pinned Node archive (verified by SHA-256) and installs an exact version of @deepseek-ai/dsh — all inside the desktop window.",
      "faq.q3": "What if the default port is taken?",
      "faq.a3": "Nothing to do. When the service reports EADDRINUSE, the launcher retries with the official --port 0 option so the OS picks a free port, and always shows the address the service actually publishes.",
      "faq.q4": "Where are my sessions and settings stored?",
      "faq.a4": "Everything lives under ~/.dsh-desktop/, isolated from both the install directory and an existing ~/.dsh. Upgrading the launcher or Harness never loses settings or sessions.",
      "faq.q5": "Does the service keep running after I close the window?",
      "faq.a5": "Yes. Closing the window only hides it to the system tray while the service keeps running. To stop everything, use Quit in the tray menu.",

      "footer.note": "© 2026 DSH Launcher · MIT License"
    }
  };

  var STORAGE_KEY = "dsh-launcher-site-lang";

  /* ---------------- 语言 ---------------- */

  function detectLang() {
    // 1) URL 参数优先（便于直接分享某个语言版本）
    try {
      var q = new URLSearchParams(window.location.search).get("lang");
      if (q === "zh" || q === "en") return q;
    } catch (e) { /* 忽略 */ }
    // 2) 本地保存的选择
    try {
      var saved = localStorage.getItem(STORAGE_KEY);
      if (saved === "zh" || saved === "en") return saved;
    } catch (e) { /* 隐私模式下 localStorage 可能不可用 */ }
    // 3) 浏览器语言
    var nav = (navigator.language || "en").toLowerCase();
    return nav.indexOf("zh") === 0 ? "zh" : "en";
  }

  function applyLang(lang) {
    var dict = I18N[lang] || I18N.zh;

    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";

    document.querySelectorAll("[data-i18n]").forEach(function (el) {
      var key = el.getAttribute("data-i18n");
      if (dict[key] !== undefined) el.textContent = dict[key];
    });

    document.querySelectorAll("[data-i18n-html]").forEach(function (el) {
      var key = el.getAttribute("data-i18n-html");
      if (dict[key] !== undefined) el.innerHTML = dict[key];
    });

    document.querySelectorAll("[data-i18n-alt]").forEach(function (el) {
      var key = el.getAttribute("data-i18n-alt");
      if (dict[key] !== undefined) el.alt = dict[key];
    });

    // 所有产品截图跟随语言；新增截图时只需声明两种语言的路径。
    document.querySelectorAll("[data-screenshot-zh][data-screenshot-en]").forEach(function (img) {
      img.src = img.getAttribute("data-screenshot-" + lang);
    });

    var toggle = document.getElementById("langToggle");
    if (toggle) toggle.textContent = lang === "zh" ? "EN" : "中文";

    try { localStorage.setItem(STORAGE_KEY, lang); } catch (e) { /* 忽略 */ }
  }

  var lang = detectLang();
  applyLang(lang);

  var toggle = document.getElementById("langToggle");
  if (toggle) {
    toggle.addEventListener("click", function () {
      lang = lang === "zh" ? "en" : "zh";
      applyLang(lang);
    });
  }

  /* ---------------- 平台检测：高亮推荐下载 ---------------- */

  (function markRecommended() {
    var ua = navigator.userAgent || "";
    var platform = null;

    if (/Windows/i.test(ua)) {
      platform = "win-x64";
    } else if (/Mac OS X|Macintosh/i.test(ua)) {
      // macOS 的 UA 里 Apple Silicon 仍带 Intel 字样，无法可靠区分，
      // 统一推荐 Apple Silicon 版（M 系列已是主流，Intel 用户可手动选另一张卡）。
      platform = "mac-arm64";
    }

    if (!platform) return;
    var card = document.querySelector('.dl-card[data-platform="' + platform + '"]');
    if (card) card.classList.add("recommended");
  })();

  /* ---------------- 导航滚动态 ---------------- */

  var nav = document.getElementById("nav");
  function onScroll() {
    if (!nav) return;
    nav.classList.toggle("scrolled", window.scrollY > 24);
  }
  window.addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  /* ---------------- 入场动画 ---------------- */

  var revealEls = document.querySelectorAll(".reveal");
  if ("IntersectionObserver" in window) {
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          entry.target.classList.add("visible");
          io.unobserve(entry.target);
        }
      });
    }, { threshold: 0.12 });
    revealEls.forEach(function (el) { io.observe(el); });
  } else {
    revealEls.forEach(function (el) { el.classList.add("visible"); });
  }
})();
