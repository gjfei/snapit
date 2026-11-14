//! 界面语言的解析与切换。

use snapit_store::Config;

/// 设置项可选值；`auto` 跟随系统。
pub const LANGUAGE_VALUES: [&str; 3] = ["auto", "zh-CN", "en"];

/// 把设置值解析成可用的 locale：未知值等同 `auto`。
pub fn resolve(setting: &str) -> &'static str {
    match setting {
        "zh-CN" => "zh-CN",
        "en" => "en",
        _ => match sys_locale::get_locale() {
            Some(locale) if locale.to_ascii_lowercase().starts_with("zh") => "zh-CN",
            _ => "en",
        },
    }
}

pub fn apply(config: &Config) {
    let locale = resolve(&config.general.language);
    rust_i18n::set_locale(locale);
    log::info!("ui locale: {locale}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_setting_wins_and_unknown_follows_system() {
        assert_eq!(resolve("zh-CN"), "zh-CN");
        assert_eq!(resolve("en"), "en");
        assert!(matches!(resolve("auto"), "en" | "zh-CN"));
        assert!(matches!(resolve("fr"), "en" | "zh-CN"));
    }

    #[test]
    fn both_locales_have_the_interface_strings() {
        assert_eq!(t!("action.screenshot", locale = "zh-CN").as_ref(), "截图");
        assert_eq!(
            t!("action.screenshot", locale = "en").as_ref(),
            "Screenshot"
        );
        assert_eq!(t!("overlay.tool.line", locale = "zh-CN").as_ref(), "直线");
        assert_eq!(t!("overlay.tool.line", locale = "en").as_ref(), "Line");
    }
}
