# DSH Launcher — 实现与运维说明

本文档记录不适合放进用户向 `README.zh.md` 的实现与运维细节，作为启动器内部行为的参考。

## 产品范围（详细）

- macOS arm64 与 x64 DMG 安装包
- Windows x64 按用户安装的 NSIS 安装包；不再发布便携 ZIP
- 固定 Node.js 24.19.0，只有平台专属 SHA-256 匹配的归档才能进入运行环境
- 精确安装 `@deepseek-ai/dsh`；由第一个 npm registry 确定版本，并以 npm 实际使用的完整版本索引确认可安装性，安装时直接使用该版本已验证的 tarball，避免再次读取陈旧的包版本索引，后续保持成功来源以复用缓存
- 有有效旧版本且磁盘余量充足时复制到 staging 复用依赖，并刷新隐藏 lockfile 供 npm 采用，再执行可执行 smoke 校验、原子发布、启动恢复与失败回滚；复制失败会退回干净候选目录，且不修改当前运行版本
- Harness 安装实时展示依赖解析、依赖包获取、运行环境写入、验证与启用阶段；npm 长时间无输出时明确说明它可能仍在计算依赖，不会把无日志的计算阶段误判为卡死
- 稳定的终端 `dsh` 命令，由启动器私有运行环境提供；Harness 安装成功后，应用会把自有 `bin` 目录追加到 macOS 登录与交互式 Shell 配置或 Windows 用户 PATH，不会替换无关的同名命令入口
- 浏览器选择、系统托盘生命周期、中英双语、浅色/深色/跟随系统主题
- 插件市场：通过 `market.dsdesktop.com` 每日验证快照消费 [dsh-market](https://github.com/2BingLing/dsh-market) 的 `plugins.json`（Rust 拉取、哈希校验、内容校验与本地缓存，CSP 下不经 Webview 直连），支持中文搜索、类型筛选、排序与分页；cordis 插件通过启动器锁定的 pnpm 在指定候选 profile 中安装，skill 插件通过校验后的 GitHub tarball 解包进 `dsh-home/skills`，卸载会备份可恢复；安装前按 cordis peerDependencies 给出兼容/不兼容/未知三态提示。启动输出只会向恢复器暴露经过严格校验的软件包名；若命中已安装的 profile 依赖，会通过事务卸载并只重试一次。只有重试成功才会提交卸载并弹窗告知用户；重试仍失败时会恢复完整 profile 批次。
- Harness 更新与带密码学签名的桌面应用更新相互独立；Harness 检查会跟随设置中持久化选择的 npm `latest`（默认）或 `alpha` 通道。Harness 可选择前台更新并显示进度，也可在当前服务继续运行时于后台准备经过校验的候选版本。后台准备完成后由用户确认切换；若此时退出应用，下次启动会自动切换。切换后若服务启动失败，页面会把经过校验且仍保留的上一版显示为绑定确切版本的回滚操作；回滚只交换当前与上一版目录，不删除任一侧，并在更新版本标记前验证恢复后的运行时。
- 只有经过净化的服务输出明确识别到 `session_projcache` schema 错误或点名不兼容第三方包时，页面才提供启动修复。启动器先确保托管服务完全停止，写入修复清单，把两个受支持的确切缓存布局（`storages/session_projcache.json` 与 `storages/session_projcache/`）移入私有备份，并真实恢复一次完成恢复演练，然后再隔离它们用于重试。会话日志、`workspace.json`、设置、凭据、附件和其他 storage domain 全都不在修改范围内。加载器点名的插件会加入市场回滚批次。Web 服务成功发布地址后才提交 Web 插件批次并把旧缓存备份标记为已验证；重试失败则保留新生成的缓存证据，并恢复原缓存和 profile。
- 已完成的启动修复备份只会在托管服务成功发布健康地址后执行回收：保留 7 天、最多保留最近 3 份已验证修复，全部已完成修复备份合计最多 512 MiB，超限时从最旧项开始删除。扫描器只接受 `backups/startup-repair-*` 下具有可读完成态清单、且整棵树仅包含普通文件和目录的直接子目录；未完成、清单损坏、特殊文件或包含符号链接的条目会显示为受保护，绝不自动删除。设置页展示可清理备份数量、容量、最早到期时间和受保护数量；手动清理复用同一安全校验，不会触及迁移备份或备份根目录外的路径。
- 远程访问：侧边栏独立页面，提供总开关、局域网/公网独立开关、二维码与可刷新的 8 位连接密码。`dsh-core` 内置自研认证反代（HTTP + WebSocket 透传、按 IP 与全局的登录限速、内存会话），为仅监听回环地址的 Harness 界面守门；公网访问由托管的 cloudflared 快速隧道提供，二进制按固定 SHA-256 校验后才允许落地

## 远程访问

「远程」页（侧边栏 → 远程）把只监听 127.0.0.1 的 Harness 界面，经由启动器自带的认证反代暴露给操作者自己的手机；Harness 服务本身不做任何配置改动。一个总开关统管两个相互独立的作用域：

- **局域网**：确认本机存在可用的非回环 IPv4 路由后（网线与 Wi-Fi 均支持），才监听本机全部 IPv4 接口；二维码与地址栏展示主局域网地址，并配以 8 位连接密码。同一局域网内的手机扫码、输入一次密码后，在启动器进程存活期间保持会话。没有可用地址时不会启动局域网监听，界面也会禁用开启操作。应用处于前台期间会在窗口聚焦时及低频定时刷新中重新协调路由与监听器，因此插拔网线或切换 Wi-Fi 后无需反复开关远程访问即可更新可用状态。
- **公网**：仅监听回环地址的代理，前置一个托管的 cloudflared 快速隧道（固定版本、按 SHA-256 校验后落地、无 shell 启动并套用启动器代理策略，Windows 下无控制台窗口）。开启时后端强制要求确认安全提示，无法绕过；随机分配的 `*.trycloudflare.com` 域名每次启动都会变化，旧链接随隧道进程一起失效。

两个作用域共用同一套代理设计：只解析请求头；登录成功后签发不透明的 HttpOnly 会话 Cookie（仅保存在内存，启动器重启即全部失效）；同一 IP 连续输错 5 次密码锁定 60 秒，另有多来源集中失败的全局锁定兜底；认证之后的所有字节——表单提交、流式响应、WebSocket 帧——都走原始双向管道透传。Harness 0.1.2-alpha.2 及更高版本会打印私有的 `/?token=...` 启动地址，用每次进程启动生成的令牌换取 Harness 自己的浏览器 Cookie；代理只在内存中保留这个完整目标，并在启动器密码校验通过后，按远程会话和 Harness 启动代次仅通过回环链路完成一次交换。令牌不会进入局域网/公网地址、二维码或远程重定向；旧版裸根地址则跳过交换。刷新密码会立即吊销该作用域的全部会话并断开其现有连接。完整上游端点按连接实时解析，因此 Harness 重启或更新后可在需要时为已有启动器会话重新握手，不必替换远程监听器或已建立的隧道。远程密码保存在桌面应用自有的 `remote/` 目录，绝不写入 DSH_HOME。

登录防暴力破解完全在内存中进行：同一地址 5 次失败后锁定 60 秒（成功登录即清除记录），60 秒内多地址累计 30 次失败则触发一次短暂的全局锁定。

## 代理支持

「设置 → 代理」控制启动器自身的全部联网行为——Harness registry 版本查询、tarball 与 Node.js 下载、发布来源检查、插件市场的 catalog/registry/GitHub 客户端、会联网的 npm/pnpm/Harness 子进程，以及桌面更新的检查与下载，都使用同一份配置（Tauri 更新器通过其 `configure_client` 钩子适配，检查和下载使用同一份代理计划）。三种互斥模式：

- **跟随系统**（默认值，对代理功能之前写入的旧配置同样生效）：使用代理环境变量（`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` 及其小写形式，大小写间取第一个有效值——传统的无协议 `host:port` 按 HTTP 代理处理），并在 reqwest 提供系统代理读取能力的平台合并操作系统代理。macOS 下 HTTP 客户端与更新器会合并环境变量和 OS 系统代理；Linux 下 reqwest 的系统模式由环境变量驱动。子进程只会得到环境变量推导出的代理变量（OS 系统代理没有变量形式，不会导出给子进程）。Windows 下系统代理匹配器无法处理注册表中的逐协议条目，启动器会为客户端、更新器和子进程解析一份合并的逐协议代理计划：每个协议优先使用对应环境变量，其次是 `ALL_PROXY`/`all_proxy`；只有环境未覆盖的协议，才由当前用户 Internet Settings（`ProxyEnable`、`ProxyServer`、`ProxyOverride`，严格只读）的对应项补齐，并把单地址或 `http=...;https=...;socks=...` 形式转换为 npm/pnpm 可用的变量，将 `<local>` 展开为明确的回环绕过项（面向 npm/pnpm 的近似处理，并非完整复刻 WinInet 的本地域名语义）。`NO_PROXY`/`no_proxy` 优先于 `ProxyOverride`。CGI 环境（设置了 `REQUEST_METHOD`）下不信任任何代理来源。启动器不会修改注册表或系统代理。
- **直连**：始终直接连接，忽略系统代理与所有代理环境变量；子进程得到不含代理变量的环境。
- **手动**：所有启动器流量、更新器（检查与下载）和子进程使用同一个代理 URL（支持 `http`、`https`、`socks5`、`socks5h`，仅允许 `scheme://host[:port]`），可另配绕过列表（NO_PROXY，支持域名、IP 及 IPv4/IPv6 CIDR 段）。IE 风格的 `*.domain` 条目会被规范化为 reqwest、curl、npm 都能理解的等价前导点域名规则；其他通配符形式会被丢弃，不会传给 reqwest。

保存后，启动器请求、「重试」和新启动的子进程立即使用新设置。已经运行的进程无法原地修改环境变量，因此 Harness 正在运行时，设置页会明确提供重启操作，不会擅自中断当前会话。「测试连接」使用表单中当前填写的内容（无论是否已保存）请求 Harness 的两个 registry，逐一报告来源结果，失败时给出分类且脱敏的网络错误（超时、代理要求认证——包括 CONNECT 隧道 407——、TLS/证书、连接/DNS、HTTP 状态）。本版本拒绝并永不保存代理 URL 中的用户名和密码；跟随系统/直连模式还会清除未启用的手动字段，诊断信息也不会回显 URL userinfo。PAC/WPAD 自动配置与 NTLM/Kerberos 集成代理认证暂不支持。

## 桌面宠物

桌面宠物是 DSH Launcher 的一等功能，不是需要用户安装的 Harness 插件。功能按六层完整落地：

- **M0 — 状态契约：**单一 reducer 把顶层 Harness 会话事件归并为且仅归并为五种公开状态（`waiting`、`error`、`working`、`thinking`、`idle`）。询问用户或请求审批进入等待，只有 reasoning 流片段进入思考，其余活跃阶段（包括模型正文输出、工具调用生成、命令执行和查找）均进入工作，失败进入错误，正常完成或中止进入空闲。多个会话并行时优先级为等待 → 错误 → 工作 → 思考 → 空闲；子 Agent 不会覆盖顶层状态。
- **M1 — Harness bridge：**自包含的 `pet-bridge.mjs` 与既有余额桥一起暂存，并通过自动生成的 `dsh web --patch` overlay 注入。桥只通过随机令牌保护的回环 SSE 端点发布长度受限、经过净化的活动元数据；令牌只留在子进程环境中，不进入 URL 或前端 payload。
- **M2 — 桌面服务：**`dsh-core::pet` 严格解析带版本的 snapshot，拒绝未知字段、超限文本和非法进度，通过有上限的退避自动重连，并用类型化 Tauri 命令和 `pet://state` 发布 connected/stale/unavailable 连接健康度。如果组合 overlay 启动失败，会先用仅余额桥的 overlay 重试一次，再降级到不打补丁的 Harness，确保可选宠物永远不会阻断工作台启动。
- **M3 — 宠物窗口：**Tauri 创建独立的透明、无边框、始终置顶窗口。React 渲染器按需加载 Lottie，并使用单槽最新状态队列：实时动画至少完整播放一轮，相同或已经过时的待播状态会合并，只在轮次边界切换，没有新状态时原动画无重载循环；减弱动态和手动预览仍即时更新。动画、气泡、CSS 状态和无障碍文案始终同步。用户可拖动整个窗口，物理屏幕坐标会持久化，恢复时会限制在当前可用显示器范围内，Harness 未就绪时窗口自动隐藏。鼠标穿透作用于整个窗口，并始终可在主应用的「桌面宠物」页关闭。
- **M4 — 产品控制：**功能注册表拥有侧边栏入口与路由。页面可以选择目录中的宠物、独立预览五态而不改写实时状态、控制显隐/气泡/尺寸/减弱动态/鼠标穿透，并显示 bridge 健康度。偏好原子保存到 `preferences.json`；旧版配置加载后默认关闭宠物。托盘菜单同步提供显隐操作。
- **M5 — 目录与验证：**内置资源统一位于仓库根目录 `pets/`。`pets/config.json` 声明 `count`，并为每个实体提供中英双语昵称、物种名、多个标签、简介、可选的逐状态气泡文字、资源文件夹及五个动画文件。缺少气泡配置时回退到 Launcher 的双语默认文案。目录、reducer、事件序号重置、严格 payload、偏好、IPC 生成、lint、构建和 Rust 测试共同覆盖该功能。

目录包含 Gru 提供的土拨鼠「麻薯」（`marmot`，仍为默认宠物）和橘猫「橘子」（`orange-cat`）。运行时打包仅纳入每只宠物的五个 Lottie JSON 及其引用的 PNG 分层，不包含生成器、QA 结果或预览工具。橘子原样使用提供的 v4 五态资源包，保留资源说明中已知的等待动画尾巴接缝，气泡使用双语默认文案。目录测试逐一解析每只宠物各状态的图片资源，检查缺失分层。这些视觉素材不适用仓库的 MIT 许可，只能依据 `pets/ASSET-LICENSE.md` 用于及再分发于非商业用途；商业使用必须事先取得 Gru 的书面许可。

## 架构

```text
React 功能注册表 + HashRouter
  └─ 类型化 launcher API / 带 revision 的状态事件
      └─ Tauri 命令与生命周期适配层
          └─ dsh-core 应用服务
              ├─ 运行环境部署与回滚
              ├─ source/CC Switch 导入
              ├─ 托管 dsh web 进程树
              ├─ 插件市场（目录缓存、查询、安装/卸载、已装检测、兼容性检查）
              ├─ 远程访问代理与 cloudflared 隧道
              ├─ 桌面宠物 reducer、回环 SSE 客户端与偏好
              └─ 浏览器与偏好设置端口
                  └─ 固定 Node.js → 已发布 @deepseek-ai/dsh
```

功能注册表统一拥有路由和导航元数据。后续增加页面时只需增加 feature descriptor 和对应后端模块，不必继续扩大单个全局 View。业务规则全部位于不依赖 Tauri 的 `dsh-core`；Tauri 只负责操作系统生命周期、托盘、剪贴板、更新器和类型化 IPC。命令与事件按模块命名，前端只接受 revision 单调递增的新状态。Windows 构建使用 GUI 子系统，所有辅助进程均以无控制台窗口方式启动。系统托盘可用时，关闭主窗口只会隐藏界面，正常退出通过托盘菜单完成。托盘初始化失败不会阻塞启动；缺失应用图标也会沿用同一可失败路径处理，而不会触发 panic。在这一降级模式下，关闭主窗口会完成清理并彻底退出。如果初始后台启动线程无法创建，桌面壳会继续打开并进入可重试的失败状态；前端预热 IPC 被拒绝时也会消费该 Promise 拒绝，同时由 React 错误边界展示致命界面，避免形成未处理启动错误。部署进程树与本地服务进程树均已停止、回环服务端口也已关闭后，才会放行彻底退出。每个桌面数据目录持有唯一实例锁，避免多个启动器并发运行，并会短暂等待正在更新的旧实例释放锁。macOS 和 Windows 启动服务前都会核验可执行文件路径、命令参数和用户归属，并回收该私有运行时遗留的全部旧服务；macOS 还会核验进程组归属。服务由父进程管道守护，启动器被强制终止后，守护进程会结束服务并随即退出。Unix 守护进程终止独立进程组，Windows 守护进程则结合内外两层关闭即终止的 Job Object 与父进程管道兜底清理。

项目有意不为启动器自身引入运行时插件系统、通用工作流引擎或前后端重复状态机。这些抽象对当前启动器没有收益，只会增加未来修改成本。插件市场管理的是 Harness 自己的插件生态，通过锁定的 Harness CLI 与 npm/pnpm 层操作；应用适配层协调托管服务生效，核心层负责包事务。

## 数据兼容与安全

Rust 应用保持原有磁盘协议：

```text
~/.dsh-desktop/
├── runtime/{node,dsh,runtime.version,.deployment.lock}
├── cache/
├── dsh-home/
├── bin/dsh[.cmd]
├── server.log
├── install.log
├── server.pid
├── language
├── preferences.json
├── backups/migration-*/dsh-home
├── .migration-complete-v1
└── .migration-skip-v1
```

Harness 首次安装成功后，请打开一个新终端再运行 `dsh --version`。macOS 的登录与交互式 Shell 配置放在带明确标记的托管区块内，Windows 会广播环境变量变更。显式设置 `DSH_DESKTOP_HOME` 时仍会在该隔离目录中创建命令包装器，但会主动跳过用户 PATH 或 Shell 配置修改，确保开发与测试目录保持隔离。

显式 `DSH_HOME` 会关闭所有导入。否则启动器只会在 `DSH_DESKTOP_SOURCE_HOME`（默认 `~/.dsh`）中发现兼容数据，并在复制任何内容前要求用户选择。确认导入后会创建并校验私有备份、完成恢复演练、在活动目录之外构建完整结果，再通过可从崩溃恢复的原子事务发布；选择跳过会被持久化，并在不导入来源数据的情况下使用现有隔离启动器目录启动。已有目标值和已填充的 workspace ledger 始终优先。

CC Switch 只是可选的只读来源。导入器以只读方式打开 `cc-switch.db`，只接受具有字面凭据、非回环 HTTP(S) 地址、受支持协议且至少包含一个模型的独立 Claude provider；OAuth、托管账号、依赖代理和含义不明确的记录全部跳过。只有在能可靠理解既有文档结构时才补充缺失值。凭据只进入 `.credentials.yaml`，永不进入 settings 或日志。双文件发布失败时会恢复为完全一致的原始字节。如果 Windows 权限或其他本地 I/O 问题导致这项可选导入失败，启动器会提示已跳过，并继续安装和启动 Harness。

测试、检查、构建和打包必须设置临时的 `DSH_DESKTOP_HOME`、`DSH_HOME`、`DSH_DESKTOP_SOURCE_HOME` 与 `DSH_DESKTOP_CC_SWITCH_HOME`。不得接触真实用户目录、Keychain、凭据存储或生产数据。

Harness 更新继续复用私有 npm 下载缓存，但会在安装前后检查 `cache/npm`，达到 1 GiB 就立即清理。旧版 Node 归档及中断的归档下载也会清退，仅保留当前已校验 Node 归档供复用。`install.log` 与 `server.log` 各自限制为 16 MiB。这些策略不会触碰 `dsh-home`、配置、会话或凭据。运行环境平时只保留当前版本和一个上一版本用于回滚；后台更新准备完成、尚未切换时会额外保留一个隔离且已校验的候选版本。

插件市场数据只读消费 dsh-market 目录，缓存于 `cache/marketplace` 并按 `generatedAt` 每日刷新。北京时间每天 07:00，公开仓库工作流解析不可变的 GitHub commit，验证其历史仍继承自内置信任锚点，校验目录后发布为受限双槽 R2 快照。客户端只访问 `market.dsdesktop.com`，核验清单大小和 SHA-256，刷新失败时保留最后一次验证成功的缓存；不安全条目会被逐条隔离。安装与卸载全部落在隔离的 `dsh-home`：Cordis 变更先在完整候选 profile 中构建并校验，再以目录级事务发布；skill 卸载只把用户明确选择的目录移入 `cache/marketplace/trash`。安装前会即时解析并锁定确切 npm 版本或 Skill 提交，校验 npm `repository` 和目录来源字段与所展示 GitHub ID 的绑定，并展示来源、目标、版本、绑定状态和执行风险；卸载只影响明确选中的 profile 或技能副本。

市场会从受支持的 `dsh plugin … add …` 命令解析完整包列表及 `--profile`（`pnpm add …` 默认使用 `web`）。README 中的不同命令仍视为替代方案：优先选择首条包含当前目录插件名的命令，否则选择首条有效命令。本地路径、可移动的 Git 引用、Shell 命令链、未知选项及不安全的 profile 名称会被拒绝，不会只安装其中一部分。registry 包会校验 npm 仓库来源并固定到精确版本。`github:owner/repo` Cordis 来源只有与市场仓库一致时才会继续：预检把默认分支解析为不可变 commit，限量读取 `package.json`，要求合法包名、精确 semver 和安全的 `dsh.bundle.patch`，确认限量 patch 文件存在，再向 pnpm 传递带明确包名和 commit 的安装参数。对于没有安装命令的市场记录，npm 查询失败后也只进行同样受校验的 GitHub 回退，不再把展示名称盲猜成 npm 包。每个来源还会校验 Cordis peer；确认凭据绑定完整解析来源列表及 profile。共用的 `@deepseek-ai/dsh-base` 和 `@deepseek-ai/dsh-web-app` 绑定到 `deepseek-ai/deepseek-harness`。缺少或不匹配的仓库声明可以作为提醒确认，但来源不可用或仓库不具备合法 DSH bundle 时仍会被阻止。预检会直接列出全部来源提醒和兼容性问题；只有所有构件都解析为不可变目标后，用户才可明确确认强制安装。生命周期脚本禁用、候选组合验证、Git commit 精确复核及事务回滚同样适用于强制安装。

Skill 技能包选择兼容根目录 `SKILL.md`、安全的 `skill.json.entrypoint`，以及全仓库唯一可定位的嵌套 `SKILL.md`；包含多个技能且未声明入口的仓库会被拒绝。发布后的技能目录带有启动器自有元数据，绑定目录 ID、不可变 commit 与安装时核对过的依赖步骤。目录中的依赖命令及结构化 `skill.json.runtime.pythonPackage` 声明会与技能文件安装分开展示，所有命令都可一键复制。直接执行必须二次明确确认，后端按已保存摘要查找步骤，不接受 Webview 传入任意命令文本；仅允许解析后的 `pip install`、`python -m pip install` 和 `npm install` 参数，完全不经过 Shell，并拒绝管道、重定向、命令串联、全局/指定前缀目标和逃出技能目录的依赖文件路径。子进程固定在已安装技能目录中，以启动器受限环境运行，并限制超时与输出。其他脚本和系统包命令只允许复制，并提供项目文档入口。

整组包通过一次禁用生命周期脚本的 pnpm 调用安装，新 bundle 层保留命令中的顺序，已有依赖和基础层保持不变。缺失的 profile 只在候选目录按已发布 Harness 模板初始化（自定义 profile 包含 `@deepseek-ai/dsh-base`、空 patch 和 hoisted pnpm 配置）。持久化的“原先不存在”标记让发布前失败能够撤回新建 profile，同时保留诊断证据。来源和兼容性正常的条目从卡片直接一键安装；存在真实风险时才显示确认框及全部包和目标 profile。解析器保留来源安装配方，桌面安装则统一接入托管的 `web` profile，复用其网页基础层，让插件出现在实际 Harness 页面中。已有独立 profile 不会被迁移或改写；卡片优先展示完整的 Web 安装，旧版独立副本提供“修正”操作以安装到 Harness。包版本、bundle 顺序及已有依赖仍受保护。profile 内的安装记录保存每个市场插件的包组，以及由市场新增的直接依赖。桌面卸载还会处理旧 profile 中登记为同一市场插件 ID 的副本，先处理非活动副本，最后处理 web。每个 profile 保留独立恢复事务；后续副本失败时报告卸载未完成，不返回全部成功。对于未登记的旧包，不推断明确选择位置之外的所有权。即使市场目录随后变更，一键卸载也按原记录对每个包组进行事务处理；共享包、仍被依赖的包及原先已存在的包会保留。此前由市场新增而暂时保留的依赖，会在最后一个依赖组卸载时清退。没有安装记录的旧版安装继续按明确选择的单包卸载，不推测所有权。配置与用户数据保留；卸载不清空包管理器下载缓存。

每个 profile 的持久化事务记录关联候选发布、备份及待验证日志。发布中断时恢复原 profile 和日志，并持久化撤回完成标记，确保候选清理再次中断后仍可重复恢复。候选校验覆盖整组卸载依赖、辅助非 bundle 包、已有包的实际版本，以及候选准备期间用户对配置文件的修改；随后通过启动器锁定的 Harness 对候选执行无启动配置组合检查。配置检查失败时不发布。后端在候选准备、发布、服务重启、直连 loopback HTTP 检查成功 HTML 页面及提交的全过程持有同一操作锁。启动或页面检查失败时，先停止候选服务，恢复上次可用 Web profile，再恢复启动后返回错误；停止失败则保留日志和快照。配置 dump 本身不能提交变更或删除备份。Harness 空闲时，新安装与 Web 卸载在后台完成生效后返回成功；有工作或状态未知时只完成包变更，等待用户手动重启。网页沿用 Harness 原有的启动时自动打开行为，市场生效和手动重启均不再额外发起浏览器打开请求；HTML 检查使用的临时 Cookie 仅保存在内存，只允许同源跳转。卸载旧版非活动独立副本时保留隐藏的 `.PROFILE.market-retained-*` 备份，不启动其中的独立任务；旧版自定义待验证日志继续保留，页面挂载不再自动接受。强制确认会关闭严格 peer 检查，继续禁用生命周期脚本；新增直接依赖保存为精确版本。

市场自动生效在候选准备前和即将重启前，直连带鉴权的 loopback 工作状态端点。仅确认当前 Harness 空闲时才自动重启；执行、思考、等待输入、错误或无法获取状态时延后生效。已有 Web 待生效批次也会使后续变更继续等待。延后时安装与卸载返回成功及 `restartRequired`，保留待验证日志和回滚快照，不停止当前进程或打开浏览器。市场页面持续显示待重启提示，用户手动重启后先验证网页可用，再提交批次；不会在任务结束后自行安排重启。

可运行 `python3 scripts/test-marketplace-real-pnpm.py` 复现真实 pnpm 生命周期检查。脚本将 pnpm 10.12.3 下载到临时目录并校验完整性，以本机 loopback 仓库提供合成包，验证整组安装、候选失败隔离、强制安装、完整卸载、回滚及脚本禁用。运行环境目录、npm 配置、存储和测试包均与生产数据隔离。此检查仅适用于 Unix；文件系统中断测试覆盖发布阶段，不代表硬件故障或所有 Windows 文件占用情形。

## 开发与官网

依赖：Node.js 24+、pnpm 10.12.3、Rust 1.96。

`pnpm bindings` 从 Rust 领域类型生成 [bindings.ts](../src/platform/generated/bindings.ts)。生成结果需要提交，并在本机确认重新生成后没有差异。`pnpm deadcode` 约束前端依赖边界，严格 Clippy 对 Rust 执行同类约束。

仓库根目录的 `public/` 是独立官网。Vite 配置了 `publicDir: false`，官网与桌面资源不会意外混入彼此。官网代码继续使用原生 HTML/CSS/JavaScript。Cloudflare Workers Builds 应将根目录设为 `public`、构建命令留空、部署命令设为 `npx wrangler deploy`。Worker 会在 `/latest.json` 代理已发布的 GitHub 更新清单；客户端优先请求此端点，失败后直接回退 GitHub。更新包及其强制签名仍是 GitHub Release 产物。独立的 Standard 类 R2 桶 `dsh-launcher-marketplace` 通过 `market.dsdesktop.com` 只读公开，关闭 `r2.dev` 并为 JSON 配置缓存规则。市场发布通过 S3 兼容 API 使用桶级最小权限的 `R2_ACCESS_KEY_ID`、`R2_SECRET_ACCESS_KEY` 与 `CLOUDFLARE_ACCOUNT_ID`；固定的 `latest.json`、`catalog-a.json`、`catalog-b.json` 控制存储增长并保持在预计免费额度内。`pnpm cloudflare:check` 会守护这些约定与全部本地资源，且不改变现有自动部署路径。

配置市场发布通道时，创建 Standard 存储类的上述桶，只连接 `market.dsdesktop.com` 自定义域名并关闭 `r2.dev`，再为 `/v1/*.json` 添加 Cache Everything 规则且让完整查询参数参与缓存键。设置较低的 R2 预算提醒，创建只允许该桶 Object Read & Write 的 R2 令牌，将访问密钥、秘密密钥和账户 ID 添加为 Actions secrets，随后手动运行一次 **Publish Plugin Marketplace** 并设置 `bootstrap=true`。后续定时任务通过已认证的 S3 兼容 R2 API 直接读取 `latest.json`，不会使用可能滞后的 CDN 响应。

## 发布与签名

`package.json`、workspace `Cargo.toml` 与 `src-tauri/tauri.conf.json` 的版本必须和 `desktop-v<version>` tag 一致，`pnpm versions` 会强制检查。

即使平台代码签名暂未配置，Tauri 更新签名也必须存在：

1. 创建已被 Git 忽略的本地密钥目录，并使用明确的**文件**路径生成更新密钥对（`-w` 的目标不是目录）：

   ```sh
   mkdir -p signer-keys
   chmod 700 signer-keys
   pnpm tauri signer generate -w signer-keys/dsh-launcher-updater.key
   ```

   未经单独评审的轮换和恢复方案，不得对已有更新密钥使用 `--force`。私钥绝不能提交，并应在仓库外保留经过验证的加密备份。

2. 将私钥和可选密码保存为 GitHub secrets：`TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。
3. 将 `signer-keys/dsh-launcher-updater.key.pub` 的完整内容保存为 GitHub Actions variable：`TAURI_UPDATER_PUBLIC_KEY`。
4. 打 tag 前，在临时的 `DSH_DESKTOP_HOME`、`DSH_HOME`、`DSH_DESKTOP_SOURCE_HOME` 和 `DSH_DESKTOP_CC_SWITCH_HOME` 路径下执行完整本机通用门禁：`pnpm versions`、重新生成 bindings 并检查无差异、`pnpm format:check`、`pnpm lint`、`pnpm test`、`pnpm deadcode`、`pnpm cloudflare:check`、`pnpm build`、`cargo fmt --all -- --check`、`cargo test --workspace --all-targets`、`cargo clippy --workspace --all-targets -- -D warnings` 和 `cargo run --quiet -p dsh-core --example release_check`。
5. 推送 `desktop-v<version>`。CI 只执行发布矩阵需要的原生平台回归测试，Windows 测试留在 Windows 打包 job，macOS 专项测试放在 arm64 Mac job；随后分别构建并签名 macOS arm64/x64 与 Windows x64 隔离产物，矩阵 job 不直接发布。最后由唯一一个 job 生成包含版本和架构的规范资产名，串行创建干净的 GitHub Release 草稿并上传全部文件，逐项核对安装包、更新归档、签名、manifest 条目和官网精确下载链接，全部通过后才正式发布。构建失败或产物不完整时 Release 会保持未发布状态，因此 `releases/latest/download/latest.json` 与已安装客户端不会看到残缺版本。

仓库配置中的 updater 公钥有意留空：本地源码构建不属于生产更新频道。发布 CI 会校验 minisign 公钥格式、写入仅用于本次发布的临时 Tauri 配置，并通过 `--config` 显式交给 CLI；更新信任链任一端缺失都会阻止发布。没有 Developer ID 时，macOS App 会获得完整的 ad-hoc 签名，本地打包与 CI 都会用严格的 `codesign` 校验阻止签名不完整的产物。ad-hoc 签名不等于 Apple 公证：浏览器下载的版本首次启动时仍可能需要用户在 macOS「隐私与安全性」中确认放行；要让任意 Mac 首次启动都不出现身份提示，必须使用 Developer ID Application 证书并完成公证。Windows Authenticode 仍是独立的可选加固，不会降低 Tauri 更新签名的强制要求。
