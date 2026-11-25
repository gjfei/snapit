<p align="center">
  <img src="assets/snapit-icon.png" width="128" alt="SnapIt">
</p>

<h1 align="center">SnapIt</h1>

<p align="center">
  简体中文 | <a href="README.en.md">English</a>
</p>

<p align="center">
  <a href="https://github.com/gjfei/snapit/releases/latest"><img src="https://img.shields.io/github/v/release/gjfei/snapit?sort=semver" alt="Release"></a>
</p>

SnapIt 是支持 macOS 和 Windows 的原生截图工具，用 Rust 和 [gpui-kit](https://github.com/longbridge/gpui-kit)（Zed GPUI）写成，不依赖 Electron 或 WebView。它常驻托盘，没有主窗口或启动器：按下全局快捷键后冻结所有显示器，在选区上完成复制、保存、贴图、文字识别或录制 GIF / APNG / MP4。

<p align="center">
  <img src="assets/screenshots/zh-CN/capture.png" width="820" alt="SnapIt 选区浮层与工具栏">
</p>
<p align="center"><sub>选区浮层与工具栏 · 以下界面预览中的演示数据均为虚构内容</sub></p>

## 功能

- **多显示器区域截图**：截图时冻结全部显示器，每个显示器一个全屏浮层；拖拽选区，或把光标悬停在窗口 / 界面元素上单击选择。
- **多选区与快捷操作**：`F` 选中当前显示器全屏，`C` 复制光标处的颜色（`#RRGGBB`）。按住 `Shift` 进入多选：单击或拖动逐个添加选区，单击已选区域可移除；松开 `Shift` 后同一显示器的选区合成一个整体选区，可继续标注，复制 / 保存 / 贴图使用合成图，各区域保持屏幕上的原始位置，其余部分在 PNG 中透明。
- **自动框选窗口与元素**：高亮光标下的窗口；开启元素框选后会进一步高亮最小的界面控件，滚轮可在控件与上层容器之间切换层级。
- **标注**：矩形、箭头、画笔、马赛克、文字、序号，6 种颜色、3 档线宽；`⌘Z` / `Ctrl+Z` 撤销，可一键清除。
- **水印**：可在独立三栏设计窗口中自由组合多行文字（支持 `{date}` / `{time}` / `{datetime}` / `{year}` 动态变量、加粗、阴影、描边与背景胶囊）和多张不同 PNG / JPEG 图片（支持圆角裁剪），并在透明棋盘格、浅色或深色底图上直接拖动、角点缩放、顶部手柄旋转（`Shift` 15° 吸附）、参考线对齐与撤销重做，也支持拖放文件与剪贴板粘贴图片；提供 5 种起步模板。模板名称、可点击九宫格位置、可拖动预览边距、平铺与间距均在设置页配置，截图、动图、视频可通过分段标签切换独立模板，或切换为统一模板；支持一键创建副本。水印默认关闭，三类输出各有独立开关；OCR 始终识别不含标注和水印的干净画面。
- **复制与保存**：复制为 PNG；另存为 PNG 或 JPEG（可调 JPG 质量），也可在保存对话框里改扩展名。
- **HDR 截图**：显示器开启 HDR 且画面确实超出 SDR 白点时，保存、复制与历史原图保留同一份带 cICP 的 16-bit PNG（BT.2020 / PQ）。单区域截图添加标注或水印后仍在线性 HDR 空间中合成并保留 HDR；选区预览、贴图、OCR、GIF 与历史缩略图使用 SDR 图像。跨显示器多区域拼图仍为 8-bit，JPEG 也不承载 HDR。开启 HDR 录制且系统支持时，MP4 编码为 HDR10（HEVC Main10、PQ / BT.2020），水印不会使其静默降级；编码器不可用时才回退为已合成水印的 SDR / H.264。macOS 需要 15+ 与 Apple Silicon，Windows 需要开启 HDR 的显示器。
- **贴图（Pin）**：把截图钉在屏幕上，可拖动、缩放、调整透明度，支持多张贴图；长截图自动以可滚动与分块渲染的贴图打开。
- **滚动截图**：框选区域后立即支持手动滚轮向上、向下双向拼接，也可用控制条上的向上 / 向下按钮开启或切换自适应自动滚动；持续反向人工滚动会接管并停止自动滚动。自动识别固定吸顶/吸底栏（忽略纯空白区域）与右侧滚动条边栏并完成无漂移中缝拼接。实时预览只请求当前可见范围，不累积瓦片；可在采集过程中直接使用完整标注工具，向上拼接时已有标注保持锚定。结束后可直接复制、保存、贴图、文字识别，或按 `E` 打开长图编辑器做首尾裁剪和继续标注。超过 4096 px 的长图会自动分段 OCR 并在接缝处去重，历史列表对 2:1 以上超长图裁切局部生成缩略图。
- **文字识别**：识别选区文字并尽量自动复制；结果面板可再次复制，识别文字也会写入历史记录。
- **录制视频 / 动图（GIF、APNG）**：选区工具栏和托盘都直接提供「动图 / 视频」两个入口；动图使用设置页中选择的格式（默认 GIF），不再在录制入口重复选择。GIF 适合贴进聊天窗口，使用自适应调色板与有序抖动减少渐变和文字边缘的色带；APNG 使用 `.png` 扩展名保存无损 RGBA 动画，默认无限循环；MP4 走系统硬件编码、体积更小也更流畅。鼠标光标和点击特效默认开启，设置页提供 22 种效果，按基础、动态、粒子和柔光分组，包括菱形描边、四角取景、双轨卫星、雷达扫描、星尘、微型火箭、星云和彩虹光环等；切换分类标签可减少占用空间，点击样式卡片会在点击位置单次预览，播放时仅显示点击效果，停止点击 1.2 秒后恢复静态缩略图。点击特效需要同时开启录制光标，并在相同帧去重前绘制，静止画面也能看到效果。录制由采集线程边采集边编码、流式写入文件；GIF / APNG 使用按物理分辨率调整的有界队列，变化帧先抽样筛查再做精确去重，不降低输出像素尺寸。录制时显示进度控制条，可暂停 / 继续、取消或停止；按快捷键、点「停止」或到达时长上限会结束并保存，按 `Esc` 或点「取消」则丢弃本次录制。
- **托盘与全局快捷键**：默认 `⌥⇧A` 截图、`⌥⇧S` 滚动截图、`⌥⇧O` 识别文字、`⌥⇧G` 录制动图、`⌥⇧V` 录制视频，可在设置中修改，冲突会提示。
- **历史记录**：复制、保存、贴图、识别文字和录制的 GIF / APNG / MP4 都会自动保存一份，按条数自动清理（设为 0 表示不限数量）。
- **权限引导**：macOS 上缺少屏幕录制 / 辅助功能权限时（启动或开始截图时）会打开权限面板，说明用途并跳到系统设置。
- **轻量原生**：单个可执行文件，界面跟随系统浅色 / 深色主题，支持简体中文和 English。

## 与常见截图工具的差异

- **只做托盘应用**：没有主窗口、启动器和常驻大窗口，设置、历史、识别结果都是按需打开的面板。
- **只录选区**：录制（GIF / APNG / MP4）只支持选区，没有摄像头、麦克风和画中画。
- **元素框选依赖系统辅助功能 API**：macOS 与 Windows 均可用。
- **内置更新器**：启动后自动检查 GitHub Releases（可在设置中关闭），托盘「检查更新…」或应用菜单可手动检查；预发布版本也参与检查，下载后校验 SHA-256。Windows 会安装并自动重启，macOS 把校验过的 DMG 存到「下载」并在 Finder 中显示，拖进「应用程序」即可。
- **Windows 提供安装程序**：`setup.exe` 安装到 `Program Files`，自带卸载项，应用内更新会静默调用它；macOS 使用 DMG / zip。

## 下载安装

从 [GitHub Releases](https://github.com/gjfei/snapit/releases/latest) 下载对应平台的文件（`<版本>` 为版本号）：

| 平台 | 文件 |
| --- | --- |
| macOS Apple 芯片 | `SnapIt-<版本>-macos-aarch64.dmg` 或 `.zip` |
| macOS Intel | `SnapIt-<版本>-macos-x86_64.dmg` 或 `.zip` |
| Windows x64 | `SnapIt-<版本>-windows-x86_64-setup.exe` |
| Windows ARM64 | `SnapIt-<版本>-windows-aarch64-setup.exe` |

每个版本还附带 `SHA256SUMS.txt`（所有文件的校验和）和更新清单 `latest.json`。应用会在启动后检查它（包含预发布版本），也可以在托盘菜单或设置 → 通用 → 更新中手动检查；macOS 首次手动覆盖安装仍需按下方 macOS 说明绕过 Gatekeeper。

### macOS

- 要求 macOS 13 或更高版本。
- 安装包默认只做 ad-hoc 签名，没有经过 Apple 公证。第一次打开时请在「应用程序」中**右键点击应用 → 打开**；或者执行：

  ```sh
  xattr -dr com.apple.quarantine "/Applications/SnapIt.app"
  ```

- 请先把应用**移动到「应用程序」文件夹**再运行，否则屏幕录制权限和通知会记在终端或 IDE 上。
- 首次截图前需要在 **系统设置 → 隐私与安全性 → 屏幕与系统音频录制** 中勾选 **SnapIt**；要使用元素框选，还需要在 **辅助功能** 中勾选。授权屏幕录制后可能需要重新启动 SnapIt。
- 从终端或 IDE 里 `cargo run -p snapit` 时，权限属于对应的终端 / IDE（Terminal、iTerm、Cursor 等），而不是 SnapIt。

### Windows

- 运行 `SnapIt-<版本>-windows-<arch>-setup.exe`，安装到 `Program Files\SnapIt`（需要管理员权限），默认创建开始菜单快捷方式，可选桌面快捷方式。安装目录可在向导中修改，下次安装会沿用上次的目录。
- 应用内更新会自动下载并运行新版本的 `setup.exe`（静默安装，需要管理员权限）；也可以手动运行它覆盖安装。配置和历史记录会保留。若 SnapIt 正在运行，交互安装 / 卸载会先询问；应用内静默更新则直接结束进程再继续——配置和历史都是即时写入的。
- 程序没有代码签名，SmartScreen 可能提示「Windows 已保护你的电脑」，点击「更多信息 → 仍要运行」即可。
- 截图与元素框选不需要额外系统权限；元素框选使用 Windows 辅助功能 API。
- 卸载：设置 → 应用 → 已安装的应用 → SnapIt，或运行 `Program Files\SnapIt\uninstall.exe`。卸载会保留配置和历史记录。

## 使用说明

### 托盘与全局快捷键

启动后只在托盘（macOS 菜单栏）显示图标，托盘菜单依次是：截图、滚动截图、动图、视频、识别文字、钉住上次截图、历史记录、设置、退出。动图格式在设置页配置，默认 GIF。关闭设置 / 历史 / 识别结果 / 贴图 / 截图浮层都不会退出应用。macOS 上平时不显示 Dock 图标，打开设置、历史、识别结果面板或长图编辑器时才会临时显示。

默认全局快捷键：

| 操作 | 默认快捷键 |
| --- | --- |
| 区域截图 | `⌥⇧A`（Windows：`Alt+Shift+A`） |
| 滚动截图 | `⌥⇧S`（`Alt+Shift+S`） |
| 识别文字 | `⌥⇧O`（`Alt+Shift+O`） |
| 录制动图 | `⌥⇧G`（`Alt+Shift+G`） |
| 录制视频 | `⌥⇧V`（`Alt+Shift+V`） |

可在 **设置 → 快捷键** 中点击后按下新组合键；`Esc` 放弃录入，`Backspace` / `⌫` 清除（禁用）。不支持的组合、与其他动作冲突或被其他应用占用时会显示错误。录制或滚动截图期间按任一全局快捷键会先结束当前任务（滚动截图会自动完成并复制），若按下的是其他动作的快捷键则在完成后接着执行对应动作。

同一数据目录只允许运行一个 SnapIt，避免重复托盘图标和全局快捷键冲突。再次启动时若不带参数会直接退出；带 `--capture` / `--settings` / `--history` 时，请求会通过数据目录里的 `snapit.request` 转交给已运行的实例执行（约 50 ms 内响应），不会静默丢失。开机自启的 `--background` 属于无参数启动，不会重复打开窗口。

### 截图

1. 按下截图快捷键，或从托盘选择「截图」。所有显示器会立即冻结，并在每个显示器上打开一个浮层。
2. 拖动鼠标选择区域；松开后可拖动边缘调整大小。
3. 用 `Enter` 或双击选区复制，或点击工具栏上的按钮。`Esc` 或右键取消；已有选区时右键先清除选区。
4. `⌘C` / `Ctrl+C` 复制，`⌘S` / `Ctrl+S` 保存。复制到剪贴板的内容始终是 PNG。
5. `F` 直接选中当前显示器的整个画面；`C` 复制光标下的颜色（`#RRGGBB`，放大镜里也会显示）。
6. 按住 `Shift` 进入多选模式：单击窗口 / 元素或拖动框选可逐个添加，单击已选区域可移除。松开 `Shift` 后同一显示器上的选区会合成一个整体选区，可直接标注、复制、保存、贴图：各区域保持屏幕上的原始位置，其余部分在 PNG 中透明；合成后不能拖边缩放，也不支持录制，再按住 `Shift` 可恢复为多个选区继续增删。跨显示器时改为上下拼接，仍按多个区域处理（不支持标注和录制）。

<p align="center">
  <img src="assets/screenshots/zh-CN/multi-region.png" width="820" alt="Shift 多选区">
</p>
<p align="center"><sub>按住 Shift 逐个收集选区，松开后合成一个整体选区</sub></p>

保存对话框默认文件名是 `SnapIt <日期> <时间>.<扩展名>`，默认目录依次为：设置里指定的保存目录 → `~/Pictures` → `~/Desktop` → 用户主目录。图片格式和 JPG 质量在 **设置 → 截图** 中调整；在保存对话框里把扩展名改成 `.jpg` / `.jpeg` 会按 JPEG 保存。

未打开选区时，悬停会高亮光标下的窗口；开元素框选时高亮最小的界面控件，滚轮向外切换容器，反向滚动再收回。单击高亮区域直接选中它。

### 滚动截图

1. 按下滚动截图快捷键（默认 `⌥⇧S` / `Alt+Shift+S`），或从托盘选择「滚动截图」，或在普通选区工具栏上点击「滚动截图」按钮（支持单显示器单选区；多选区或跨屏拼图下不可用）。
2. 选区浮层立即转为穿透点击的实时边框提示，同时弹出悬浮控制条和实时长图预览（可在控制条或设置中开关）。直接滚动鼠标滚轮或触控板即可向上、向下拼接；点击控制条的向上 / 向下按钮可开启对应方向的自动滚动，自动开启时点击另一方向直接切换，`Space` 开关当前方向。持续反向人工滚动会接管并停止自动滚动；到达当前方向边界或达到最大高度时也会停止。
3. 实时拼接器会自动剔除纯空白区以外的固定吸顶/吸底栏（避免重复出现）、忽略右侧滚动条区域并在重叠中缝择优缝合；若滚动过快无法匹配或遇到低纹理纯白区域，边框会短暂转为醒目的警示色并提示放慢速度，回滚至重叠区域即可自动恢复继续拼接。
4. 展开实时预览后，可在采集过程中直接使用矩形、箭头、直线、画笔、马赛克、文字和序号工具标注，支持撤销、删除和样式调整；向上新增内容不会移动已有标注的内容锚点。控制条可 **重置**、切换 **自动滚动方向**、开关 **实时预览**，或直接结束并 **复制**（`Enter`）、**保存**（`⌘S` / `Ctrl+S`）、**钉住**、**文字识别**；按 `E` 打开 **长图编辑器**，按 `Esc` 或点取消直接丢弃。
5. **长图编辑器**：按字节预算惰性加载可见 GPU 瓦片，支持拖动顶部与底部裁剪手柄截取局部、继续使用完整 7 种标注工具、滚轮缩放及适配宽度 / 100% 缩放，并可直接复制、保存、贴图或识别文字。

### 标注

选中区域后，工具栏左侧出现标注工具：

| 工具 | 说明 |
| --- | --- |
| 矩形 | 线宽 2 / 4 / 8 px |
| 箭头 | 线宽 2 / 4 / 8 px |
| 画笔 | 自由绘制 |
| 马赛克 | 真实像素化，不跟随颜色 |
| 文字 | 点击定位后输入，`Enter` 确认、`Esc` 取消 |
| 序号 | 依次放置 1、2、3… 的圆形编号 |

<p align="center">
  <img src="assets/screenshots/zh-CN/annotations.png" width="820" alt="标注工具与样式">
</p>
<p align="center"><sub>矩形、箭头、画笔、马赛克、文字与序号标注，可选颜色和线宽</sub></p>

除马赛克外都可以选颜色；矩形、箭头、画笔可选线宽。工具栏上还有撤销和清除标注。工具栏可以拖动，拖到想放的位置后就停在那里。标注只画在冻结的截图上：复制、保存和贴图时会把标注烘焙进图片；文字识别和录制使用未标注的选区。水印位于标注之上，复制、保存、贴图和历史原件共享同一份冻结的合成结果。

### 水印

在 **设置 → 水印** 中启用后，从模板下拉框选取已保存的水印，也可点击「新建…」从 5 种起步模板（空白模板、简洁署名、品牌 + 时间戳、胶囊角标、斜向平铺）创建，或点击「编辑…」「创建副本」进入独立三栏设计窗口。弹窗顶部工具栏可自由添加多个多行文字元素（支持 `{date}`、`{time}`、`{datetime}`、`{year}` 变量、加粗、阴影、描边与背景胶囊）和多张不同图片（PNG / JPEG，支持圆角裁剪），控制撤销 / 重做、复制与全局对齐排列；左栏管理元素图层与「整体」开关，中间画布可在透明棋盘格、浅色或深色底图上直接拖动（带 5 px 智能对齐参考线）、四角手柄缩放、顶部手柄旋转（按 `Shift` 按 15° 吸附）、触控板双指捏合或滚轮缩放，并支持拖放图片文件或 `⌘V` / `Ctrl+V` 粘贴剪贴板图片。编辑只作用于草稿，点击「保存模板」后写入配置，取消不会修改原模板。模板名称、可点击九宫格位置、平铺与间距均直接在设置页中配置，未平铺的非居中水印也可直接在设置页预览画布上拖动调整边距。模板名称唯一且可搜索，删除模板后引用它的输出会自动回退。

「应用范围」提供截图、动图（GIF / APNG）和视频（MP4）三个独立开关：截图默认开启，动图和视频默认关闭。关闭总开关会禁用范围控件并显示关闭状态；未选择任何输出类型时会提示先选择。

默认开启「统一水印」，三类输出共用同一个模板；关闭后通过分段标签切换截图、动图和视频分别选取模板（选择「沿用统一水印」可继承共享模板，多输出复用同一模板时还会显示共享提示）。切换统一 / 独立模式不会覆盖各自的模板选择，三个应用范围开关始终互不影响。模板名称、设计器字段和输出类型均纳入设置搜索。修改模板会立即更新设置页与当前截图选区的水印预览；预览外框统一为 16:9，内部按截图、方形动图和 16:9 视频的代表画布等比完整显示，方形画布居中留边。水印配置采用全新模板结构，不迁移旧版内联字段。

开启的动图和视频录制会在编码前烘焙对应类型的水印。水印在光标 / 点击效果之后、精确去重之后合成，并只栅格化一次，因此静止画面不会因为水印反复变化，也不会重复叠加。HDR MP4 直接在线性 HDR 帧上合成，再转换为 BT.2020 / PQ Main10。

导入的图片会校验格式、尺寸和文件大小，并复制到应用数据目录的 `watermarks/` 下，配置只保存受管理的文件名。水印仅在本机处理，不会上传。历史原图与从历史打开的贴图保留创建时的水印像素；之后修改水印设置不会重新合成，再次复制或保存也不会叠加第二次。OCR 识别输入始终是干净 SDR 原图。

### 文字识别

按识别快捷键、从托盘选择「识别文字」，或在选区工具栏点击识别按钮。拖动选区后立刻开始识别，并打开小结果面板：

- 有识别结果时会**自动复制**到剪贴板。
- 结果面板可再次复制文字；没有文字、识别失败或后端不可用时会显示对应提示。
- 识别使用当前选区的原始画面（不含标注和水印）；历史中的图片仍是与复制、保存一致的合成结果。

识别语言在 **设置 → 文字识别** 中选择：自动（简体中文、繁体中文、英文）或指定的简体中文、繁体中文、英文、日文、韩文、法文、德文。语言设置对 macOS Vision 和 Windows.Media.Ocr 生效；ocrs 后端只支持拉丁字母，此设置不改变结果。

| 后端 | 平台 | 说明 |
| --- | --- | --- |
| Apple Vision | macOS | 默认，无需模型 |
| Windows.Media.Ocr | Windows | 默认，需要系统安装对应 OCR 语言包；创建失败时回退到 ocrs |
| [ocrs](https://github.com/robertknight/ocrs) | 回退后端 | 纯 Rust，仅拉丁字母，需要下载模型 |

<p align="center">
  <img src="assets/screenshots/zh-CN/ocr.png" width="820" alt="文字识别结果面板">
</p>
<p align="center"><sub>识别结果面板：有结果时自动复制到剪贴板，可再次复制</sub></p>

### 录制视频 / 动图（GIF / APNG）

1. 按动图或视频快捷键、从托盘选择「动图」或「视频」，或点击选区工具栏对应的动图 / 视频按钮。动图入口统一使用 **设置 → 录制 → 动图格式** 中选择的 GIF 或 APNG，默认 GIF。
2. 拖动选区后立即开始录制：选区四周出现高亮边框（边框落在录制区域之外，不会录进画面），旁边出现控制条，显示已录制时间；设置了时长上限时还会显示上限。控制条上是暂停 / 继续、取消、停止三个图标按钮（鼠标悬停会显示用途），可以拖动，拖到哪就停在哪儿；SnapIt 自己的窗口不会出现在录制画面里，所以拖到选区上也没关系。
3. 按 `Space` 暂停或继续：暂停期间画面不会被写入文件，暂停的时间也不计入录制时长和时长上限，继续后时间轴接着走。按任一全局快捷键、点击停止图标（或按 `Enter`），或到达最长时长都会结束并保存录制；随后控制条显示对应格式的生成状态。用快捷键停止时，该快捷键对应的动作会在编码完成后自动开始。按 `Esc` 或点取消图标则直接丢弃本次录制：不生成文件、不进历史记录、不写剪贴板。
4. 编码完成后文件自动保存到默认保存目录，文件名为 `SnapIt <日期> <时间>.gif`、`.png`（APNG）或 `.mp4`，同时写入历史记录；通知里显示路径、帧数和时长。GIF 和 APNG 会以图片形式自动复制到剪贴板（APNG 使用 PNG 图像数据），MP4 会以文件形式自动复制，结束后直接粘贴即可。

<p align="center">
  <img src="assets/screenshots/zh-CN/recording.png" width="820" alt="录制时的选区边框与控制条">
</p>
<p align="center"><sub>录制边框落在选区之外，控制条可暂停 / 取消 / 停止并可拖动</sub></p>

<p align="center">
  <img src="assets/screenshots/zh-CN/recording-demo.gif" width="820" alt="SnapIt 录制的 GIF 效果演示">
</p>
<p align="center"><sub>GIF 录制效果（演示画面为虚构内容）：逐帧记录选区内的变化</sub></p>

动图格式只在 **设置 → 录制 → 动图格式** 中配置，默认 GIF；快捷键、托盘和选区工具栏共用这一项。录制参数也在该页调整：GIF / APNG 帧率 1–30 fps（默认 12）、MP4 帧率 1–30 fps（默认 30）、MP4 目标码率（Kbps）、最长时长 0–600 秒（默认 0，即不限时长）、是否录制鼠标光标（默认开启）、是否绘制鼠标点击效果（默认开启）、点击样式（基础 / 动态 / 粒子 / 柔光四组共 22 款，包含多色烟花、星芒、彗星和极光光效）和 HDR 录制（默认开启，仅 MP4）。MP4 目标码率填 0 时使用自动值，填写正整数则覆盖自动值；也可在 `config.toml` 的 `[recording]` 中设置 `animation_format`、`click_effect`、`mp4_bitrate_kbps`、`captures_cursor` 和 `click_effects`。该码率仅影响 MP4，对 SDR / H.264 和 HDR / HEVC 都生效，不影响 GIF / APNG。两种容器的帧率分开设置：GIF / APNG 是带动画的容器，低帧率更省体积；MP4 走视频编码器，可以放心用高帧率。录制时会去掉连续相同的帧，并默认按显示器的物理像素输出，与截图分辨率一致，不会在 Retina 屏幕上自动降到逻辑尺寸。点击特效在去重前绘制，静止画面也会通过超时检查生成后续效果帧。

一次录制只产出一个文件，三种格式都会留在历史记录里并存。MP4 走系统硬件编码（macOS 用 AVFoundation，Windows 用 Media Foundation），HDR 会话使用 HEVC Main10，编码器不可用或首帧被拒绝时自动重开为 SDR / H.264，不需要另外安装编码器；SDR 视频明确标记为 BT.709，未设置手动码率时按物理像素数以 0.1 bit/pixel/frame 的目标码率编码，给小文字和细边缘留出余量。GIF 用纯 Rust 编码，任何地方都能直接播放：整段复用同一张 256 色板（从画面抽样训练、按需建立查表），只编码画面里变化的矩形，静止区域沿用上一帧内容，配色变化较大时会自动重建色板并写一帧全画面。APNG 使用现有 PNG 编码器保存无损 RGBA8 帧，复用 GIF 的脏矩形队列，不处置或混合前一帧，默认无限循环，结束时把真实帧数和 CRC 补回 `acTL`。两种动画格式通过 8 帧有界队列边录边写，不会把每一帧都留在内存里。物理像素会明显增加 CPU、内存和文件体积，GIF 也仍受 256 色限制；需要缩小输出时，录制 API 可显式设置 `max_width`。GIF / APNG 保留实际像素尺寸（包括奇数宽高），H.264 与 HEVC 因为 4:2:0 采样要求偶数宽高，所以 MP4 的尺寸最多比选区少 1 像素。

### 贴图

在选区工具栏点击「钉住」、从托盘选择「钉住上次截图」，或在历史记录里选择「钉在屏幕上」。没有本次会话的截图时，「钉住上次截图」会使用历史记录里最新的一张图片。

<p align="center">
  <img src="assets/screenshots/zh-CN/pin.png" width="820" alt="置顶贴图窗口">
</p>
<p align="center"><sub>贴图始终置顶，可拖动、缩放并调整透明度，支持多张</sub></p>

- 拖动贴图移动；普通图片滚轮缩放、长截图开启可滚动贴图后普通滚轮上下浏览，`⌘` / `Ctrl` + 滚轮缩放；按住 `⌥` / `Alt` 滚动调整不透明度（最低 20%）。
- 双击或 `Esc` 关闭；右键菜单可复制、保存、调整不透明度（100% / 80% / 60% / 40%）、缩放、恢复原始大小、关闭。
- `⌘C` / `Ctrl+C` 复制，`⌘S` / `Ctrl+S` 保存，`⌘=` / `⌘-` / `⌘0`（Windows：`Ctrl+=` 等）缩放，`⌘W` / `Ctrl+W` 关闭。
- 贴图始终置顶，不显示 Dock 图标；超过 GPU 纹理高度上限的图片自动分块渲染，高长宽比长图初始高度限制在屏幕 75% 以内并带滚动条。从选区打开时覆盖在原选区位置，从历史记录打开时缩放到不超过屏幕的 60%（长图按宽度适配）并居中。

### 历史记录

复制、保存、贴图、识别文字和录制时都会自动保存一份到历史记录（可在 **设置 → 历史记录** 关闭）。默认最多保留 200 项，超出后删除最早的记录；把「最多保留条数」设为 0 即不限数量。

<p align="center">
  <img src="assets/screenshots/zh-CN/history.png" width="820" alt="历史记录面板">
</p>
<p align="center"><sub>历史面板：筛选、缩略图与复制 / 贴图 / 在文件夹中显示 / 删除</sub></p>

历史面板顶部有「全部 / 截图 / 录制」三个筛选按钮，切换时条数和占用空间一起更新。支持复制图片、钉在屏幕上、在文件夹中显示、删除，以及清空全部记录。**单击条目的缩略图或文字区域**会用系统默认程序打开**原始文件**（PNG 交给看图应用、GIF 交给默认查看器、MP4 交给播放器），应用内不做预览。视频没有「复制图片」按钮，但有「复制文件」：复制的是视频文件本身，可直接粘贴到 Finder 或聊天窗口。GIF / APNG / MP4 在列表和贴图里都用首帧 PNG 缩略图。

### 设置

| 页面 | 选项 |
| --- | --- |
| 通用 | 语言（跟随系统 / 简体中文 / English）、主题（跟随系统 / 浅色 / 深色）、开机时启动 |
| 快捷键 | 截图、滚动截图、识别文字、录制动图、录制视频五个全局快捷键 |
| 截图 | 默认保存目录、图片格式（PNG / JPEG）、JPG 质量、自动框选窗口、自动框选元素、选区外遮罩深度、选区外阴影，以及滚动截图参数（最大高度、自动滚动速度、实时预览） |
| 水印 | 总开关、统一 / 独立模板、三类应用开关、命名模板选取 / 新建 / 编辑 / 删除、设计弹窗、旋转 / 缩放 / 透明度、位置 / 边距 / 平铺 / 间距、图片导入、即时预览 |
| 录制 | 动图格式（GIF / APNG）、GIF / APNG 帧率、MP4 帧率、MP4 目标码率、最长时长、录制光标、点击效果与样式、HDR 录制 |
| 文字识别 | 识别语言 |
| 历史记录 | 是否保存、最多保留条数（0 为不限），以及打开历史面板 |
| 关于 | 配置文件路径、在文件夹中显示 |

<p align="center">
  <img src="assets/screenshots/zh-CN/settings.png" width="820" alt="设置面板">
</p>
<p align="center"><sub>设置面板：通用、快捷键、截图、水印、录制、文字识别、历史记录与关于</sub></p>

开机启动通过 LaunchAgent（macOS）或当前用户注册表 Run 项（Windows）注册，启动时带 `--background` 参数，只显示托盘图标。

## 快捷键

全局快捷键默认值见上一节，可在设置中修改。以下快捷键在对应窗口获得焦点时生效：

| 操作 | macOS | Windows |
| --- | --- | --- |
| 确认选区（复制） | <kbd>Enter</kbd> | <kbd>Enter</kbd> |
| 复制 | <kbd>⌘C</kbd> | <kbd>Ctrl+C</kbd> |
| 保存 | <kbd>⌘S</kbd> | <kbd>Ctrl+S</kbd> |
| 选中当前显示器全屏 | <kbd>F</kbd> | <kbd>F</kbd> |
| 复制光标处颜色 | <kbd>C</kbd> | <kbd>C</kbd> |
| 追加选区（多选） | <kbd>Shift</kbd> + 单击 / 拖动 | <kbd>Shift</kbd> + 单击 / 拖动 |
| 撤销标注 | <kbd>⌘Z</kbd> | <kbd>Ctrl+Z</kbd> |
| 取消截图 / 关闭贴图 | <kbd>Esc</kbd> | <kbd>Esc</kbd> |
| 录制时暂停 / 继续 | <kbd>Space</kbd> | <kbd>Space</kbd> |
| 录制时停止并保存 | <kbd>Enter</kbd> | <kbd>Enter</kbd> |
| 录制时取消并丢弃 | <kbd>Esc</kbd> | <kbd>Esc</kbd> |
| 滚动截图开关当前方向自动滚动 | <kbd>Space</kbd> | <kbd>Space</kbd> |
| 滚动截图完成并复制 / 保存 / 编辑 | <kbd>Enter</kbd> / <kbd>⌘S</kbd> / <kbd>E</kbd> | <kbd>Enter</kbd> / <kbd>Ctrl+S</kbd> / <kbd>E</kbd> |
| 滚动截图取消并丢弃 | <kbd>Esc</kbd> | <kbd>Esc</kbd> |
| 长图编辑器复制 / 保存 / 撤销 | <kbd>⌘C</kbd> / <kbd>⌘S</kbd> / <kbd>⌘Z</kbd> | <kbd>Ctrl+C</kbd> / <kbd>Ctrl+S</kbd> / <kbd>Ctrl+Z</kbd> |
| 长图编辑器放大 / 缩小 / 100% / 关闭 | <kbd>⌘=</kbd> / <kbd>⌘-</kbd> / <kbd>⌘0</kbd> / <kbd>Esc</kbd> | <kbd>Ctrl+=</kbd> / <kbd>Ctrl+-</kbd> / <kbd>Ctrl+0</kbd> / <kbd>Esc</kbd> |
| 关闭贴图 | <kbd>⌘W</kbd> | <kbd>Ctrl+W</kbd> |
| 贴图放大 / 缩小 / 原始大小 | <kbd>⌘=</kbd> / <kbd>⌘-</kbd> / <kbd>⌘0</kbd> | <kbd>Ctrl+=</kbd> / <kbd>Ctrl+-</kbd> / <kbd>Ctrl+0</kbd> |
| 打开设置 | <kbd>⌘,</kbd> | <kbd>Ctrl+,</kbd> |
| 打开历史记录 | <kbd>⌘Y</kbd> | <kbd>Ctrl+Y</kbd> |
| 退出 | <kbd>⌘Q</kbd> | <kbd>Ctrl+Q</kbd> |

## 权限

- **macOS**：截图和录制需要「屏幕录制」权限；元素框选需要「辅助功能」权限。启动时缺少权限会打开权限面板，可以打开对应系统设置或重新检测。屏幕录制是必需的；辅助功能可以跳过，此时悬停只选择整个窗口。授权屏幕录制后可能需要重新启动应用才生效。
- **Windows**：不需要额外权限。元素框选使用系统辅助功能 API；首次运行时 SmartScreen 可能拦截，见「下载安装」。

权限面板只在 macOS 上出现。用 `--background` 开机启动时不会主动弹出权限面板，开始截图时才会提示。

## 数据与配置位置

由 `directories` 的 `ProjectDirs::from("dev", "SnapIt", "SnapIt")` 决定；设置环境变量 `SNAPIT_HOME` 可以把配置和数据放到同一个目录。

| 文件 | macOS | Windows |
| --- | --- | --- |
| 设置 `config.toml` | `~/Library/Application Support/dev.SnapIt.SnapIt/config.toml` | `%APPDATA%\SnapIt\SnapIt\config\config.toml` |
| 历史 `history/index.json` 与截图、录制文件 | `~/Library/Application Support/dev.SnapIt.SnapIt/history/` | `%APPDATA%\SnapIt\SnapIt\data\history\` |
| 图片水印 | `~/Library/Application Support/dev.SnapIt.SnapIt/watermarks/` | `%APPDATA%\SnapIt\SnapIt\data\watermarks\` |

首次运行写入默认 TOML。损坏的配置会备份为 `config.toml.bak` 后恢复默认值。设置修改后立即写回 `config.toml`。例如，将 MP4 目标码率设为 12 Mbps：

```toml
[recording]
mp4_bitrate_kbps = 12000
```

省略该项或将其设为 `0`，都会使用自动码率。

## 从源码构建

### 准备

- 安装 [rustup](https://rustup.rs/)。工具链固定在 `rust-toolchain.toml`（当前为 1.99.1），首次构建会自动下载。
- 本仓库通过 git submodule 引入 `vendor/xcap`（录制需要的区域裁剪、帧率上限、时间戳、光标与 `OutputSize` 物理/逻辑尺寸选项）。克隆时使用 `git clone --recursive`；已克隆的仓库执行 `git submodule update --init`。
- **macOS**：Xcode Command Line Tools（`xcode-select --install`）。
- **Windows**：安装 Visual Studio Build Tools 中的「使用 C++ 的桌面开发」工作负载，包括 MSVC 和 Windows SDK（gpui 用 SDK 中的 `fxc` 编译着色器，打包脚本用资源编译器嵌入图标）。打安装包还需要 [NSIS](https://nsis.sourceforge.io/)（`makensis`）。

### 常用命令

```bash
cargo check --workspace --all-targets   # 类型检查
cargo build                             # 构建 snapit
cargo test --workspace                  # 运行测试
cargo clippy --workspace --all-targets -- -D warnings   # 静态检查，要求零警告
cargo run -p snapit                      # 启动（仅托盘 + 全局快捷键）
cargo run -p snapit -- --capture         # 启动后直接进入截图
cargo run -p snapit -- --settings        # 打开设置窗口
cargo run -p snapit -- --history         # 打开历史记录窗口
cargo run -p snapit -- --background      # 开机启动用（与默认托盘模式相同）
```

各 crate 还带命令行示例：

```bash
cargo run -p snapit-capture  --example capture_once -- /tmp
cargo run -p snapit-ocr      --example ocr_image    -- path/to/image.png
cargo run -p snapit-recorder --example record_gif   -- /tmp/out.gif 3
cargo run -p snapit-recorder --example record_video -- /tmp/out.mp4 3 1280
```

### 架构

```
crates/
├── capture   # snapit-capture：xcap 多显示器截屏、缩放换算、裁剪、编码、窗口与元素枚举
├── recorder  # snapit-recorder：xcap 采集 + gif / APNG / 系统 H.264 / HEVC Main10 编码、相同帧去重、最长时长限制
├── scroll    # snapit-scroll：实时双向滚动拼接、吸顶/吸底与滚动条识别、自动滚轮注入、分块预览
├── ocr       # snapit-ocr：OcrEngine trait + Vision / ocrs / Windows.Media.Ocr 后端 + 长图分段识别
├── store     # snapit-store：TOML 设置、历史索引与文件上限清理、数据目录
└── app       # snapit：gpui-kit 界面、托盘、全局快捷键、选区浮层、标注、长图编辑器、贴图、控制条
```

`crates/app` 的主要模块：`controller.rs`（全局工作流）、`session.rs`（冻结帧与选区）、`overlay.rs`（选区界面与工具栏）、`annotate.rs`（标注模型与烘焙）、`scroll_bar.rs`（滚动控制条与实时预览）、`scroll_editor.rs`（长图裁剪与标注编辑器）、`pin.rs`、`recording_bar.rs`、`tray.rs`、`settings*`、`history*`、`hotkey.rs`、`permissions*`、`appearance.rs`、`autostart.rs`。

## macOS 应用包

`cargo run -p snapit` 直接运行 `snapit` 二进制，屏幕录制权限和通知会记在 Terminal / Cursor 上。本地打包可以把这些权限挂到 **SnapIt.app**（bundle id `dev.SnapIt.SnapIt`，与配置目录一致）：

```bash
./scripts/bundle-macos.sh
# 默认使用已有的 debug 二进制；没有才执行 cargo build -p snapit
# 产物：target/macos/SnapIt.app
# 指定二进制：./scripts/bundle-macos.sh --bin /path/to/snapit
```

1. 打开 `target/macos/SnapIt.app`（不要用 `cargo run` 验证打包后的身份）。
2. 在 **系统设置 → 隐私与安全性 → 屏幕与系统音频录制** 中勾选 **SnapIt**。
3. 通知同样归属 SnapIt.app；未放入 `.app` 时系统通知不可用。

脚本使用 ad-hoc 签名（`codesign --sign - --force --deep`），不需要付费证书。重新编译后再打包会改变代码签名，系统可能把它当成新应用，需要重新勾选屏幕录制权限。

## 打包

入口：`./scripts/bundle.sh macos|windows|all`。Windows 在缺少 Rust target、交叉链接器或 NSIS 时打印 `skip:` 并以状态码 0 退出，**不会**自动安装交叉编译工具。

```bash
./scripts/bundle.sh macos
./scripts/bundle.sh windows
./scripts/bundle.sh all
```

| 平台 | 脚本 | 产物 |
| --- | --- | --- |
| macOS | `scripts/bundle-macos.sh` | `target/macos/SnapIt.app`（CI 用 `--target` 写到 `dist/SnapIt.app`） |
| macOS 安装包 | `scripts/package-macos.sh` | `dist/SnapIt-<版本>-macos-aarch64.dmg` 与 `.zip`（Intel：`macos-x86_64`） |
| Windows | `scripts/bundle-windows.sh` | `dist/SnapIt-<版本>-windows-x86_64-setup.exe`（ARM64：`windows-aarch64`）；暂存目录 `dist/windows/SnapIt/` 含 `SnapIt.exe` |

Windows 安装包用 NSIS（`scripts/windows/snapit.nsi`）编译，默认安装到 `Program Files\SnapIt`，所有用户可用。`makensis` 不在 PATH 时可设置 `MAKENSIS=/path/to/makensis`。未指定 `--target` 时，Windows 上使用 rustc 的 host triple（MSVC 或 GNU 工具链均可）；在其他平台上默认交叉编译 `x86_64-pc-windows-msvc`。

`dist/` 已加入 `.gitignore`。在 macOS 上打 Windows（`x86_64-pc-windows-msvc`）包需要事先装好对应 target **和**链接器（如 `cargo-xwin`）；缺一即跳过。

macOS DMG 在 `.app` 之上用 `hdiutil` 打出带「应用程序」快捷方式的 ULMO（lzma）镜像，并附一份 `ditto` 打出的 zip（不用 create-dmg）：

```bash
./scripts/bundle-macos.sh --release
./scripts/package-macos.sh
# 产物：dist/SnapIt-<版本>-macos-aarch64.dmg 与 .zip
# Intel：./scripts/package-macos.sh --target x86_64-apple-darwin
```

签名 / 公证 / staple 使用与 SwitchHostsLite 相同的环境变量（也是 GitHub Secrets 名）：`MACOS_CERTIFICATE`、`MACOS_CERTIFICATE_PASSWORD`、`MACOS_SIGNING_IDENTITY`、`APPLE_ID`、`APPLE_TEAM_ID`、`APPLE_APP_PASSWORD`。未配置时保持 ad-hoc 签名并照样生成 DMG，**不会失败**。

## 发布

把 `Cargo.toml` 里 `[workspace.package] version` 改成要发布的版本并提交，然后打 **`vX.Y.Z`** 标签推送（必须与版本号一致，例如 `0.1.0` → `v0.1.0`）：

```bash
git tag v0.1.0
git push origin v0.1.0
```

[`.github/workflows/release.yml`](.github/workflows/release.yml) 先核对标签与 `Cargo.toml` 的版本号，再构建 4 个目标：

- macOS arm64、macOS Intel（`macos-latest` 交叉编译，部署目标 13.0）
- Windows x64、Windows ARM64（`windows-latest` 交叉编译）

上传的文件：

| 平台 | 文件 |
| --- | --- |
| macOS Apple 芯片 | `SnapIt-<版本>-macos-aarch64.dmg` 或 `.zip` |
| macOS Intel | `SnapIt-<版本>-macos-x86_64.dmg` 或 `.zip` |
| Windows x64 | `SnapIt-<版本>-windows-x86_64-setup.exe` |
| Windows ARM64 | `SnapIt-<版本>-windows-aarch64-setup.exe` |

发布任务还会生成 `SHA256SUMS.txt` 和更新清单 `latest.json`（`scripts/release-manifest.py`；macOS 的 DMG 会额外写一个 `macos-<架构>-dmg` 键，应用优先下载它），并创建 GitHub Release（更新说明自动生成）。带 `-` 的标签（例如 `v0.2.0-beta.1`）会发布为预发布版本，应用同样能检查到。也可以在 Actions 页面对**已有**标签手动运行 `workflow_dispatch`；不要对未打标签的分支做发布。

`.github/workflows/ci.yml` 在 macOS 和 Windows 上运行 `cargo test --locked` 与 clippy。CI 使用 `rust-toolchain.toml` 固定的 1.99.1，**不要改成 `stable`**。

### 可选：代码签名

`release.yml` 中有注释掉的 Windows 签名步骤，配置 `WINDOWS_CERTIFICATE`、`WINDOWS_CERTIFICATE_PASSWORD` 后可启用 Authenticode 签名。macOS 的签名 / 公证在配置上述 Apple 相关 Secrets 后自动生效。缺少签名配置不会让发布失败。

## OCR 模型（ocrs 后端）

macOS 默认使用 Vision，无需模型。Windows 默认使用 Windows.Media.Ocr（需安装对应 OCR 语言包）；WinRT 创建失败时会回退到 ocrs，此时需要下载两个模型文件（不随仓库提交）：

```bash
mkdir -p ~/.snapit/models && cd ~/.snapit/models
curl -LO https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten
curl -LO https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten
```

模型目录查找顺序：环境变量 `SNAPIT_OCR_MODELS` → 可执行文件同级 `models/` → `~/.snapit/models`。

## 已知限制

- **元素框选**只在 macOS 和 Windows 可用，且依赖「辅助功能」权限；关闭「自动框选窗口」时元素框选也不生效。
- **录制**（GIF / APNG / MP4）只有选区录制，没有声音；最长时长默认 0（不限时长），上限 600 秒。历史记录里的录制用首帧 PNG 缩略图贴图，视频交给系统播放器打开，应用内不播放视频。
- **录制控制条与光标**：SnapIt 自己的窗口（选区高亮和控制条）在 macOS（ScreenCaptureKit）和 Windows（显示亲和力）上都不会被录进画面，控制条可以放在任意位置。macOS 光标由 ScreenCaptureKit 提供；Windows 的 DXGI 不含光标，由 SnapIt 获取并合成当前系统光标。点击检测只轮询当前鼠标状态，不需要全局输入钩子，也不增加 macOS 权限。
- **复制视频文件**：macOS 写入的是文件引用（可直接粘贴）；Windows 退回复制文件路径文本。
- **文字识别**的 ocrs 后端只支持拉丁字母；语言设置只对 Vision 和 Windows.Media.Ocr 生效。
- **自动更新**：macOS 只下载 DMG 到「下载」并在 Finder 中显示，安装（拖进 `/Applications`）由用户完成，因为应用无法可靠地替换正在运行的自身；从 `target/debug` 直接运行的开发构建不能自更新。Windows 更新通过管理员权限运行 `setup.exe` 完成，安装后自动重启。
- **水印与 HDR 边界**：图片水印只接受 PNG / JPEG，并始终按 sRGB 来源在线性 HDR 中合成，不会把普通图片提升为真正的 HDR 素材。单区域 HDR 截图支持标注和水印；跨显示器多区域拼接与 JPEG 仍为 8-bit。水印、标注或 HDR 编码失败会明确报错，不会静默省略水印或丢弃 HDR。
- **多选区与跨屏拼接**：多显示器会同时冻结；按住 `Shift` 可添加或移除多个选区，松开后完成收集，再通过工具栏导出。同一显示器内的多个选区按屏幕上的相对位置合成为一张透明底图，支持标注，但不支持调整选区大小、区域录制或滚动截图；跨显示器的选区保持为多个独立区域，导出时按选择顺序自上而下纵向拼接，不支持标注、调整选区大小、区域录制或滚动截图。
- **滚动截图**：在 macOS 上开启一键「自动滚动」需要「辅助功能」权限；未授权时不会阻断流程，仍可立即使用手动滚动（并在尝试开启自动滚动时打开权限引导）。极端大面积纯白/低纹理或大幅跳跃滚动会触发警告并暂停拼接，回滚至重合处即可恢复；单张长图最大高度默认 30,000 px（可在设置 `[scroll]` 中调整至最高 60,000 px），始终以 SDR 导出。

## 致谢与许可

- UI 基于 [gpui](https://www.gpui.rs/) 和 [gpui-kit](https://github.com/longbridge/gpui-kit)（gpui-kit 固定为 `0.9.1`，对应 GPUI `gpui-pre` 0.3.12），均以 Apache-2.0 许可发布。
- 截屏使用 [xcap](https://github.com/nashaofu/xcap)，录制部分使用以子模块引入的 [SnapIt 分支](https://github.com/gjfei/xcap)（macOS 改用 ScreenCaptureKit，补充区域录制、帧率上限、时间戳、光标、排除自身窗口与 `OutputSize` 尺寸选项），GIF 编码使用 `gif` 与 `color_quant`（共享色板 + 查表量化 + 只编码变化区域），APNG 复用现有 PNG 依赖编码无损 RGBA8 动画。SnapIt 默认请求 `OutputSize::Native` 物理像素；需要降低编码成本时可显式使用逻辑尺寸或 CPU 整数均值降采样，而不使用通用重采样滤镜。
- 纯 Rust OCR 使用 [ocrs](https://github.com/robertknight/ocrs)。
- 标注烘焙使用 [tiny-skia](https://github.com/RazrFalcon/tiny-skia)，文字位图使用 [fontdue](https://github.com/mooman219/fontdue)。
- 托盘、全局快捷键、开机启动分别使用 [tray-icon](https://github.com/tauri-apps/tray-icon)、[global-hotkey](https://github.com/tauri-apps/global-hotkey)、[auto-launch](https://github.com/zzzgydi/auto-launch)。
- 界面图标来自 [Lucide](https://lucide.dev/)（ISC）；应用图标与菜单栏模板见 `assets/`。

本项目使用 [Apache-2.0](LICENSE) 许可证。
