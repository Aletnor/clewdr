use std::{
    collections::HashSet,
    fmt::{Debug, Display},
    net::{IpAddr, SocketAddr},
};

use axum::http::{Uri, uri::Scheme};
use clap::Parser;
use colored::Colorize;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use http::uri::Authority;
use passwords::PasswordGenerator;
use serde::{Deserialize, Serialize};
use tokio::spawn;
use tracing::error;
use url::Url;
use wreq::Proxy;

use super::{CONFIG_PATH, ENDPOINT_URL};
use crate::{
    Args,
    config::{
        CC_CLIENT_ID, CookieStatus, UselessCookie, default_check_update, default_ip,
        default_max_retries, default_port, default_skip_cool_down, default_use_real_roles,
    },
    error::ClewdrError,
};

/// Generates a random password for authentication
/// Creates a secure 64-character password with mixed character types
///
/// # Returns
/// A random password string
fn generate_password() -> String {
    let pg = PasswordGenerator {
        length: 64,
        numbers: true,
        lowercase_letters: true,
        uppercase_letters: true,
        symbols: false,
        spaces: false,
        exclude_similar_characters: true,
        strict: true,
    };

    println!("{}", "Generating random password......".green());
    pg.generate_one().unwrap()
}

/// A struct representing the configuration of the application
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClewdrConfig {
    // key configurations
    #[serde(default)]
    pub cookie_array: HashSet<CookieStatus>,
    #[serde(default)]
    pub wasted_cookie: HashSet<UselessCookie>,

    // Server settings, cannot hot reload
    #[serde(default = "default_ip")]
    ip: IpAddr,
    #[serde(default = "default_port")]
    port: u16,

    // App settings, can hot reload, but meaningless
    #[serde(default = "default_check_update")]
    pub check_update: bool,
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default)]
    pub no_fs: bool,
    #[serde(default)]
    pub log_to_file: bool,

    // Network settings, can hot reload
    #[serde(default)]
    password: String,
    #[serde(default)]
    admin_password: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub rproxy: Option<Url>,

    // Api settings, can hot reload
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default)]
    pub preserve_chats: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub enable_web_count_tokens: bool,
    #[serde(default)]
    pub sanitize_messages: bool,

    // Cookie settings, can hot reload
    #[serde(default)]
    pub skip_first_warning: bool,
    #[serde(default)]
    pub skip_second_warning: bool,
    #[serde(default)]
    pub skip_restricted: bool,
    #[serde(default)]
    pub skip_non_pro: bool,
    #[serde(default = "default_skip_cool_down")]
    pub skip_rate_limit: bool,
    #[serde(default)]
    pub skip_normal_pro: bool,

    // Prompt configurations, can hot reload
    #[serde(default = "default_use_real_roles")]
    pub use_real_roles: bool,
    #[serde(default)]
    pub custom_h: Option<String>,
    #[serde(default)]
    pub custom_a: Option<String>,
    #[serde(default)]
    pub custom_prompt: String,

    // Claude Code settings, can hot reload
    #[serde(default)]
    pub claude_code_client_id: Option<String>,
    #[serde(default)]
    pub custom_system: Option<String>,

    // Prompt cache settings (Claude Code channel), can hot reload.
    // See middleware/claude/prompt_cache.rs.
    #[serde(default = "default_cache_on")]
    pub auto_cache: bool,
    #[serde(default = "default_cache_on")]
    pub magic_cache: bool,
    #[serde(default)]
    pub cache_ttl: Option<String>,
    // -1: stamp the last user message. n >= 0: SillyTavern's cachingAtDepth,
    // stamp the messages n and n + 2 role switches from the end.
    #[serde(default = "default_cache_depth")]
    pub cache_at_depth: i64,

    // Carry the upstream atis slot on /v1/messages, can hot reload.
    // See claude_code_state/atis.rs. Off: requests go out without it.
    #[serde(default)]
    pub atis: bool,

    // Skip field, can hot reload
    #[serde(skip)]
    pub wreq_proxy: Option<Proxy>,
}

fn default_cache_on() -> bool {
    true
}

fn default_cache_depth() -> i64 {
    -1
}

impl Default for ClewdrConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            check_update: default_check_update(),
            auto_update: false,
            cookie_array: HashSet::new(),
            wasted_cookie: HashSet::new(),
            password: String::new(),
            admin_password: String::new(),
            proxy: None,
            ip: default_ip(),
            port: default_port(),
            rproxy: None,
            use_real_roles: default_use_real_roles(),
            custom_prompt: String::new(),
            custom_h: None,
            custom_a: None,
            wreq_proxy: None,
            preserve_chats: false,
            web_search: false,
            enable_web_count_tokens: false,
            sanitize_messages: false,
            skip_first_warning: false,
            skip_second_warning: false,
            skip_restricted: false,
            skip_non_pro: false,
            skip_rate_limit: default_skip_cool_down(),
            skip_normal_pro: false,
            claude_code_client_id: None,
            custom_system: None,
            auto_cache: default_cache_on(),
            magic_cache: default_cache_on(),
            cache_ttl: None,
            cache_at_depth: default_cache_depth(),
            atis: false,
            no_fs: false,
            log_to_file: false,
        }
    }
}

impl Display for ClewdrConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // What a person needs right after starting it: where the panel is and
        // the two things to paste into the chat client. Only the Claude Code
        // endpoint is shown, since that is the one logged-in accounts serve.
        // The address is one a browser on this machine can open, even when the
        // server was bound to every interface.
        let host = if self.ip.is_unspecified() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            self.ip
        };
        let authority: Authority = SocketAddr::new(host, self.port)
            .to_string()
            .parse()
            .map_err(|_| std::fmt::Error)?;
        let panel = Uri::builder()
            .scheme(Scheme::HTTP)
            .authority(authority)
            .path_and_query("")
            .build()
            .map_err(|_| std::fmt::Error)?
            .to_string();
        write!(
            f,
            "管理面板（用浏览器打开）：{}\n\
            管理员密钥（登面板用）：{}\n\
            酒馆里填的反代地址：{}\n\
            酒馆里填的密码（API 密钥）：{}\n",
            panel.green().underline(),
            self.admin_password.yellow(),
            (panel.clone() + "code/v1").green().underline(),
            self.password.yellow(),
        )?;
        if let Some(ref proxy) = self.proxy {
            writeln!(f, "代理：{}", proxy.to_string().blue())?;
        }
        if let Some(ref rproxy) = self.rproxy {
            writeln!(f, "上游地址覆盖：{}", rproxy.to_string().blue())?;
        }
        writeln!(
            f,
            "第一次用：打开管理面板 → 账户 → 右上角加号 → 授权登录。\n\
            反代地址和密码在面板「设置」页最上面也能复制。这个窗口要一直开着。"
        )?;
        Ok(())
    }
}

impl ClewdrConfig {
    pub fn user_auth(&self, key: &str) -> bool {
        key == self.password
    }

    pub fn admin_auth(&self, key: &str) -> bool {
        key == self.admin_password
    }

    /// A blank key arriving from the panel means "leave it as it is". Without
    /// this `validate` would roll a fresh random one, and whoever cleared the
    /// field by accident is locked out of a key they have never seen.
    pub fn keep_keys_if_blank(&mut self, current: &ClewdrConfig) {
        if self.password.trim().is_empty() {
            self.password = current.password.clone();
        }
        if self.admin_password.trim().is_empty() {
            self.admin_password = current.admin_password.clone();
        }
    }

    pub fn cc_client_id(&self) -> String {
        self.claude_code_client_id
            .as_deref()
            .unwrap_or(CC_CLIENT_ID)
            .to_string()
    }

    /// Loads configuration from files and environment variables
    /// Combines settings from config.toml, clewdr.toml, and environment variables
    /// Also loads cookies from a file if specified
    ///
    /// # Returns
    /// * Config instance
    pub fn new() -> Self {
        sweep_unfinished_saves(CONFIG_PATH.as_path());
        // Load config from TOML then override with environment variables.
        // Use double underscore "__" to map nested keys.
        let mut config: ClewdrConfig = Figment::from(Toml::file(CONFIG_PATH.as_path()))
            .admerge(Env::prefixed("CLEWDR_").split("__"))
            .extract_lossy()
            .inspect_err(|e| {
                error!("Failed to load config: {}", e);
                // From here on an empty config is used and then saved over the
                // file. Whatever is in the file now (it is there, it just did
                // not parse) is the only copy of the accounts and keys: keep it.
                Self::keep_unreadable_copy();
            })
            .unwrap_or_default();
        if let Some(ref f) = Args::try_parse().ok().and_then(|a| a.file) {
            // load cookies from file
            if f.exists() {
                if let Ok(cookies) = std::fs::read_to_string(f) {
                    let cookies = cookies
                        .lines()
                        .filter_map(|line| CookieStatus::new(line, None).ok());
                    config.cookie_array.extend(cookies);
                } else {
                    error!("Failed to read cookie file: {}", f.display());
                }
            } else {
                error!("Cookie file not found: {}", f.display());
            }
        }
        let config = config.validate();
        if !config.no_fs {
            let config_clone = config.to_owned();
            spawn(async move {
                config_clone.save().await.unwrap_or_else(|e| {
                    error!("Failed to save config: {}", e);
                });
            });
        }
        config
    }

    /// Gets the API endpoint for the Claude service
    /// Returns the reverse proxy URL if configured, otherwise the default endpoint
    ///
    /// # Returns
    /// The URL for the API endpoint
    pub fn endpoint(&self) -> Url {
        if let Some(ref proxy) = self.rproxy {
            return proxy.to_owned();
        }
        ENDPOINT_URL.to_owned()
    }

    /// address of proxy
    pub fn address(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }

    /// Copy a config file that would not parse to `clewdr.toml.broken-<time>`.
    /// The log is not set up yet when the config is first read, so the notice
    /// goes to the console as well, next to the start-up banner.
    fn keep_unreadable_copy() {
        match keep_copy_of(CONFIG_PATH.as_path()) {
            Ok(Some(backup)) => {
                error!(
                    "The config file could not be read, a copy was kept at {}",
                    backup.display()
                );
                eprintln!(
                    "配置文件读不出来（格式坏了），这次先用一份空配置启动。\n\
                    原来的文件另存在：{}\n\
                    账号和密钥都还在那份文件里，修好格式改回原名就能找回。",
                    backup.display().to_string().yellow()
                );
            }
            Ok(None) => {}
            Err(e) => {
                error!("The config file could not be read, and keeping a copy failed: {e}");
                eprintln!("配置文件读不出来，另存一份也没成功：{e}");
            }
        }
    }

    /// Save the configuration to a file
    pub async fn save(&self) -> Result<(), ClewdrError> {
        if self.no_fs {
            return Ok(());
        }
        if let Some(parent) = CONFIG_PATH.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path = CONFIG_PATH.as_path();
        let data = toml::ser::to_string_pretty(self)?;
        write_whole(path, &data).await?;
        Ok(())
    }

    /// Validate the configuration
    pub fn validate(mut self) -> Self {
        if self.password.trim().is_empty() {
            self.password = generate_password();
        }
        if self.admin_password.trim().is_empty() {
            self.admin_password = generate_password();
        }
        self.cookie_array = self.cookie_array.into_iter().map(|x| x.reset()).collect();
        self.wreq_proxy = self.proxy.to_owned().and_then(|p| {
            Proxy::all(p)
                .inspect_err(|e| {
                    self.proxy = None;
                    error!("Failed to parse proxy: {}", e);
                })
                .ok()
        });
        // No proxy filled in: follow the one the machine itself is set to use.
        // A VPN client in "system proxy" mode only announces itself there, and
        // without this a fresh install talks to upstream directly and is refused.
        if self.wreq_proxy.is_none()
            && let Some(found) = system_proxy()
            && let Ok(proxy) = Proxy::all(found.as_str())
        {
            println!("代理（跟随系统设置）：{}", found.blue());
            self.wreq_proxy = Some(proxy);
        }
        self
    }
}

/// The name of the file at `path`, with something put before and after it.
fn beside(path: &std::path::Path, before: &str, after: &str) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("clewdr.toml");
    path.with_file_name(format!("{before}{name}{after}"))
}

/// Replace the file at `path` with `data`, either whole or not at all.
///
/// The text goes into a file next to it, is flushed to disk, and is then
/// swapped in. Writing straight into the file empties it first: cut off
/// there (disk full, process killed) and what is left is an empty or half
/// config, accounts and keys gone.
async fn write_whole(path: &std::path::Path, data: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = beside(path, ".", &format!(".{}.{seq}.tmp", std::process::id()));
    let written = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        file.write_all(data.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if written.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    written
}

/// The files next to `path` whose names start with `prefix`.
fn siblings(path: &std::path::Path, prefix: &str) -> Vec<std::path::PathBuf> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => std::path::Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .collect()
}

/// Remove what a save left behind when the program was stopped in the
/// middle of one. Those files were never swapped in, so nothing reads them.
fn sweep_unfinished_saves(path: &std::path::Path) {
    let prefix = beside(path, ".", ".");
    let Some(prefix) = prefix.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    for left in siblings(path, prefix) {
        if left.extension().is_some_and(|e| e == "tmp") {
            let _ = std::fs::remove_file(left);
        }
    }
}

/// Copy the file at `path` to `<name>.broken-<time>` next to it and say
/// where it went. A file that is missing or empty has nothing worth keeping,
/// and a file that was already copied is not copied again, so a program that
/// keeps being restarted on the same bad file does not fill the disk.
fn keep_copy_of(path: &std::path::Path) -> std::io::Result<Option<std::path::PathBuf>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => {}
        _ => return Ok(None),
    }
    let text = std::fs::read(path)?;
    let marker = beside(path, "", ".broken-");
    if let Some(marker) = marker.file_name().and_then(|n| n.to_str())
        && let Some(same) = siblings(path, marker)
            .into_iter()
            .find(|kept| std::fs::read(kept).is_ok_and(|kept| kept == text))
    {
        return Ok(Some(same));
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let backup = beside(path, "", &format!(".broken-{stamp}"));
    // It holds the accounts and keys, so it is the owner's alone from the start.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(&backup)?, &text)?;
    Ok(Some(backup))
}

/// The proxy this machine is configured to use: the usual environment
/// variables first, then on Windows the system proxy setting.
fn system_proxy() -> Option<String> {
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(value) = std::env::var(name)
            && let Some(proxy) = proxy_url(&value)
        {
            return Some(proxy);
        }
    }
    #[cfg(windows)]
    {
        let read = |value: &str| -> Option<String> {
            let out = std::process::Command::new("reg")
                .args([
                    "query",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    value,
                ])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find(|line| line.trim_start().starts_with(value))
                .and_then(|line| line.split_whitespace().last())
                .map(str::to_owned)
        };
        if read("ProxyEnable").as_deref() == Some("0x1") {
            return read("ProxyServer").and_then(|server| proxy_url(&server));
        }
    }
    None
}

/// Turns a proxy setting into a URL. Takes a plain `host:port`, a full URL,
/// or the Windows per-protocol form `http=host:port;https=host:port`.
fn proxy_url(setting: &str) -> Option<String> {
    let setting = setting.trim();
    if setting.is_empty() {
        return None;
    }
    if !setting.contains('=') {
        return Some(if setting.contains("://") {
            setting.to_owned()
        } else {
            format!("http://{setting}")
        });
    }
    let entries: Vec<(&str, &str)> = setting
        .split(';')
        .filter_map(|entry| entry.trim().split_once('='))
        .collect();
    let pick = |scheme: &str| {
        entries
            .iter()
            .find(|(name, host)| name.eq_ignore_ascii_case(scheme) && !host.is_empty())
            .map(|(_, host)| *host)
    };
    if let Some(host) = pick("https").or_else(|| pick("http")) {
        return Some(if host.contains("://") {
            host.to_owned()
        } else {
            format!("http://{host}")
        });
    }
    pick("socks").map(|host| {
        if host.contains("://") {
            host.to_owned()
        } else {
            format!("socks5://{host}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{keep_copy_of, proxy_url, sweep_unfinished_saves, write_whole};

    /// An empty directory of its own for one test.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("clewdr-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn names_in(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("a readable directory")
            .map(|e| e.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_save_swaps_the_whole_file_in_and_leaves_nothing_behind() {
        let dir = scratch("swap");
        let path = dir.join("clewdr.toml");
        std::fs::write(&path, "old = true\n").unwrap();
        write_whole(&path, "new = true\n").await.unwrap();
        write_whole(&path, "newer = true\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer = true\n");
        assert_eq!(names_in(&dir), ["clewdr.toml"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_save_that_cannot_finish_leaves_what_was_there() {
        let dir = scratch("stuck");
        // Something a file cannot be swapped over: a directory that is not empty.
        let path = dir.join("clewdr.toml");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("kept"), "still here").unwrap();
        assert!(write_whole(&path, "new = true\n").await.is_err());
        assert_eq!(names_in(&dir), ["clewdr.toml"]);
        assert_eq!(std::fs::read_to_string(path.join("kept")).unwrap(), "still here");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_that_would_not_parse_is_copied_before_it_is_written_over() {
        let dir = scratch("broken");
        let path = dir.join("clewdr.toml");
        assert_eq!(keep_copy_of(&path).unwrap(), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(keep_copy_of(&path).unwrap(), None);
        std::fs::write(&path, "cookie_array = [ this is not toml").unwrap();
        let copy = keep_copy_of(&path).unwrap().expect("a copy");
        let name = copy.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("clewdr.toml.broken-"), "{name}");
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            "cookie_array = [ this is not toml"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&copy).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Restarted again and again on the same bad file: still one copy.
        assert_eq!(keep_copy_of(&path).unwrap(), Some(copy.clone()));
        assert_eq!(names_in(&dir), ["clewdr.toml".to_owned(), name]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_a_stopped_save_left_behind_is_swept_and_nothing_else() {
        let dir = scratch("sweep");
        let path = dir.join("clewdr.toml");
        for name in [
            "clewdr.toml",
            ".clewdr.toml.4242.0.tmp",
            ".clewdr.toml.4242.17.tmp",
            "clewdr.toml.broken-20261013-000000",
            "clewdr.toml.bak",
            ".other.toml.1.0.tmp",
            "notes.tmp",
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        sweep_unfinished_saves(&path);
        assert_eq!(
            names_in(&dir),
            [
                ".other.toml.1.0.tmp",
                "clewdr.toml",
                "clewdr.toml.bak",
                "clewdr.toml.broken-20261013-000000",
                "notes.tmp",
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bare_host_and_port_becomes_an_http_proxy() {
        assert_eq!(proxy_url("127.0.0.1:7890").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(proxy_url(" 127.0.0.1:7890 ").as_deref(), Some("http://127.0.0.1:7890"));
    }

    #[test]
    fn a_full_url_is_kept_as_it_is() {
        assert_eq!(proxy_url("socks5://127.0.0.1:1080").as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(proxy_url("http://127.0.0.1:7897").as_deref(), Some("http://127.0.0.1:7897"));
    }

    #[test]
    fn the_per_protocol_form_prefers_https_then_http_then_socks() {
        assert_eq!(
            proxy_url("http=127.0.0.1:7890;https=127.0.0.1:7891;socks=127.0.0.1:7892").as_deref(),
            Some("http://127.0.0.1:7891")
        );
        assert_eq!(proxy_url("http=127.0.0.1:7890").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(proxy_url("socks=127.0.0.1:7892").as_deref(), Some("socks5://127.0.0.1:7892"));
    }

    #[test]
    fn nothing_usable_means_no_proxy() {
        assert_eq!(proxy_url(""), None);
        assert_eq!(proxy_url("ftp=127.0.0.1:21"), None);
    }
}
