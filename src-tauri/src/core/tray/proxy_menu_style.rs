//! macOS 托盘代理菜单富文本样式模块
//!
//! 通过 AppKit 的 `NSMenuItem.attributedTitle` 给代理节点延迟片段单独上色，
//! 并使用右对齐制表位将延迟列固定在菜单右侧，解决节点名和延迟列混排不齐的问题。

use crate::core::tray::proxy_menu::{TrayDelayColor, TrayDelayFormatter, TrayDelayState};
use crate::process::AsyncHandler;
use crate::{Type, logging};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{ClassType as _, DefinedClass as _, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSButton, NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu, NSMenuItem,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab, NSTextTabType,
};
use objc2_foundation::{NSArray, NSDictionary, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSSize, NSString};
use std::collections::HashMap;
use tauri::AppHandle;

const DELAY_TAB_LOCATION: f64 = 260.0;
const MENU_FONT_SIZE: f64 = 14.0;
const DELAY_TEST_BUTTON_WIDTH: f64 = 260.0;
const DELAY_TEST_BUTTON_HEIGHT: f64 = 26.0;

#[derive(Debug)]
struct DelayTestButtonIvars {
    group: Retained<NSString>,
}

define_class!(
    #[unsafe(super = NSButton)]
    #[name = "TrayDelayTestButton"]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelayTestButtonIvars]
    struct DelayTestButton;

    unsafe impl NSObjectProtocol for DelayTestButton {}

    impl DelayTestButton {
        #[unsafe(method(runDelayTest:))]
        fn run_delay_test(&self, _sender: Option<&AnyObject>) {
            let group = self.ivars().group.to_string();
            logging!(debug, Type::Tray, "托盘延迟测试按钮触发: {group}");
            AsyncHandler::spawn(move || async move {
                crate::core::tray::proxy_menu::TrayProxyLatencyController::global()
                    .test_group_delay(group)
                    .await;
            });
        }
    }
);

impl DelayTestButton {
    /// 创建延迟测试按钮，按钮自身保存代理组名并作为 action target 触发后台测速。
    fn new(mtm: MainThreadMarker, group: &str) -> Retained<Self> {
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(DELAY_TEST_BUTTON_WIDTH, DELAY_TEST_BUTTON_HEIGHT),
        );
        let this = Self::alloc(mtm).set_ivars(DelayTestButtonIvars {
            group: NSString::from_str(group),
        });
        let button: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let title = clash_verge_i18n::t!("tray.delayTest");
        let ns_title = NSString::from_str(&title);

        button.setTitle(&ns_title);
        button.setBordered(false);
        button.setTransparent(false);
        button.as_super().setFont(Some(&NSFont::menuFontOfSize(MENU_FONT_SIZE)));
        button.as_super().setAlignment(NSTextAlignment::Left);
        unsafe {
            button.as_super().setTarget(Some(button.as_super().as_super()));
            button.as_super().setAction(Some(sel!(runDelayTest:)));
        }

        button
    }
}

/// macOS 代理菜单富文本样式应用器。
pub struct TrayProxyMenuStyler;

impl TrayProxyMenuStyler {
    /// 遍历当前托盘菜单，并为代理节点延迟片段应用富文本样式。
    pub fn apply(app_handle: &AppHandle) {
        let Some(tray) = app_handle.tray_by_id("main") else {
            return;
        };

        let result = tray.with_inner_tray_icon(|inner| {
            if let Some(status_item) = inner.ns_status_item()
                && let Some(mtm) = MainThreadMarker::new()
                && let Some(menu) = status_item.menu(mtm)
            {
                Self::style_menu(&menu, mtm);
            }
        });

        if let Err(err) = result {
            logging!(warn, Type::Tray, "应用托盘代理菜单富文本失败: {err}");
        }
    }

    /// 递归遍历 NSMenu 菜单项并处理代理节点项。
    fn style_menu(menu: &NSMenu, mtm: MainThreadMarker) {
        let items = menu.itemArray();
        for index in 0..items.count() {
            let item = items.objectAtIndex(index);
            let title = item.title().to_string();
            if title == clash_verge_i18n::t!("tray.delayTest")
                && let Some(group) = Self::group_name_for_delay_item(&item)
            {
                Self::install_delay_test_button(&item, &group, mtm);
                continue;
            }
            Self::style_item(&item);
            if item.hasSubmenu()
                && let Some(submenu) = item.submenu()
            {
                Self::style_menu(&submenu, mtm);
            }
        }
    }

    /// 为延迟测试菜单项安装原生按钮视图，点击按钮时不触发普通菜单项关闭行为。
    fn install_delay_test_button(item: &NSMenuItem, group: &str, mtm: MainThreadMarker) {
        if item.view().is_some() {
            return;
        }

        let button = Self::create_delay_test_button(group, mtm);
        item.setView(Some(button.as_super()));
    }

    /// 创建延迟测试按钮及其 action target，按钮 target 由按钮强引用保持生命周期。
    fn create_delay_test_button(group: &str, mtm: MainThreadMarker) -> Retained<NSButton> {
        DelayTestButton::new(mtm, group).into_super()
    }

    /// 从延迟测试菜单项所属子菜单恢复代理组名称。
    fn group_name_for_delay_item(item: &NSMenuItem) -> Option<String> {
        unsafe { item.menu() }.map(|menu| menu.title().to_string())
    }

    /// 对单个代理节点菜单项应用延迟文字富文本。
    fn style_item(item: &NSMenuItem) {
        let title = item.title().to_string();
        let Some((proxy, delay_text)) = title.rsplit_once('\t') else {
            return;
        };
        let display = TrayDelayFormatter::display(Self::state_from_delay_text(delay_text));
        let parts = TrayDelayFormatter::node_title(proxy, &display);
        let attributed_title =
            Self::build_attributed_title(&parts.title, parts.delay_start, parts.delay_len, display.color);
        item.setAttributedTitle(Some(&attributed_title));
    }

    /// 原位刷新指定代理组下的节点延迟标题，避免替换 NSMenu 导致菜单收起。
    pub fn refresh_group(app_handle: &AppHandle, group: &str, states: HashMap<String, TrayDelayState>) {
        let Some(tray) = app_handle.tray_by_id("main") else {
            return;
        };

        let group_name = group.to_string();
        let result = tray.with_inner_tray_icon(move |inner| {
            if let Some(status_item) = inner.ns_status_item()
                && let Some(mtm) = MainThreadMarker::new()
                && let Some(menu) = status_item.menu(mtm)
                && let Some(group_menu) = Self::find_group_menu(&menu, &group_name)
            {
                Self::refresh_group_menu_items(&group_menu, &states);
                group_menu.update();
                menu.update();
            }
        });

        if let Err(err) = result {
            logging!(warn, Type::Tray, "刷新托盘代理组延迟菜单失败: {group}, {err}");
        }
    }

    /// 在当前菜单树中查找指定代理组对应的子菜单。
    fn find_group_menu(menu: &NSMenu, group: &str) -> Option<Retained<NSMenu>> {
        let items = menu.itemArray();
        for index in 0..items.count() {
            let item = items.objectAtIndex(index);
            if Self::is_group_item(&item.title().to_string(), group)
                && item.hasSubmenu()
                && let Some(submenu) = item.submenu()
            {
                return Some(submenu);
            }
            if item.hasSubmenu()
                && let Some(submenu) = item.submenu()
                && let Some(found) = Self::find_group_menu(&submenu, group)
            {
                return Some(found);
            }
        }
        None
    }

    /// 刷新代理组子菜单中的节点菜单项标题与富文本颜色。
    fn refresh_group_menu_items(group_menu: &NSMenu, states: &HashMap<String, TrayDelayState>) {
        let items = group_menu.itemArray();
        for index in 0..items.count() {
            let item = items.objectAtIndex(index);
            let title = item.title().to_string();
            let Some(proxy_name) = Self::proxy_name_from_title(&title) else {
                continue;
            };
            let Some(state) = states.get(proxy_name) else {
                continue;
            };
            Self::set_proxy_item_title(&item, proxy_name, *state);
        }
    }

    /// 根据节点名与延迟状态设置菜单项标题，节点名保留系统默认颜色。
    fn set_proxy_item_title(item: &NSMenuItem, proxy_name: &str, state: TrayDelayState) {
        let display = TrayDelayFormatter::display(state);
        let parts = TrayDelayFormatter::node_title(proxy_name, &display);
        let ns_title = NSString::from_str(&parts.title);
        let attributed_title =
            Self::build_attributed_title(&parts.title, parts.delay_start, parts.delay_len, display.color);
        item.setTitle(&ns_title);
        item.setAttributedTitle(Some(&attributed_title));
    }

    /// 判断当前菜单项标题是否匹配指定代理组。
    fn is_group_item(title: &str, group: &str) -> bool {
        title == group
    }

    /// 从节点菜单标题中取出节点名，忽略延迟测试等非节点菜单项。
    fn proxy_name_from_title(title: &str) -> Option<&str> {
        title.rsplit_once('\t').map(|(proxy_name, _)| proxy_name)
    }

    /// 根据菜单中的延迟文本恢复富文本颜色等级。
    fn state_from_delay_text(delay_text: &str) -> TrayDelayState {
        match delay_text.trim_end_matches("ms").parse::<u32>() {
            Ok(delay) => TrayDelayState::Value(delay),
            Err(_) if delay_text == "-" => TrayDelayState::Unknown,
            Err(_) if delay_text == clash_verge_i18n::t!("tray.delayStatus.testing") => TrayDelayState::Testing,
            Err(_) if delay_text == clash_verge_i18n::t!("tray.delayStatus.timeout") => TrayDelayState::Timeout,
            Err(_) => TrayDelayState::Error,
        }
    }

    /// 构建包含右对齐制表位和延迟片段颜色的 NSAttributedString。
    pub(crate) fn build_attributed_title(
        title: &str,
        delay_start: usize,
        delay_len: usize,
        color: TrayDelayColor,
    ) -> Retained<NSMutableAttributedString> {
        unsafe {
            let ns_title = NSString::from_str(title);
            let attr_string = NSMutableAttributedString::initWithString_attributes(
                <NSMutableAttributedString as objc2::AnyThread>::alloc(),
                &ns_title,
                None,
            );

            let full_range = NSRange::new(0, title.encode_utf16().count());
            attr_string.addAttributes_range(&Self::base_attrs(), full_range);

            if color != TrayDelayColor::Default {
                let delay_range = NSRange::new(delay_start, delay_len);
                attr_string.addAttributes_range(&Self::delay_color_attrs(color), delay_range);
            }

            attr_string
        }
    }

    /// 构造菜单标题基础富文本属性。
    fn base_attrs() -> Retained<NSDictionary<NSString, AnyObject>> {
        unsafe {
            let font = NSFont::menuFontOfSize(MENU_FONT_SIZE);
            let paragraph = NSMutableParagraphStyle::new();
            paragraph.setAlignment(NSTextAlignment::Left);
            let tab = NSTextTab::initWithType_location(
                <NSTextTab as objc2::AnyThread>::alloc(),
                NSTextTabType::RightTabStopType,
                DELAY_TAB_LOCATION,
            );
            let tab_stops = NSArray::from_retained_slice(&[tab]);
            paragraph.setTabStops(Some(&tab_stops));

            let keys: &[&NSString] = &[NSFontAttributeName, NSParagraphStyleAttributeName];
            let values: &[&AnyObject] = &[&font, &paragraph];
            NSDictionary::from_slices(keys, values)
        }
    }

    /// 构造延迟片段颜色属性。
    fn delay_color_attrs(color: TrayDelayColor) -> Retained<NSDictionary<NSString, AnyObject>> {
        unsafe {
            let ns_color = match color {
                TrayDelayColor::Success => NSColor::systemGreenColor(),
                TrayDelayColor::Primary => NSColor::systemBlueColor(),
                TrayDelayColor::Warning => NSColor::systemOrangeColor(),
                TrayDelayColor::Error => NSColor::systemRedColor(),
                TrayDelayColor::Default => NSColor::labelColor(),
            };
            let keys: &[&NSString] = &[NSForegroundColorAttributeName];
            let values: &[&AnyObject] = &[&ns_color];
            NSDictionary::from_slices(keys, values)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tray::proxy_menu::TrayDelayDisplay;

    #[test]
    fn title_parts_keep_delay_after_tab() {
        let parts = TrayDelayFormatter::node_title(
            "节点 A",
            &TrayDelayDisplay {
                text: "88ms".into(),
                color: TrayDelayColor::Success,
            },
        );
        assert!(parts.title.contains('\t'));
        assert_eq!(parts.delay_start, "节点 A\t".encode_utf16().count());
        assert_eq!(parts.delay_len, "88ms".encode_utf16().count());
    }

    #[test]
    fn proxy_name_from_title_only_accepts_delay_title() {
        assert_eq!(
            TrayProxyMenuStyler::proxy_name_from_title("香港 A\t88ms"),
            Some("香港 A")
        );
        assert_eq!(TrayProxyMenuStyler::proxy_name_from_title("延迟测试"), None);
    }

    #[test]
    fn group_item_matches_exact_title() {
        assert!(TrayProxyMenuStyler::is_group_item("自动选择", "自动选择"));
        assert!(!TrayProxyMenuStyler::is_group_item("自动选择 A", "自动选择"));
    }
}
