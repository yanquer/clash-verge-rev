//! macOS 托盘代理菜单富文本样式模块
//!
//! 通过 AppKit 的 `NSMenuItem.attributedTitle` 给代理节点延迟片段单独上色，
//! 并使用右对齐制表位将延迟列固定在菜单右侧，解决节点名和延迟列混排不齐的问题。

use crate::core::tray::proxy_menu::{TrayDelayColor, TrayDelayFormatter};
use crate::{Type, logging};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu, NSMenuItem, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab, NSTextTabType,
};
use objc2_foundation::{NSArray, NSDictionary, NSMutableAttributedString, NSRange, NSString};
use tauri::AppHandle;

const DELAY_TAB_LOCATION: f64 = 260.0;
const MENU_FONT_SIZE: f64 = 14.0;

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
                Self::style_menu(&menu);
            }
        });

        if let Err(err) = result {
            logging!(warn, Type::Tray, "应用托盘代理菜单富文本失败: {err}");
        }
    }

    /// 递归遍历 NSMenu 菜单项并处理代理节点项。
    fn style_menu(menu: &NSMenu) {
        let items = menu.itemArray();
        for index in 0..items.count() {
            let item = items.objectAtIndex(index);
            Self::style_item(&item);
            if item.hasSubmenu()
                && let Some(submenu) = item.submenu()
            {
                Self::style_menu(&submenu);
            }
        }
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

    /// 根据菜单中的延迟文本恢复富文本颜色等级。
    fn state_from_delay_text(delay_text: &str) -> crate::core::tray::proxy_menu::TrayDelayState {
        match delay_text.trim_end_matches("ms").parse::<u32>() {
            Ok(delay) => crate::core::tray::proxy_menu::TrayDelayState::Value(delay),
            Err(_) if delay_text == "-" => crate::core::tray::proxy_menu::TrayDelayState::Unknown,
            Err(_) if delay_text == clash_verge_i18n::t!("tray.delayStatus.testing") => {
                crate::core::tray::proxy_menu::TrayDelayState::Testing
            }
            Err(_) if delay_text == clash_verge_i18n::t!("tray.delayStatus.timeout") => {
                crate::core::tray::proxy_menu::TrayDelayState::Timeout
            }
            Err(_) => crate::core::tray::proxy_menu::TrayDelayState::Error,
        }
    }

    /// 构建包含右对齐制表位和延迟片段颜色的 NSAttributedString。
    fn build_attributed_title(
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
}
