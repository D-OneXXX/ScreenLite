// Windows 发布版不弹控制台窗口；调试版保留输出便于排错。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    screenlite_lib::run()
}
