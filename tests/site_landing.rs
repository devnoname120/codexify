#[test]
fn install_tabs_auto_select_windows_and_keep_manual_switching() {
    let html = include_str!("../site/index.html");

    assert!(html.contains("navigator.userAgentData?.platform"));
    assert!(html.contains("navigator.platform"));
    assert!(html.contains("navigator.userAgent"));
    assert!(html.contains("installTabForPlatform"));
    assert!(html.contains("activateInstallTab(initialInstallTab)"));
    assert!(html.contains("activateInstallTab(tab.dataset.tab)"));
}
