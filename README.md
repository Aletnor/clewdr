# clewdr 魔改版

把酒馆（SillyTavern）接到自己 Claude 订阅上的反代。基于 [Xerxes-2/clewdr](https://github.com/Xerxes-2/clewdr) v0.12.25，沿用原项目的 AGPL-3.0 许可证。

## 和原版的区别

- 新模型（Fable、Opus 5 系、Sonnet 5 系、Opus 4.8）的思考链能正常显示
- 自带提示词缓存：酒馆那边不用开缓存，它自己打断点，连着聊能省一大半额度
- 账户页多了「授权登录」，不用再贴 Cookie（Cookie 现在一两周就换不出令牌了）
- 被限流时只有整个订阅的额度用完才冷却账号，某一个模型的额度用完不影响别的模型
- 令牌续期不会再被旧的盖回去（原版偶尔会因此把号判失效）
- 不再把每条请求的全文写到硬盘上
- 去掉了对 4.6 系默认带「百万上下文」请求头的老逻辑，要用就在模型名后面加 `-1M`
- 面板重做过，手机上好用；多了一个「监控」页

## 下载

到 [Releases](../../releases) 里按自己的系统拿一个压缩包：

| 文件 | 用在哪 |
| --- | --- |
| `clewdr-windows-x86_64.zip` | Windows 电脑 |
| `clewdr-android-aarch64.zip` | 安卓手机（Termux） |
| `clewdr-musllinux-x86_64.zip` | Linux 服务器 |

## 启动

**Windows**：解压到一个文件夹，双击 `clewdr.exe`，会弹出一个黑窗口，别关它。

**安卓（Termux）**：把压缩包放进 Termux 能访问的目录，然后

```sh
unzip clewdr-android-aarch64.zip -d clewdr && cd clewdr
chmod +x clewdr
./clewdr
```

如果报找不到 `libc++_shared.so`，改用 `LD_LIBRARY_PATH=. ./clewdr`。

**Linux**：

```sh
unzip clewdr-musllinux-x86_64.zip -d clewdr && cd clewdr
chmod +x clewdr
./clewdr
```

第一次启动会自己生成两个密码并打印在窗口里：

- `API Password`：给酒馆用
- `Web Admin Password`：登管理面板用

它们也存在程序旁边的 `clewdr.toml` 里。这个文件里还有你的登录令牌，别发给任何人。

然后用浏览器打开 <http://127.0.0.1:8484>，拿 `Web Admin Password` 登进面板。

> 在连不上 Claude 的网络里跑（比如国内的电脑或手机），先把梯子开着再启动程序。面板「设置」里的代理留空时，它会自己用电脑上开着的系统代理（启动时会打印一行 `Proxy (from the system)`）；梯子是启动之后才开的，就重启一下程序。想指定别的代理，再去「设置」里填，例如 `http://127.0.0.1:7890`。

> 架在服务器上想从别的设备访问，请自己在前面加一层带 https 和门禁的反向代理，不要把 8484 端口直接暴露到公网。

## 加账号

面板「账户」页右上角点加号，切到「授权登录」：

1. 生成登录链接
2. 打开登录页，用你自己的 Claude 账号登录并同意
3. 页面上会显示一串码，整串复制回来贴进输入框
4. 完成登录

这样登的号大约四周要重新登一次，到期了卡片上会写「授权已过期」。

## 接酒馆

API 选 Claude，反向代理地址填 `http://127.0.0.1:8484/code/v1`，密码填 `API Password`。

模型名直接填，例如 `claude-opus-5-5`、`claude-fable-5-1`、`claude-sonnet-5-5`、`claude-opus-4-6`。酒馆下拉框里没有新模型的话，装一个 Custom Models 扩展自己把名字加进去。

思考强度：

- **Fable 和 5 系**：在模型名后面加后缀，`-low` / `-medium` / `-high` / `-xhigh` / `-max`，不加就是模型自己决定。酒馆里的「推理强度」下拉框对这些模型不起作用。
- **4.6 及更老的**：用酒馆的「推理强度」下拉框，它按档位从「回复长度」里切一块给思考。模型名后缀对它们无效。
- Fable 和 5 系的回复长度低于 16000 会被自动顶到 16000，温度固定为 1，这是为了让思考链能出来。

## 缓存

面板「设置」页，建议：自动缓存 开，缓存时长 1h，缓存深度 2。

酒馆自己的缓存开关不要打开（`config.yaml` 里 `enableSystemPromptCache`、`cachingAtDepth` 保持默认）。酒馆只要自己打了一个断点，这边的自动缓存就会整个让位。

## 其他

- 「监控」页能看每条请求的结局：成功率、每小时请求数、缓存命中率、被拒绝或被截断的原因、按模型的合计。数据来自程序自己记的一份统计（时间、模型、token 数、结局），不含任何聊天内容，存在配置文件旁边的 `clewdr-inspect.jsonl` 里，满 2MB 自动轮转。
- `GET /api/health` 不用密码，返回 200 说明程序活着，可以拿来做探活。
- 「检查更新」看的是这个仓库的 Releases，默认只提示不自动更新。
- 新模型刚发布时如果报 `Claude Code x.x.x does not support this model`，是程序里写的客户端版本号太旧，要改 `src/config/constants.rs` 里的 `CLAUDE_CODE_VERSION` 和 `CLAUDE_CODE_USER_AGENT` 再重新编译。

## 自己编译

装好 Rust 和 pnpm 之后：

```sh
cd frontend && pnpm install && pnpm run build && cd ..
cargo build --release --no-default-features --features embed-resource,portable --bin clewdr
```

`portable` 是把配置放在程序旁边；想放进系统的配置目录就换成 `xdg`。
