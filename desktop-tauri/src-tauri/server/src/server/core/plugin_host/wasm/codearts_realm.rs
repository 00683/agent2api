//! Fixed profiles from HuaweiCloud.vscode-codebot 26.10.300 product.json.
//! HK inference and portal are regional; OAuth STS remains shared in cn-north-4.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Realm {
    pub(crate) provider: &'static str,
    pub(crate) origin: &'static str,
    pub(crate) authority: &'static str,
    pub(crate) portal: &'static str,
    pub(crate) version: &'static str,
    pub(crate) locale: &'static str,
}
impl Realm {
    pub(crate) fn for_provider(provider: &str) -> Option<Self> {
        match provider {
            "codearts" => Some(Self { provider: "codearts",
                origin: "https://snap-access.cn-north-4.myhuaweicloud.com",
                authority: "snap-access.cn-north-4.myhuaweicloud.com",
                portal: "https://codearts.huaweicloud.com/portal", version: "26.9.101", locale: "zh-cn" }),
            "codearts-intl" => Some(Self { provider: "codearts-intl",
                origin: "https://snap-access.ap-southeast-1.myhuaweicloud.com",
                authority: "snap-access.ap-southeast-1.myhuaweicloud.com",
                portal: "https://codearts.ap-southeast-1.huaweicloud.com/portal", version: "26.10.300", locale: "en-us" }),
            _ => None,
        }
    }
    pub(crate) fn is_chat_url(self, raw: &str) -> bool {
        url::Url::parse(raw).ok().is_some_and(|url| url.origin().ascii_serialization() == self.origin
            && url.path() == "/api/v2/chat/completions" && url.username().is_empty() && url.password().is_none())
    }
}
#[cfg(test)]
mod tests {
    use super::Realm;
    #[test]
    fn regional_profiles_keep_login_and_chat_in_the_selected_realm() {
        let cn = Realm::for_provider("codearts").unwrap();
        let hk = Realm::for_provider("codearts-intl").unwrap();
        assert_ne!(cn.provider, hk.provider);
        assert!(hk.portal.contains("ap-southeast-1"));
        assert!(hk.is_chat_url(&format!("{}/api/v2/chat/completions", hk.origin)));
        assert!(!hk.is_chat_url(&format!("{}/api/v2/chat/completions", cn.origin)));
        assert!(!hk.is_chat_url(&format!("{}/api/v2/chat/completions/other", hk.origin)));
        assert!(!hk.is_chat_url("https://snap-access.ap-southeast-1.myhuaweicloud.com.evil.invalid/api/v2/chat/completions"));
        assert!(Realm::for_provider("codearts-other").is_none());
    }
}
