use std::{
    env,
    fs::File,
    io::{BufReader, copy},
    path::{Path, PathBuf},
};

use colored::Colorize;
use http::header::USER_AGENT;
use serde::Deserialize;
use snafu::ResultExt;
use tracing::info;
use wreq::Client;
use zip::ZipArchive;

use crate::{
    Args,
    config::{CLEWDR_CONFIG, CONFIG_PATH},
    error::{ClewdrError, WreqSnafu},
};

#[derive(Debug, Deserialize)]
pub struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    assets: Vec<GitHubAsset>,
}

impl GitHubRelease {
    /// Version number without the leading `v`
    pub fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    /// Release notes as written on the release page
    pub fn notes(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// GitHub account whose `clewdr` repository holds this fork's releases.
const FORK_OWNER: &str = "Aletnor";

/// Set on the process started by [`ClewdrUpdater::restart`]
pub const RESTART_ENV: &str = "CLEWDR_RESTARTED";

/// `clewdr.toml` → `clewdr.toml.bak-v0.12.29`, in the same folder
fn config_backup_path(config: &Path, version: &str) -> PathBuf {
    let name = config
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("clewdr.toml");
    config.with_file_name(format!("{name}.bak-v{version}"))
}

/// Updater for the ClewdR application
/// Handles checking for updates and updating the application
pub struct ClewdrUpdater {
    client: Client,
    user_agent: String,
    repo_owner: &'static str,
    repo_name: &'static str,
}

impl ClewdrUpdater {
    /// Creates a new ClewdrUpdater instance
    ///
    /// # Returns
    /// * `Result<Self, ClewdrError>` - A new updater instance or an error
    pub fn new() -> Result<Self, ClewdrError> {
        // This fork's own releases. The stock updater derived the owner from the
        // crate authors, which points at upstream, and an update from there
        // would replace the fork with the stock build.
        let repo_owner = FORK_OWNER;
        let repo_name = env!("CARGO_PKG_NAME");
        let policy = wreq::redirect::Policy::default();
        // A machine that cannot reach GitHub must not hang over this.
        let mut builder = wreq::Client::builder()
            .redirect(policy)
            .timeout(std::time::Duration::from_secs(8));
        // GitHub is reached the way upstream is: through the outbound proxy
        // from the panel, or the system proxy picked up at startup. Without
        // it a machine behind a VPN in system-proxy mode goes out directly.
        if let Some(proxy) = CLEWDR_CONFIG.load().wreq_proxy.to_owned() {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().context(WreqSnafu {
            msg: "Failed to create HTTP client",
        })?;

        let user_agent = format!(
            "clewdr/{} (+https://github.com/{}/{})",
            env!("CARGO_PKG_VERSION"),
            repo_owner,
            repo_name
        );

        Ok(Self {
            client,
            user_agent,
            repo_owner,
            repo_name,
        })
    }

    /// Update from the command line (`--update`): install the latest release
    /// if it is newer, then exit so the user starts the new version.
    /// Without the flag nothing is checked at startup; checking and installing
    /// are left to the user, from the panel.
    pub async fn check_for_updates(&self) -> Result<bool, ClewdrError> {
        if CLEWDR_CONFIG.load().no_fs {
            info!("Update check skipped due to no_fs feature");
            return Ok(false);
        }

        let args: Args = clap::Parser::parse();
        if !args.update {
            return Ok(false);
        }

        info!("Checking for updates...");
        let release = self.latest().await?;
        let current_version = env!("CARGO_PKG_VERSION");
        if !Self::is_newer(current_version, release.version())? {
            info!("Already at the latest version {}", current_version.green());
            return Ok(false);
        }
        info!(
            "New version {} available (current: {})",
            release.version().green().italic(),
            current_version.yellow()
        );
        self.install(&release).await?;
        println!("{}", format!("已更新到 v{}，请重新启动程序。", release.version()).green());
        std::process::exit(0);
    }

    /// Fetches the latest release of this fork from GitHub
    pub async fn latest(&self) -> Result<GitHubRelease, ClewdrError> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            self.repo_owner, self.repo_name
        );

        let response = self
            .client
            .get(&url)
            .header(USER_AGENT, &self.user_agent)
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to fetch latest release from GitHub",
            })?
            .error_for_status()
            .context(WreqSnafu {
                msg: "Fetch latest release from GitHub returned an error",
            })?;

        response.json().await.context(WreqSnafu {
            msg: "Failed to parse GitHub release response",
        })
    }

    /// Installs `release` over the running program, without restarting it.
    ///
    /// Only the program file is replaced. Accounts, tokens, usage and settings
    /// live in the config file next to it and are left where they are; the
    /// config is written out once more first, and a copy of it is kept as
    /// `clewdr.toml.bak-v<current version>` before anything is replaced.
    pub async fn install(&self, release: &GitHubRelease) -> Result<(), ClewdrError> {
        let latest_version = release.version();

        // Find appropriate asset for this platform
        let asset = self.find_appropriate_asset(release)?;

        info!("Downloading update from {}", asset.browser_download_url);

        // Create a temporary directory
        let temp_dir = tempfile::tempdir()?;
        let zip_path = temp_dir.path().join("update.zip");

        // Download the asset
        let response = self
            .client
            .get(&asset.browser_download_url)
            .header(USER_AGENT, &self.user_agent)
            // The short client timeout is for the version check, not for this.
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .context(WreqSnafu {
                msg: "Failed to download update asset",
            })?
            .error_for_status()
            .context(WreqSnafu {
                msg: "Download update asset returned an error",
            })?;

        // Save the downloaded file
        let content = response.bytes().await.context(WreqSnafu {
            msg: "Failed to read response bytes from update asset",
        })?;
        let mut file = File::create(&zip_path)?;
        copy(&mut content.as_ref(), &mut file)?;

        // Extract the zip
        let extract_dir = temp_dir.path().join("extracted");
        std::fs::create_dir_all(&extract_dir)?;

        let file = File::open(&zip_path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        // Extract all files
        archive.extract(&extract_dir)?;

        let binary_name = if cfg!(windows) {
            "clewdr.exe"
        } else {
            "clewdr"
        };
        let binary_path = extract_dir.join(binary_name);

        if !binary_path.exists() {
            return Err(ClewdrError::AssetError {
                msg: format!("Binary not found in the update package: {binary_name}"),
            });
        }

        // Make the binary executable on Unix systems
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&binary_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&binary_path, perms)?;
        }

        #[cfg(target_os = "android")]
        {
            use tracing::warn;
            let so_path = extract_dir.join("libc++_shared.so");
            if so_path.exists() {
                let current_dir = env::current_exe()?
                    .parent()
                    .ok_or(ClewdrError::AssetError {
                        msg: "Failed to get current directory".to_string(),
                    })?
                    .to_path_buf();
                let target_so_path = current_dir.join("libc++_shared.so");
                std::fs::copy(&so_path, &target_so_path)?;
                info!("Copied libc++_shared.so to the application directory");
            } else {
                warn!("libc++_shared.so not found in the update package");
            }
        }

        // The config on disk is brought up to date and copied aside before the
        // program is touched, so the accounts survive whatever happens next.
        CLEWDR_CONFIG.load().save().await?;
        let config_path = CONFIG_PATH.as_path();
        if config_path.exists() {
            let backup = config_backup_path(config_path, env!("CARGO_PKG_VERSION"));
            std::fs::copy(config_path, &backup)?;
            info!("Config copied to {}", backup.display());
        }

        // Replace the current binary
        self_replace::self_replace(&binary_path)?;

        info!("Updated to version {}", latest_version.green());
        Ok(())
    }

    /// Starts the program again from `exe` with the same arguments.
    ///
    /// On Unix the running process is replaced in place: same pid, same
    /// terminal, same service unit, so a shell or systemd sees nothing change.
    /// On Windows a new process is started on the same console and this one
    /// exits once it is running; the new one waits for the port to come free.
    /// Returns only when that failed, with this process still serving.
    pub fn restart(exe: &Path) -> std::io::Error {
        let mut command = std::process::Command::new(exe);
        command.args(env::args_os().skip(1)).env(RESTART_ENV, "1");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.exec()
        }
        #[cfg(not(unix))]
        {
            match command.spawn() {
                Ok(_) => std::process::exit(0),
                Err(e) => e,
            }
        }
    }

    /// Finds the appropriate asset for the current platform and architecture
    ///
    /// # Arguments
    /// * `release` - GitHub release information containing available assets
    ///
    /// # Returns
    /// * `Result<&'a GitHubAsset, ClewdrError>` - Appropriate asset or error if none found
    fn find_appropriate_asset<'a>(
        &self,
        release: &'a GitHubRelease,
    ) -> Result<&'a GitHubAsset, ClewdrError> {
        // Determine platform and architecture
        let os = env::consts::OS;
        let arch = env::consts::ARCH;

        let target = match (os, arch) {
            ("windows", "x86_64") => "windows-x86_64",
            ("linux", "x86_64") => {
                if cfg!(target_env = "musl") {
                    "musllinux-x86_64"
                } else {
                    "linux-x86_64"
                }
            }
            ("linux", "aarch64") => {
                if cfg!(target_env = "musl") {
                    "musllinux-aarch64"
                } else {
                    "linux-aarch64"
                }
            }
            ("macos", "x86_64") => "macos-x86_64",
            ("macos", "aarch64") => "macos-aarch64",
            ("android", "aarch64") => "android-aarch64",
            _ => {
                return Err(ClewdrError::AssetError {
                    msg: format!("Unsupported platform: {os}-{arch}"),
                });
            }
        };
        info!("Detected platform: {}", target);
        release
            .assets
            .iter()
            .find(|asset| asset.name.contains(target) && asset.name.ends_with(".zip"))
            .ok_or(ClewdrError::AssetError {
                msg: format!("No suitable asset found for platform: {target}"),
            })
    }

    /// Compares two version strings to determine if an update is needed
    /// Parses versions in the format major.minor.patch
    ///
    /// # Arguments
    /// * `current` - Current version string
    /// * `latest` - Latest version string from GitHub
    ///
    /// # Returns
    /// * `Result<bool, ClewdrError>` - True if latest is newer than current, false otherwise
    pub fn is_newer(current: &str, latest: &str) -> Result<bool, ClewdrError> {
        let parse_version = |v: &str| -> Result<(u32, u32, u32), ClewdrError> {
            let vec = v.split('.').collect::<Vec<_>>();
            let [major, minor, patch, ..] = vec.as_slice() else {
                return Err(ClewdrError::InvalidVersion {
                    version: v.to_string(),
                });
            };
            Ok((major.parse()?, minor.parse()?, patch.parse()?))
        };
        let current = parse_version(current)?;
        let latest = parse_version(latest)?;
        Ok(current < latest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_higher_version_counts_as_newer() {
        assert!(ClewdrUpdater::is_newer("0.12.29", "0.12.30").unwrap());
        assert!(ClewdrUpdater::is_newer("0.12.29", "0.13.0").unwrap());
        assert!(!ClewdrUpdater::is_newer("0.12.29", "0.12.29").unwrap());
        assert!(!ClewdrUpdater::is_newer("0.12.30", "0.12.29").unwrap());
        assert!(ClewdrUpdater::is_newer("0.12.9", "0.12.10").unwrap());
    }

    #[test]
    fn the_config_copy_sits_next_to_the_config() {
        let p = Path::new("/a/b/clewdr.toml");
        assert_eq!(
            config_backup_path(p, "0.12.29"),
            PathBuf::from("/a/b/clewdr.toml.bak-v0.12.29")
        );
    }
}
