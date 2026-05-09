use crate::core::handle;
use crate::core::tray::proxy_speed::ProxyConnectionSpeedSampler;
use crate::process::AsyncHandler;
use crate::utils::tray_speed;
use crate::{Type, logging};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tauri::async_runtime::JoinHandle;

/// 托盘代理速率快照采样间隔。
const TRAY_SPEED_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// macOS 托盘速率任务控制器。
#[derive(Clone)]
pub struct TraySpeedController {
    speed_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Default for TraySpeedController {
    fn default() -> Self {
        Self {
            speed_task: Arc::new(Mutex::new(None)),
        }
    }
}

impl TraySpeedController {
    /// 创建 macOS 托盘速率任务控制器实例。
    pub fn new() -> Self {
        Self::default()
    }

    /// 根据配置开关启动或停止托盘代理速率任务。
    pub fn update_task(&self, enable_tray_speed: bool) {
        if enable_tray_speed {
            self.start_task();
        } else {
            self.stop_task();
        }
    }

    /// 启动托盘代理连接速率采集后台任务（基于 `/connections` 快照差值）。
    fn start_task(&self) {
        if handle::Handle::global().is_exiting() {
            return;
        }

        // 关键步骤：托盘不可用时不启动速率任务，避免无效连接重试。
        if !Self::has_main_tray() {
            logging!(warn, Type::Tray, "托盘不可用，跳过启动托盘速率任务");
            return;
        }

        let mut guard = self.speed_task.lock();
        if guard.as_ref().is_some_and(|task| !task.inner().is_finished()) {
            return;
        }

        let task = AsyncHandler::spawn(|| async move {
            let mut sampler = ProxyConnectionSpeedSampler::new();
            loop {
                if handle::Handle::global().is_exiting() {
                    break;
                }

                if !Self::has_main_tray() {
                    logging!(warn, Type::Tray, "托盘已不可用，停止托盘速率任务");
                    break;
                }

                match handle::Handle::mihomo().await.get_connections().await {
                    Ok(connections) => {
                        let connections = connections.connections.unwrap_or_default();
                        let speed = sampler.sample(&connections);
                        Self::apply_tray_speed(speed.up, speed.down);
                    }
                    Err(err) => {
                        logging!(debug, Type::Tray, "托盘代理速率采样失败: {err}");
                        Self::apply_tray_speed(0, 0);
                    }
                }

                tokio::time::sleep(TRAY_SPEED_SAMPLE_INTERVAL).await;
            }
        });

        *guard = Some(task);
    }

    /// 停止托盘速率采集后台任务并清除速率显示。
    fn stop_task(&self) {
        // 取出任务句柄并异步中止，避免阻塞当前菜单更新流程。
        let task = self.speed_task.lock().take();

        AsyncHandler::spawn(move || async move {
            // 关键步骤：等待任务退出，避免停止后继续把旧速率写回托盘。
            if let Some(task) = task {
                task.abort();
                let _ = task.await;
            }
        });

        let app_handle = handle::Handle::app_handle();
        if let Some(tray) = app_handle.tray_by_id("main") {
            let result = tray.with_inner_tray_icon(|inner| {
                if let Some(status_item) = inner.ns_status_item() {
                    tray_speed::clear_speed_attributed_title(&status_item);
                }
            });
            if let Err(err) = result {
                logging!(warn, Type::Tray, "清除富文本速率失败: {err}");
            }
        }
    }

    /// 判断主托盘图标当前是否仍然存在。
    fn has_main_tray() -> bool {
        handle::Handle::app_handle().tray_by_id("main").is_some()
    }

    /// 将采样到的代理上传/下载速率写入 macOS 托盘富文本标题。
    fn apply_tray_speed(up: u64, down: u64) {
        let app_handle = handle::Handle::app_handle();
        if let Some(tray) = app_handle.tray_by_id("main") {
            let result = tray.with_inner_tray_icon(move |inner| {
                if let Some(status_item) = inner.ns_status_item() {
                    tray_speed::set_speed_attributed_title(&status_item, up, down);
                }
            });
            if let Err(err) = result {
                logging!(warn, Type::Tray, "设置富文本速率失败: {err}");
            }
        }
    }
}
