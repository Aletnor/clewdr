//! 面板在线更新：由用户手动检查 GitHub 上的新版本，并自行决定是否安装。
//! 程序不会在后台检查或安装。安装只替换程序文件，账户、令牌、用量与设置所在的配置文件保持不变
use axum::{Json, extract::Query};
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::ApiError;

#[derive(Deserialize)]
pub struct UpdateQuery {
    #[serde(default)]
    check: bool,
}

/// `GET /api/update`：当前版本与是否支持在线更新；`?check=true` 时查询 GitHub 上的最新版本
pub async fn api_update_status(Query(q): Query<UpdateQuery>) -> Result<Json<Value>, ApiError> {
    let current = env!("CARGO_PKG_VERSION");
    #[cfg(feature = "portable")]
    {
        use crate::services::update::ClewdrUpdater;
        if !q.check {
            return Ok(Json(json!({ "current": current, "supported": true })));
        }
        let updater = ClewdrUpdater::new().map_err(|e| ApiError::internal(e.to_string()))?;
        let release = updater
            .latest()
            .await
            .map_err(|e| ApiError::service_unavailable(format!("无法获取最新版本信息：{e}")))?;
        let available = ClewdrUpdater::is_newer(current, release.version())
            .map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(Json(json!({
            "current": current,
            "supported": true,
            "latest": release.version(),
            "available": available,
            "notes": release.notes(),
        })))
    }
    #[cfg(not(feature = "portable"))]
    {
        let _ = q.check;
        Ok(Json(json!({ "current": current, "supported": false })))
    }
}

/// `POST /api/update`：安装最新版本，完成后程序自动重启。
/// 有请求正在处理时拒绝；开始后到重启前，新收到的对话请求一律拒收，避免被重启中断
pub async fn api_update_apply() -> Result<Json<Value>, ApiError> {
    #[cfg(feature = "portable")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};

        use tracing::{info, warn};

        use crate::services::{busy, update::ClewdrUpdater};

        static UPDATING: AtomicBool = AtomicBool::new(false);
        const BUSY: &str = "有请求正在生成，请等待完成后再更新。";

        if busy::any() {
            return Err(ApiError::bad_request(BUSY));
        }
        if UPDATING.swap(true, Ordering::SeqCst) {
            return Err(ApiError::bad_request("更新已在进行中。"));
        }
        let abort = || {
            busy::resume();
            UPDATING.store(false, Ordering::SeqCst);
        };
        // 先关上闸门再看一次：一条请求可能恰好在上面那次检查之后开始。
        // 对话处理函数先计数再看闸门，所以它要么被这里看到，要么被闸门拒收
        busy::pause();
        if busy::any() {
            abort();
            return Err(ApiError::bad_request(BUSY));
        }
        let result = async {
            // 替换程序文件之前取路径，替换后部分系统上取到的是旧文件的新位置
            let exe = std::env::current_exe().map_err(|e| ApiError::internal(e.to_string()))?;
            let updater = ClewdrUpdater::new().map_err(|e| ApiError::internal(e.to_string()))?;
            let release = updater
                .latest()
                .await
                .map_err(|e| ApiError::service_unavailable(format!("无法获取最新版本信息：{e}")))?;
            let current = env!("CARGO_PKG_VERSION");
            let newer = ClewdrUpdater::is_newer(current, release.version())
                .map_err(|e| ApiError::internal(e.to_string()))?;
            if !newer {
                return Err(ApiError::bad_request("当前已是最新版本。"));
            }
            updater
                .install(&release)
                .await
                .map_err(|e| ApiError::internal(format!("更新失败，程序未改动：{e}")))?;
            Ok((exe, release.version().to_owned()))
        }
        .await;
        let (exe, version) = match result {
            Ok(v) => v,
            Err(e) => {
                abort();
                return Err(e);
            }
        };
        // 先把响应发回面板，再重启。重启失败时旧版本继续运行，闸门重新打开
        let installed = version.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            info!("Restarting into version {installed}");
            let e = ClewdrUpdater::restart(&exe);
            warn!("Restart failed: {e}");
            println!("更新已安装，但未能自动重启（{e}）。当前仍在运行旧版本，请手动重新打开程序。");
            abort();
        });
        Ok(Json(json!({ "version": version })))
    }
    #[cfg(not(feature = "portable"))]
    {
        Err(ApiError::not_implemented("此构建不包含在线更新。"))
    }
}
