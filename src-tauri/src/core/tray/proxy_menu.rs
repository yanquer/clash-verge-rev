//! 托盘代理菜单构建与延迟状态模块
//!
//! 负责代理节点菜单的 ID 编解码、延迟状态格式化、测速状态缓存与菜单项构建，
//! 将代理菜单相关逻辑从托盘主模块中拆出，避免菜单渲染、测速和事件解析互相耦合。

use crate::config::Config;
use crate::core::{handle, tray::Tray};
use crate::process::AsyncHandler;
use crate::{Type, logging};
use anyhow::Result;
use clash_verge_i18n::t;
#[cfg(not(target_os = "macos"))]
use clash_verge_logging::logging_error;
use futures::stream::{self, StreamExt as _};
use parking_lot::Mutex;
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tauri::menu::{CheckMenuItem, IsMenuItem, MenuItem, Submenu};
use tauri::{AppHandle, Wry};
use tauri_plugin_mihomo::models::{Proxies, Proxy};

const DEFAULT_LATENCY_TEST_URL: &str = "http://cp.cloudflare.com/generate_204";
const DEFAULT_LATENCY_TIMEOUT_MS: u32 = 10_000;
const TRAY_DELAY_TEST_CONCURRENCY: usize = 10;

/// 代理菜单点击后的业务动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayProxyMenuAction {
    SelectNode { group: String, proxy: String },
    TestGroupDelay { group: String },
}

/// 托盘延迟显示等级，用于 macOS 富文本上色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayDelayColor {
    Default,
    Success,
    Primary,
    Warning,
    Error,
}

/// 托盘节点延迟状态，统一表达未知、测速中、超时、错误与有效延迟。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayDelayState {
    Unknown,
    Testing,
    Timeout,
    Error,
    Value(u32),
}

/// 托盘延迟格式化后的文本与颜色信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayDelayDisplay {
    pub text: String,
    pub color: TrayDelayColor,
}

/// 代理菜单项标题的拆分结果，range 使用 AppKit 需要的 UTF-16 单位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayProxyTitleParts {
    pub title: String,
    pub delay_start: usize,
    pub delay_len: usize,
}

/// 代理菜单 ID 编解码器，避免组名或节点名包含 `_` 时解析错误。
pub struct TrayProxyMenuId;

impl TrayProxyMenuId {
    const NODE_PREFIX: &'static str = "proxy_node:";
    const DELAY_TEST_PREFIX: &'static str = "proxy_delay_test:";

    /// 创建代理节点选择菜单 ID。
    pub fn node(group: &str, proxy: &str) -> String {
        format!("{}{}:{}", Self::NODE_PREFIX, Self::encode(group), Self::encode(proxy))
    }

    /// 创建代理组延迟测试菜单 ID。
    pub fn delay_test(group: &str) -> String {
        format!("{}{}", Self::DELAY_TEST_PREFIX, Self::encode(group))
    }

    /// 解析代理菜单 ID 为具体动作。
    pub fn parse(id: &str) -> Option<TrayProxyMenuAction> {
        if let Some(rest) = id.strip_prefix(Self::NODE_PREFIX) {
            let (group, proxy) = rest.split_once(':')?;
            return Some(TrayProxyMenuAction::SelectNode {
                group: Self::decode(group)?,
                proxy: Self::decode(proxy)?,
            });
        }

        id.strip_prefix(Self::DELAY_TEST_PREFIX)
            .and_then(Self::decode)
            .map(|group| TrayProxyMenuAction::TestGroupDelay { group })
    }

    /// 对菜单 ID 片段进行百分号编码。
    fn encode(value: &str) -> String {
        utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
    }

    /// 对菜单 ID 片段进行百分号解码。
    fn decode(value: &str) -> Option<String> {
        percent_decode_str(value).decode_utf8().ok().map(|v| v.into_owned())
    }
}

/// 托盘延迟格式化器，集中处理文本、颜色和标题 range。
pub struct TrayDelayFormatter;

impl TrayDelayFormatter {
    /// 将延迟状态转换为菜单显示文本和颜色。
    pub fn display(state: TrayDelayState) -> TrayDelayDisplay {
        match state {
            TrayDelayState::Unknown => TrayDelayDisplay {
                text: "-".into(),
                color: TrayDelayColor::Default,
            },
            TrayDelayState::Testing => TrayDelayDisplay {
                text: t!("tray.delayStatus.testing").into_owned(),
                color: TrayDelayColor::Primary,
            },
            TrayDelayState::Timeout => TrayDelayDisplay {
                text: t!("tray.delayStatus.timeout").into_owned(),
                color: TrayDelayColor::Error,
            },
            TrayDelayState::Error => TrayDelayDisplay {
                text: t!("tray.delayStatus.error").into_owned(),
                color: TrayDelayColor::Error,
            },
            TrayDelayState::Value(delay) => TrayDelayDisplay {
                text: format!("{delay}ms"),
                color: Self::delay_color(delay),
            },
        }
    }

    /// 根据原始延迟数值和超时时间转换为状态。
    pub const fn state_from_delay(delay: u32, timeout: u32) -> TrayDelayState {
        if delay == 0 || (delay >= timeout && delay <= 100_000) {
            TrayDelayState::Timeout
        } else if delay > 100_000 {
            TrayDelayState::Error
        } else {
            TrayDelayState::Value(delay)
        }
    }

    /// 生成带右对齐制表符的节点菜单标题，并返回延迟文本范围。
    pub fn node_title(proxy_name: &str, display: &TrayDelayDisplay) -> TrayProxyTitleParts {
        let delay_start = Self::utf16_len(proxy_name) + 1;
        TrayProxyTitleParts {
            title: format!("{}\t{}", proxy_name, display.text),
            delay_start,
            delay_len: Self::utf16_len(&display.text),
        }
    }

    /// 根据有效延迟值映射前端一致的颜色等级。
    const fn delay_color(delay: u32) -> TrayDelayColor {
        if delay >= 10_000 {
            TrayDelayColor::Error
        } else if delay >= 400 {
            TrayDelayColor::Warning
        } else if delay >= 250 {
            TrayDelayColor::Primary
        } else {
            TrayDelayColor::Success
        }
    }

    /// 计算 AppKit NSRange 所需的 UTF-16 长度。
    fn utf16_len(text: &str) -> usize {
        text.encode_utf16().count()
    }
}

/// 托盘代理节点延迟状态控制器，负责缓存测速状态并执行代理组测速。
pub struct TrayProxyLatencyController {
    cache: Mutex<HashMap<String, TrayDelayState>>,
    testing_groups: Mutex<HashSet<String>>,
}

impl Default for TrayProxyLatencyController {
    fn default() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            testing_groups: Mutex::new(HashSet::new()),
        }
    }
}

impl TrayProxyLatencyController {
    /// 获取全局托盘延迟控制器实例。
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<TrayProxyLatencyController> = OnceLock::new();
        INSTANCE.get_or_init(Self::default)
    }

    /// 读取节点延迟状态，优先使用最近测速缓存，否则回落到 Mihomo history。
    pub fn state_for_proxy(
        &self,
        group: &str,
        proxy: &str,
        proxy_data: Option<&Proxy>,
        timeout: u32,
    ) -> TrayDelayState {
        let cached_state = self.cache.lock().get(&Self::cache_key(group, proxy)).copied();
        if let Some(state) = cached_state {
            return state;
        }

        proxy_data
            .and_then(|proxy| proxy.history.last())
            .map(|history| TrayDelayFormatter::state_from_delay(u32::from(history.delay), timeout))
            .unwrap_or(TrayDelayState::Unknown)
    }

    /// 对指定代理组执行延迟测试，并在每个节点完成后刷新托盘菜单。
    pub async fn test_group_delay(&self, group: String) {
        if !self.begin_group_test(&group) {
            logging!(debug, Type::Tray, "托盘代理组正在测速，跳过重复请求: {group}");
            return;
        }

        let result = self.run_group_delay_test(group.clone()).await;
        self.testing_groups.lock().remove(&group);

        if let Err(err) = result {
            logging!(error, Type::Tray, "托盘代理组测速失败: {group}, {err}");
        }
    }

    /// 在后台启动代理组延迟测试，避免菜单事件等待测速任务完成。
    pub fn spawn_group_delay_test(group: String) {
        AsyncHandler::spawn(move || async move {
            Self::global().test_group_delay(group).await;
        });
    }

    /// 标记代理组测速状态，防止重复点击并发触发同一组测速。
    fn begin_group_test(&self, group: &str) -> bool {
        self.testing_groups.lock().insert(group.to_string())
    }

    /// 执行代理组测速的主体流程。
    async fn run_group_delay_test(&self, group: String) -> Result<()> {
        let (names, test_url, timeout) = self.collect_group_test_input(&group).await?;
        if names.is_empty() {
            logging!(warn, Type::Tray, "托盘代理组没有可测速节点: {group}");
            return Ok(());
        }

        let menu_names = names.clone();
        self.mark_group_testing(&group, &menu_names);
        logging!(info, Type::Tray, "托盘代理组测速开始: {group}, 节点数: {}", names.len());
        self.refresh_group_menu(&group, &menu_names);

        let mut pending = stream::iter(names)
            .map(|name| {
                let group = group.clone();
                let test_url = test_url.clone();
                async move {
                    let state = Self::measure_proxy_delay(&name, &test_url, timeout).await;
                    (group, name, state)
                }
            })
            .buffer_unordered(TRAY_DELAY_TEST_CONCURRENCY);

        while let Some((group, name, state)) = pending.next().await {
            self.set_state(&group, &name, state);
            logging!(debug, Type::Tray, "托盘代理节点测速完成: {group}/{name}, {state:?}");
            self.refresh_group_menu(&group, &menu_names);
        }

        logging!(info, Type::Tray, "托盘代理组测速完成: {group}");
        Ok(())
    }

    /// 刷新当前代理组的菜单延迟状态，macOS 下原位更新菜单项以避免收起菜单。
    fn refresh_group_menu(&self, group: &str, names: &[String]) {
        #[cfg(target_os = "macos")]
        {
            Tray::global().refresh_proxy_group_latency_menu(group, self.group_state_snapshot(group, names));
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (group, names);
            AsyncHandler::spawn(|| async move {
                logging_error!(Type::Tray, Tray::global().update_menu().await);
            });
        }
    }

    /// 收集代理组测速所需的节点列表、测速 URL 和超时时间。
    async fn collect_group_test_input(&self, group: &str) -> Result<(Vec<String>, String, u32)> {
        let proxies = handle::Handle::mihomo().await.get_proxies().await?;
        let names = proxies
            .proxies
            .get(group)
            .and_then(|proxy| proxy.all.clone())
            .unwrap_or_default();

        let verge = Config::verge().await.latest_arc();
        let test_url = verge
            .default_latency_test
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(DEFAULT_LATENCY_TEST_URL)
            .to_string();
        let timeout = verge
            .default_latency_timeout
            .and_then(|timeout| u32::try_from(timeout).ok())
            .filter(|timeout| *timeout > 0)
            .unwrap_or(DEFAULT_LATENCY_TIMEOUT_MS);

        Ok((names, test_url, timeout))
    }

    /// 调用 Mihomo API 测试单个代理节点延迟。
    async fn measure_proxy_delay(name: &str, test_url: &str, timeout: u32) -> TrayDelayState {
        match handle::Handle::mihomo()
            .await
            .delay_proxy_by_name(name, test_url, timeout)
            .await
        {
            Ok(result) => TrayDelayFormatter::state_from_delay(result.delay, timeout),
            Err(err) => {
                logging!(error, Type::Tray, "托盘代理节点测速出错: {name}, {err}");
                TrayDelayState::Error
            }
        }
    }

    /// 将代理组下所有节点标记为测速中。
    fn mark_group_testing(&self, group: &str, names: &[String]) {
        let mut cache = self.cache.lock();
        for name in names {
            cache.insert(Self::cache_key(group, name), TrayDelayState::Testing);
        }
    }

    /// 写入单个节点的测速状态。
    fn set_state(&self, group: &str, name: &str, state: TrayDelayState) {
        self.cache.lock().insert(Self::cache_key(group, name), state);
    }

    /// 生成指定代理组的延迟状态快照，供原生菜单原位刷新使用。
    fn group_state_snapshot(&self, group: &str, names: &[String]) -> HashMap<String, TrayDelayState> {
        let cache = self.cache.lock();
        names
            .iter()
            .filter_map(|name| {
                cache
                    .get(&Self::cache_key(group, name))
                    .map(|state| (name.clone(), *state))
            })
            .collect()
    }

    /// 生成延迟缓存键。
    fn cache_key(group: &str, proxy: &str) -> String {
        format!("{group}::{proxy}")
    }
}

/// 托盘代理菜单构建器，负责创建代理组子菜单与节点菜单项。
pub struct TrayProxyMenuBuilder;

impl TrayProxyMenuBuilder {
    /// 创建代理组子菜单，并按运行时配置中的代理组顺序排序。
    pub fn create_proxy_submenus(
        app_handle: &AppHandle,
        proxy_mode: &str,
        proxy_group_order_map: Option<HashMap<String, usize>>,
        proxy_nodes_data: Option<Proxies>,
        latency_timeout: u32,
    ) -> Vec<Submenu<Wry>> {
        let mut submenus: Vec<(String, usize, Submenu<Wry>)> = Vec::new();

        if let Some(proxy_nodes_data) = proxy_nodes_data {
            for (group_name, group_data) in proxy_nodes_data.proxies.iter() {
                if let Some(submenu) = Self::create_group_submenu(
                    app_handle,
                    proxy_mode,
                    group_name,
                    group_data,
                    &proxy_nodes_data,
                    latency_timeout,
                ) {
                    let insertion_index = submenus.len();
                    submenus.push((group_name.to_string(), insertion_index, submenu));
                }
            }
        }

        Self::sort_submenus(&mut submenus, proxy_group_order_map.as_ref());
        submenus.into_iter().map(|(_, _, submenu)| submenu).collect()
    }

    /// 创建单个代理组子菜单，首项固定为延迟测试按钮。
    fn create_group_submenu(
        app_handle: &AppHandle,
        proxy_mode: &str,
        group_name: &str,
        group_data: &Proxy,
        proxy_nodes_data: &Proxies,
        latency_timeout: u32,
    ) -> Option<Submenu<Wry>> {
        if !Self::should_show_group(proxy_mode, group_name, group_data) {
            return None;
        }

        let all_proxies = group_data.all.as_ref()?;
        let now_proxy = group_data.now.as_deref().unwrap_or_default();
        let delay_test = MenuItem::with_id(
            app_handle,
            TrayProxyMenuId::delay_test(group_name),
            t!("tray.delayTest"),
            true,
            None::<&str>,
        )
        .ok()?;

        let group_items = Self::create_node_items(
            app_handle,
            group_name,
            now_proxy,
            all_proxies,
            proxy_nodes_data,
            latency_timeout,
        );
        if group_items.is_empty() {
            return None;
        }

        let mut group_item_refs: Vec<&dyn IsMenuItem<Wry>> = vec![&delay_test];
        group_item_refs.extend(group_items.iter().map(|item| item as &dyn IsMenuItem<Wry>));

        Submenu::with_id_and_items(
            app_handle,
            format!("proxy_group_{group_name}"),
            group_name,
            true,
            &group_item_refs,
        )
        .map_err(|err| {
            logging!(
                warn,
                Type::Tray,
                "Failed to create proxy group submenu: {group_name}, {err}"
            )
        })
        .ok()
    }

    /// 创建代理组下的节点菜单项。
    fn create_node_items(
        app_handle: &AppHandle,
        group_name: &str,
        now_proxy: &str,
        all_proxies: &[String],
        proxy_nodes_data: &Proxies,
        latency_timeout: u32,
    ) -> Vec<CheckMenuItem<Wry>> {
        all_proxies
            .iter()
            .filter_map(|proxy_name| {
                let state = TrayProxyLatencyController::global().state_for_proxy(
                    group_name,
                    proxy_name,
                    proxy_nodes_data.proxies.get(proxy_name),
                    latency_timeout,
                );
                let display = TrayDelayFormatter::display(state);
                let title = TrayDelayFormatter::node_title(proxy_name, &display).title;
                let item_id = TrayProxyMenuId::node(group_name, proxy_name);
                CheckMenuItem::with_id(app_handle, item_id, title, true, proxy_name == now_proxy, None::<&str>)
                    .map_err(|err| logging!(warn, Type::Tray, "Failed to create proxy menu item: {err}"))
                    .ok()
            })
            .collect()
    }

    /// 判断代理组在当前模式下是否应该显示。
    fn should_show_group(proxy_mode: &str, group_name: &str, group_data: &Proxy) -> bool {
        (match proxy_mode {
            "global" => group_name == "GLOBAL",
            _ => group_name != "GLOBAL",
        }) && !group_data.hidden.unwrap_or_default()
            && group_data.all.is_some()
    }

    /// 按配置顺序排序代理组子菜单。
    fn sort_submenus(
        submenus: &mut [(String, usize, Submenu<Wry>)],
        proxy_group_order_map: Option<&HashMap<String, usize>>,
    ) {
        if let Some(order_map) = proxy_group_order_map {
            submenus.sort_by(|(name_a, original_index_a, _), (name_b, original_index_b, _)| {
                match (order_map.get(name_a), order_map.get(name_b)) {
                    (Some(index_a), Some(index_b)) => index_a.cmp(index_b),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => original_index_a.cmp(original_index_b),
                }
            });
        }
    }

    /// 生成代理组菜单项 ID 顺序，供单元测试验证延迟测试按钮固定在首项。
    #[cfg(test)]
    fn group_item_ids_for_test(group_name: &str, all_proxies: &[String]) -> Vec<String> {
        std::iter::once(TrayProxyMenuId::delay_test(group_name))
            .chain(all_proxies.iter().map(|proxy| TrayProxyMenuId::node(group_name, proxy)))
            .collect()
    }
}

/// 从配置中读取有效的托盘延迟超时时间。
pub fn resolve_latency_timeout_from_config(default_latency_timeout: Option<i16>) -> u32 {
    default_latency_timeout
        .and_then(|timeout| u32::try_from(timeout).ok())
        .filter(|timeout| *timeout > 0)
        .unwrap_or(DEFAULT_LATENCY_TIMEOUT_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_id_roundtrips_special_names() {
        let id = TrayProxyMenuId::node("自动_选择:组", "节点_1/香港");
        assert_eq!(
            TrayProxyMenuId::parse(&id),
            Some(TrayProxyMenuAction::SelectNode {
                group: "自动_选择:组".into(),
                proxy: "节点_1/香港".into(),
            })
        );
    }

    #[test]
    fn delay_test_id_roundtrips_group_name() {
        let id = TrayProxyMenuId::delay_test("节点选择_A:B");
        assert_eq!(
            TrayProxyMenuId::parse(&id),
            Some(TrayProxyMenuAction::TestGroupDelay {
                group: "节点选择_A:B".into(),
            })
        );
    }

    #[test]
    fn delay_state_maps_text_and_color() {
        assert_eq!(
            TrayDelayFormatter::display(TrayDelayState::Value(80)).color,
            TrayDelayColor::Success
        );
        assert_eq!(
            TrayDelayFormatter::display(TrayDelayState::Value(260)).color,
            TrayDelayColor::Primary
        );
        assert_eq!(
            TrayDelayFormatter::display(TrayDelayState::Value(450)).color,
            TrayDelayColor::Warning
        );
        assert_eq!(
            TrayDelayFormatter::display(TrayDelayState::Timeout).color,
            TrayDelayColor::Error
        );
    }

    #[test]
    fn delay_state_respects_timeout_and_error() {
        assert_eq!(TrayDelayFormatter::state_from_delay(0, 10_000), TrayDelayState::Timeout);
        assert_eq!(
            TrayDelayFormatter::state_from_delay(10_000, 10_000),
            TrayDelayState::Timeout
        );
        assert_eq!(
            TrayDelayFormatter::state_from_delay(100_001, 10_000),
            TrayDelayState::Error
        );
    }

    #[test]
    fn node_title_uses_tab_and_delay_range() {
        let display = TrayDelayDisplay {
            text: "123ms".into(),
            color: TrayDelayColor::Success,
        };
        let parts = TrayDelayFormatter::node_title("HK Node", &display);
        assert_eq!(parts.title, "HK Node\t123ms");
        assert_eq!(parts.delay_start, "HK Node\t".encode_utf16().count());
        assert_eq!(parts.delay_len, "123ms".encode_utf16().count());
    }

    #[test]
    fn node_title_delay_range_uses_utf16_for_unicode_title() {
        let display = TrayDelayDisplay {
            text: "测试中".into(),
            color: TrayDelayColor::Primary,
        };
        let parts = TrayDelayFormatter::node_title("香港_节点", &display);
        assert_eq!(parts.title, "香港_节点\t测试中");
        assert_eq!(parts.delay_start, "香港_节点\t".encode_utf16().count());
        assert_eq!(parts.delay_len, "测试中".encode_utf16().count());
    }

    #[test]
    fn proxy_group_item_ids_put_delay_test_first() {
        let ids = TrayProxyMenuBuilder::group_item_ids_for_test("自动_选择:组", &["节点_1".into(), "节点:2".into()]);
        assert_eq!(ids.first(), Some(&TrayProxyMenuId::delay_test("自动_选择:组")));
        assert_eq!(
            TrayProxyMenuId::parse(&ids[1]),
            Some(TrayProxyMenuAction::SelectNode {
                group: "自动_选择:组".into(),
                proxy: "节点_1".into(),
            })
        );
    }

    #[test]
    fn latency_controller_refreshes_testing_done_and_error_states() {
        let controller = TrayProxyLatencyController::default();
        let names = vec!["节点_A".to_string()];

        controller.mark_group_testing("自动选择", &names);
        assert_eq!(
            controller.state_for_proxy("自动选择", "节点_A", None, DEFAULT_LATENCY_TIMEOUT_MS),
            TrayDelayState::Testing
        );

        controller.set_state("自动选择", "节点_A", TrayDelayState::Value(88));
        assert_eq!(
            controller.state_for_proxy("自动选择", "节点_A", None, DEFAULT_LATENCY_TIMEOUT_MS),
            TrayDelayState::Value(88)
        );

        controller.set_state("自动选择", "节点_A", TrayDelayState::Error);
        assert_eq!(
            controller.state_for_proxy("自动选择", "节点_A", None, DEFAULT_LATENCY_TIMEOUT_MS),
            TrayDelayState::Error
        );
    }
}
